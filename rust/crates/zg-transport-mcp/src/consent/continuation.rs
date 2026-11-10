use super::{Decision, EngineError, RequestContext, RoleServer, authorization};
use sha2::Digest;

// Opaque continuation tokens reference server-owned request, caller and destination bindings.
// A bounded nonce registry makes redemption atomic even across concurrent sessions.
type PendingStates = std::collections::HashMap<String, (std::time::Instant, Vec<u8>)>;

#[derive(Default)]
pub(crate) struct ContinuationState {
    pending: std::sync::Mutex<PendingStates>,
}

#[derive(Clone)]
pub(super) struct ContinuationApproval {
    pub(super) targets: Vec<authorization::QueryAuthorization>,
    pub(super) decision: Decision,
}

impl ContinuationState {
    pub(crate) fn prepare(
        &self,
        request: &rmcp::model::CallToolRequestParams,
        context: &mut RequestContext<RoleServer>,
    ) -> Result<Option<rmcp::model::CallToolResponse>, rmcp::ErrorData> {
        use rmcp::model::ProtocolVersion;
        let invalid = || rmcp::ErrorData::invalid_params("Invalid or expired requestState", None);
        if context.ct.is_cancelled() {
            return Ok(Some(
                crate::error_result(&EngineError::cancelled("MCP request was cancelled")).into(),
            ));
        }
        let continuation = request.request_state.is_some() || request.input_responses.is_some();
        if !continuation
            && context
                .protocol_version()
                .is_none_or(|v| v < ProtocolVersion::V_2026_07_28)
        {
            return Ok(None);
        }
        let arguments = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
        let parse_error =
            |error: serde_json::Error| rmcp::ErrorData::invalid_params(error.to_string(), None);
        let mapping_error = |message| rmcp::ErrorData::invalid_params(message, None);
        let (targets, allow_fts) = match request.name.as_ref() {
            "zvec_grep_search" => {
                let input: crate::SearchInput =
                    serde_json::from_value(arguments.clone()).map_err(parse_error)?;
                let options = input.into_request().map_err(mapping_error)?;
                (
                    match authorization::query_authorizations(&options) {
                        Ok(targets) => targets,
                        Err(error) => return Ok(Some(crate::error_result(&error).into())),
                    },
                    true,
                )
            }
            "zvec_grep_index" => {
                let input: crate::IndexInput =
                    serde_json::from_value(arguments.clone()).map_err(parse_error)?;
                match input.into_request().map_err(mapping_error)? {
                    crate::IndexToolRequest::Index { options, .. } => (
                        match authorization::index_authorizations(&options) {
                            Ok(targets) => targets,
                            Err(error) => return Ok(Some(crate::error_result(&error).into())),
                        }
                        .into_iter()
                        .map(|target| authorization::QueryAuthorization {
                            target,
                            query_text: false,
                            workspace_content: true,
                        })
                        .collect(),
                        false,
                    ),
                    crate::IndexToolRequest::Drop(_) if !continuation => return Ok(None),
                    crate::IndexToolRequest::Drop(_) => return Err(invalid()),
                }
            }
            _ if !continuation => return Ok(None),
            _ => return Err(invalid()),
        };
        let binding = request_binding(request, &arguments, &targets, context)?;
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(600);
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| rmcp::ErrorData::internal_error("Consent state unavailable", None))?;
        pending.retain(|_, (expires, _)| *expires > now);
        if continuation {
            let state = request.request_state.as_deref().ok_or_else(invalid)?;
            if pending
                .get(state)
                .is_none_or(|(_, expected)| expected != &binding)
            {
                return Err(invalid());
            }
            pending.remove(state);
            let response = request
                .input_responses
                .as_ref()
                .and_then(|responses| responses.get("remote_embedding_authorization"))
                .ok_or_else(invalid)?;
            let decision = match response_decision(response, allow_fts) {
                Ok(decision) => decision,
                Err(error) => return Ok(Some(crate::error_result(&error).into())),
            };
            context
                .extensions
                .insert(ContinuationApproval { targets, decision });
            return Ok(None);
        }
        if targets.is_empty() {
            return Ok(None);
        }
        let state = issue(&mut pending, now + ttl, binding)?;
        prompt(&targets, allow_fts, state).map(Some)
    }
}

fn request_binding(
    request: &rmcp::model::CallToolRequestParams,
    arguments: &serde_json::Value,
    targets: &[authorization::QueryAuthorization],
    context: &RequestContext<RoleServer>,
) -> Result<Vec<u8>, rmcp::ErrorData> {
    // The daemon has one optional Bearer principal. Anonymous loopback callers
    // share a principal, matching the Node transport's authorization boundary.
    let principal = context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get(http::header::AUTHORIZATION))
        .map_or(&[][..], |value| value.as_bytes());
    let mut binding = serde_json::json!([request.name, arguments, targets, principal]);
    // Object key order is not part of the request identity; array order and values are.
    binding.sort_all_objects();
    let data = serde_json::to_vec(&binding)
        .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
    Ok(sha2::Sha256::digest(data).to_vec())
}

fn issue(
    pending: &mut PendingStates,
    expires: std::time::Instant,
    binding: Vec<u8>,
) -> Result<String, rmcp::ErrorData> {
    if pending.len() >= 4096 {
        return Err(rmcp::ErrorData::internal_error(
            "Too many pending consent requests; retry later",
            None,
        ));
    }
    let state = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    pending.insert(state.clone(), (expires, binding));
    Ok(state)
}

fn response_decision(
    response: &serde_json::Value,
    allow_fts: bool,
) -> Result<Decision, EngineError> {
    match (
        response.get("action").and_then(serde_json::Value::as_str),
        response
            .pointer("/content/decision")
            .and_then(serde_json::Value::as_str),
    ) {
        (Some("accept"), Some("allow_once")) => Ok(Decision::Once),
        (Some("accept"), Some("allow_workspace")) => Ok(Decision::Workspace),
        (Some("accept"), Some("use_local_search")) if allow_fts => Ok(Decision::FtsOnly),
        _ => Err(EngineError::permission_denied(
            "Remote embedding consent was declined; no remote data was sent",
        )),
    }
}

fn prompt(
    targets: &[authorization::QueryAuthorization],
    allow_fts: bool,
    state: String,
) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
    use rmcp::model::InputRequiredResult;
    use serde_json::json;
    let parse_error =
        |error: serde_json::Error| rmcp::ErrorData::internal_error(error.to_string(), None);
    let mut choices = vec![
        json!({"const": "allow_once", "title": "Allow once"}),
        json!({"const": "allow_workspace", "title": "Allow for this workspace"}),
    ];
    if allow_fts {
        choices.push(json!({"const": "use_local_search", "title": "Use FTS only"}));
    }
    choices.push(json!({"const": "cancel", "title": "Cancel"}));
    // Keep canonical authorization targets intact; simplify only the prompt's copy.
    let mut display_targets = targets.to_vec();
    for required in &mut display_targets {
        required.target.root = zg_cli::output_path(&required.target.root).to_path_buf();
        for root in &mut required.target.workspace_roots {
            *root = zg_cli::output_path(root).to_path_buf();
        }
    }
    let requests = serde_json::from_value(json!({"remote_embedding_authorization": {
            "method": "elicitation/create", "params": {
                "mode": "form",
                "message": format!("Allow remote embedding? Disclosed workspace roots, models, endpoints and data categories: {}. Only explicit acceptance permits transmission. Do not enter credentials.", serde_json::to_string(&display_targets).map_err(parse_error)?),
                "requestedSchema": {"type": "object", "properties": {"decision": {
                    "type": "string", "title": "Remote Embedding permission", "oneOf": choices, "default": "cancel"
                }}, "required": ["decision"]}
            }
        }})).map_err(parse_error)?;
    Ok(InputRequiredResult::new(Some(requests), Some(state)).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{ServerHandler, ServiceExt, model::*};
    use std::sync::Arc;

    fn disclosed_targets(required: &InputRequiredResult) -> serde_json::Value {
        let wire = serde_json::to_value(required).expect("input-required response");
        let message = wire["inputRequests"]["remote_embedding_authorization"]["params"]["message"]
            .as_str()
            .expect("consent message");
        let targets = message
            .strip_prefix("Allow remote embedding? Disclosed workspace roots, models, endpoints and data categories: ")
            .and_then(|message| message.strip_suffix(". Only explicit acceptance permits transmission. Do not enter credentials."))
            .expect("disclosed targets in consent message");
        serde_json::from_str(targets).expect("disclosed target JSON")
    }

    #[test]
    fn modern_consent_prompt_formats_only_display_paths() {
        let long_path = format!(r"\\?\C:\{}", "a".repeat(270));
        for (input, expected) in [
            (r"\\?\C:\workspace".to_owned(), r"C:\workspace".to_owned()),
            // The shared display helper preserves verbatim UNC paths.
            (
                r"\\?\UNC\server\share\workspace".to_owned(),
                r"\\?\UNC\server\share\workspace".to_owned(),
            ),
            (
                r"\\?\C:\workspace.".to_owned(),
                r"\\?\C:\workspace.".to_owned(),
            ),
            (long_path.clone(), long_path),
        ] {
            let expected = if cfg!(windows) { &expected } else { &input };
            let source = r"\\?\C:\source";
            let expected_source = if cfg!(windows) { r"C:\source" } else { source };
            for allow_fts in [false, true] {
                let targets = vec![authorization::QueryAuthorization {
                    target: authorization::IndexAuthorization {
                        root: input.clone().into(),
                        workspace_roots: vec![source.into(), input.clone().into()],
                        model: "remote/test-model".into(),
                        endpoint: "https://embedding.example.test/v1".into(),
                        endpoint_host: "embedding.example.test".into(),
                    },
                    query_text: allow_fts,
                    workspace_content: true,
                }];
                let original = targets.clone();
                let CallToolResponse::InputRequired(required) =
                    prompt(&targets, allow_fts, "opaque-state".into()).expect("consent prompt")
                else {
                    panic!("consent must require input");
                };
                assert_eq!(
                    disclosed_targets(&required),
                    serde_json::json!([{
                        "target": {
                            "root": expected,
                            "workspace_roots": [expected_source, expected],
                            "model": "remote/test-model",
                            "endpoint": "https://embedding.example.test/v1",
                            "endpoint_host": "embedding.example.test",
                        },
                        "query_text": allow_fts,
                        "workspace_content": true,
                    }]),
                );
                assert_eq!(required.request_state.as_deref(), Some("opaque-state"));
                assert_eq!(targets, original, "authorization targets must not change");
            }
        }
    }

    struct Server(Arc<ContinuationState>);
    impl ServerHandler for Server {
        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            mut context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            if let Some(principal) = serde_json::to_value(&context.meta)
                .expect("metadata")
                .get("testPrincipal")
                .and_then(serde_json::Value::as_str)
            {
                let (parts, ()) = http::Request::builder()
                    .header("authorization", principal)
                    .body(())
                    .expect("request")
                    .into_parts();
                context.extensions.insert(parts);
            }
            if let Some(required) = self.0.prepare(&request, &mut context)? {
                return Ok(required);
            }
            let input: crate::IndexInput = serde_json::from_value(serde_json::Value::Object(
                request.arguments.expect("valid consent test fixture"),
            ))
            .expect("valid consent test fixture");
            let crate::IndexToolRequest::Index { mut options, .. } =
                input.into_request().expect("valid consent test fixture")
            else {
                panic!("index");
            };
            super::super::index(&mut options, &context)
                .await
                .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
            Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string(&options.authorized_remote)
                    .expect("valid consent test fixture"),
            )])
            .into())
        }
    }

    #[tokio::test]
    async fn wire_continuation_accepts_object_reordering_but_rejects_value_and_array_changes() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let root = tempfile::tempdir().expect("workspace");
            let (server_io, client_io) = tokio::io::duplex(65536);
            let task = tokio::spawn(async move {
                Server(Arc::new(ContinuationState::default()))
                    .serve(server_io)
                    .await
                    .expect("server")
                    .waiting()
                    .await
            });
            let mut info = ClientInfo::default();
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            let client = info.serve(client_io).await.expect("client");
            let request = CallToolRequestParams::new("zvec_grep_index").with_arguments(
                serde_json::json!({
                    "root": root.path(),
                    "embedding": "qwen/text-embedding-v4",
                    "endpoint": "https://embedding.example.test/v1",
                    "globs": [
                        {"pattern": "*.rs", "caseInsensitive": false},
                        {"pattern": "!target/**", "caseInsensitive": false}
                    ]
                })
                .as_object()
                .expect("arguments")
                .clone(),
            );
            for (reorder_top, reorder_nested) in [(true, false), (false, true), (true, true)] {
                let CallToolResponse::InputRequired(required) = client
                    .call_tool_once(request.clone())
                    .await
                    .expect("initial request")
                else {
                    panic!("consent required");
                };
                let responses = serde_json::from_value(serde_json::json!({
                    "remote_embedding_authorization": {
                        "action": "accept", "content": {"decision": "allow_once"}
                    }
                }))
                .expect("responses");
                let mut retry = request
                    .clone()
                    .with_request_state(required.request_state.expect("state"))
                    .with_input_responses(responses);
                if reorder_top {
                    retry.arguments = retry
                        .arguments
                        .map(|arguments| arguments.into_iter().rev().collect());
                }
                let arguments = retry.arguments.as_mut().expect("arguments");
                if reorder_nested {
                    for rule in arguments["globs"].as_array_mut().expect("rules") {
                        let fields = rule.as_object().expect("rule").clone();
                        *rule = serde_json::Value::Object(fields.into_iter().rev().collect());
                    }
                }
                assert_ne!(
                    serde_json::to_vec(&arguments).expect("reordered JSON"),
                    serde_json::to_vec(&request.arguments).expect("original JSON"),
                    "fixture must change serialized key order"
                );
                let mut changed_value = retry.clone();
                changed_value.arguments.as_mut().expect("arguments")["globs"][0]
                    ["caseInsensitive"] = true.into();
                let mut changed_array = retry.clone();
                changed_array.arguments.as_mut().expect("arguments")["globs"]
                    .as_array_mut()
                    .expect("rules")
                    .reverse();
                for changed in [changed_value, changed_array] {
                    let error = client.call_tool_once(changed).await.expect_err("changed request");
                    assert!(error.to_string().contains("Invalid or expired requestState"));
                }
                let CallToolResponse::Complete(result) = client
                    .call_tool_once(retry)
                    .await
                    .expect("object key order must not invalidate consent")
                else {
                    panic!("authorized request must complete");
                };
                assert!(!result.is_error.unwrap_or_default());
            }
            client.cancel().await.expect("client shutdown");
            task.await.expect("server task").expect("server shutdown");
        })
        .await
        .expect("continuation test terminates");
    }

    #[tokio::test]
    async fn wire_continuation_binds_arguments_expires_and_redeems_atomically() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let root = tempfile::tempdir().expect("valid consent test fixture");
            let state = Arc::new(ContinuationState::default());
            let (server_io, client_io) = tokio::io::duplex(65536);
            let server_state = state.clone();
            let task = tokio::spawn(async move { Server(server_state).serve(server_io).await.expect("valid consent test fixture").waiting().await });
            let mut info = ClientInfo::default();
            info.protocol_version = ProtocolVersion::V_2026_07_28;
            let client = info.serve(client_io).await.expect("valid consent test fixture");
            let request = CallToolRequestParams::new("zvec_grep_index").with_arguments(serde_json::json!({
                "root": root.path(), "embedding": "qwen/text-embedding-v4", "endpoint": "https://embedding.example.test/v1"
            }).as_object().expect("valid consent test fixture").clone());
            let CallToolResponse::InputRequired(required) = client.call_tool_once(request.clone()).await.expect("valid consent test fixture") else { panic!("consent required"); };
            let disclosed = disclosed_targets(&required);
            let canonical_root = root.path().canonicalize().expect("canonical workspace root");
            let display_root = zg_cli::output_path(&canonical_root);
            assert_eq!(disclosed[0]["target"]["root"], serde_json::json!(display_root));
            assert_eq!(disclosed[0]["target"]["workspace_roots"], serde_json::json!([display_root]));
            assert_eq!(disclosed[0]["target"]["endpoint_host"], "embedding.example.test");
            assert!(!root.path().join(".zvec-grep").exists(), "authorization must not create an index");
            let state_token = required.request_state.expect("valid consent test fixture");
            let responses = serde_json::from_value(serde_json::json!({"remote_embedding_authorization": {"action": "accept", "content": {"decision": "allow_once"}}})).expect("valid consent test fixture");
            let retry = request.clone().with_request_state(&state_token).with_input_responses(responses);
            let mut changed = retry.clone();
            changed.arguments.as_mut().expect("valid consent test fixture").insert("wait".into(), true.into());
            assert!(client.call_tool_once(changed).await.is_err(), "changed request rejected");
            let mut other_caller = retry.clone();
            other_caller.meta = Some(serde_json::from_value(serde_json::json!({"testPrincipal":"Bearer different-principal"})).expect("metadata"));
            assert!(client.call_tool_once(other_caller).await.is_err(), "different principal rejected");
            let mut forged = retry.clone(); forged.request_state = Some(format!("{state_token}x"));
            assert!(client.call_tool_once(forged).await.is_err(), "tampering rejected");
            let (first, second) = tokio::join!(client.call_tool_once(retry.clone()), client.call_tool_once(retry.clone()));
            assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1, "only one concurrent redemption succeeds");
            let CallToolResponse::Complete(result) = first.or(second).expect("authorized request") else { panic!("authorization must complete"); };
            let ContentBlock::Text(text) = &result.content[0] else { panic!("authorized target JSON"); };
            let authorized: Vec<authorization::IndexAuthorization> = serde_json::from_str(&text.text).expect("authorized targets");
            assert_eq!(authorized[0].root, canonical_root);
            assert_eq!(authorized[0].workspace_roots, vec![canonical_root]);
            assert!(client.call_tool_once(retry).await.is_err(), "replay rejected");
            let CallToolResponse::InputRequired(required) = client.call_tool_once(request.clone()).await.expect("valid consent test fixture") else { panic!("new consent"); };
            let token = required.request_state.expect("valid consent test fixture");
            state.pending.lock().expect("valid consent test fixture").get_mut(&token).expect("valid consent test fixture").0 = std::time::Instant::now();
            assert!(client.call_tool_once(request.with_request_state(token)).await.is_err(), "expired state rejected");
            client.cancel().await.expect("valid consent test fixture"); task.await.expect("valid consent test fixture").expect("valid consent test fixture");
        }).await.expect("continuation test terminates");
    }
}
