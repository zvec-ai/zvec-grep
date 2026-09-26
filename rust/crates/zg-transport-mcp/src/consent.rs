//! Remote embedding consent uses the same signed grants as the CLI.

mod continuation;
use continuation::ContinuationApproval;
pub(crate) use continuation::ContinuationState;

use rmcp::{
    RoleServer,
    model::{ElicitRequestParams, ElicitResult, ElicitationAction},
    service::RequestContext,
};
use zg_engine::{
    EngineError,
    api::{context::ContextOptions, index::IndexOptions},
    authorization::{self, IndexAuthorization},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    Once,
    Workspace,
    FtsOnly,
}

pub(crate) async fn index(
    options: &mut IndexOptions,
    context: &RequestContext<RoleServer>,
) -> Result<(), EngineError> {
    let targets = authorization::index_authorizations(options)?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut decisions = Vec::new();
    for target in &targets {
        decisions.push(ask(target, false, true, false, context).await?);
    }
    if authorization::index_authorizations(options)? != targets {
        return Err(EngineError::permission_denied(
            "Workspace authorization scope changed while awaiting consent; retry to review all destinations",
        ));
    }
    for (target, decision) in targets.iter().zip(decisions) {
        apply(decision, target, &mut options.authorized_remote)?;
    }
    options.root = Some(targets[0].root.clone());
    Ok(())
}

pub(crate) async fn search(
    options: &mut ContextOptions,
    context: &RequestContext<RoleServer>,
) -> Result<(), EngineError> {
    let targets = authorization::query_authorizations(options)?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut decisions = Vec::new();
    for required in &targets {
        let decision = ask(
            &required.target,
            required.query_text,
            required.workspace_content,
            true,
            context,
        )
        .await?;
        if decision == Decision::FtsOnly {
            zg_cli::use_fts_only(options);
            return Ok(());
        }
        decisions.push(decision);
    }
    if authorization::query_authorizations(options)? != targets {
        return Err(EngineError::permission_denied(
            "Workspace authorization scope changed while awaiting consent; retry to review all destinations",
        ));
    }
    for (required, decision) in targets.iter().zip(decisions) {
        apply(decision, &required.target, &mut options.authorized_remote)?;
    }
    if targets.len() == 1 {
        options.authorization_model = Some(targets[0].target.model.clone());
    }
    Ok(())
}

fn apply(
    decision: Decision,
    target: &IndexAuthorization,
    authorized_remote: &mut Vec<IndexAuthorization>,
) -> Result<(), EngineError> {
    match decision {
        Decision::Once => authorized_remote.push(target.clone()),
        Decision::Workspace => authorization::grant_index(target)?,
        Decision::FtsOnly => {
            return Err(EngineError::permission_denied(
                "FTS-only consent cannot authorize indexing",
            ));
        }
    }
    Ok(())
}

async fn ask(
    target: &IndexAuthorization,
    query_text: bool,
    workspace_content: bool,
    allow_fts: bool,
    context: &RequestContext<RoleServer>,
) -> Result<Decision, EngineError> {
    if context.ct.is_cancelled() {
        return Err(EngineError::cancelled("MCP authorization was cancelled"));
    }
    if let Some(approval) = context.extensions.get::<ContinuationApproval>() {
        if approval.targets.iter().any(|required| {
            &required.target == target
                && required.query_text == query_text
                && required.workspace_content == workspace_content
        }) {
            return Ok(approval.decision);
        }
        return Err(EngineError::permission_denied(
            "Remote authorization scope changed; retry consent",
        ));
    }
    let supported = context
        .client_capabilities()
        .and_then(|caps| caps.elicitation)
        .is_some_and(|cap| cap.form.is_some() || cap.url.is_none());
    if !supported {
        return Err(EngineError::permission_denied(
            "Remote embedding requires consent. This MCP client does not support form elicitation; grant workspace authorization with `zg --auth` or use FTS with autoUpdate: false.",
        ));
    }
    let mut choices = vec!["once", "workspace", "cancel"];
    if allow_fts {
        choices.insert(2, "fts_only");
    }
    let requested_schema = serde_json::from_value(serde_json::json!({
        "type": "object", "properties": {"choice": {
            "type": "string", "enum": choices,
            "description": "once: this operation only; workspace: persist consent for this root/model/endpoint and future watcher updates; fts_only: lexical query without refresh; cancel: send nothing"
        }}, "required": ["choice"]
    })).map_err(|error| EngineError::internal(format!("Invalid consent schema: {error}")))?;
    let message = format!(
        "Allow remote embedding? Destination: {} (host: {}). Model: {}. Workspace: {}. Source roots: {}. Query text sent: {query_text}. Workspace content sent for indexing/refresh: {workspace_content}. Only explicit acceptance permits transmission. Do not enter credentials in this form.",
        target.endpoint,
        target.endpoint_host,
        target.model,
        target.root.display(),
        target
            .workspace_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    );
    let mut handle = context
        .peer
        .send_cancellable_request(
            rmcp::model::ServerRequest::ElicitRequest(rmcp::model::Request::new(
                ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message,
                    requested_schema,
                },
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await
        .map_err(|_| EngineError::permission_denied("Remote consent could not be requested"))?;
    let result = tokio::select! {
        biased;
        () = context.ct.cancelled() => {
            let _ = handle.cancel(Some("Originating MCP request cancelled".into())).await;
            return Err(EngineError::cancelled("MCP authorization was cancelled"));
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            let _ = handle.cancel(Some("Remote consent timed out".into())).await;
            return Err(EngineError::permission_denied("Remote consent timed out; no authorization was granted"));
        },
        result = &mut handle.rx => match result {
            Ok(Ok(rmcp::model::ClientResult::ElicitResult(result))) => result,
            _ => return Err(EngineError::permission_denied("Remote consent could not be obtained; no authorization was granted")),
        },
    };
    if context.ct.is_cancelled() {
        return Err(EngineError::cancelled("MCP authorization was cancelled"));
    }
    decision(&result, allow_fts)
}

fn decision(result: &ElicitResult, allow_fts: bool) -> Result<Decision, EngineError> {
    if result.action == ElicitationAction::Accept {
        match result
            .content
            .as_ref()
            .and_then(|value| value.get("choice"))
            .and_then(serde_json::Value::as_str)
        {
            Some("once") => return Ok(Decision::Once),
            Some("workspace") => return Ok(Decision::Workspace),
            Some("fts_only") if allow_fts => return Ok(Decision::FtsOnly),
            _ => {}
        }
    }
    Err(EngineError::permission_denied(
        "Remote embedding consent was declined, cancelled, or invalid; no authorization was granted",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{ClientHandler, RoleClient, ServerHandler, ServiceExt, model::*};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct ConsentServer;

    impl ServerHandler for ConsentServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            let target = IndexAuthorization {
                root: std::env::temp_dir().join("consent-workspace"),
                workspace_roots: vec![std::env::temp_dir().join("consent-source")],
                model: "remote/test-model".into(),
                endpoint: "https://embedding.example.test/v1".into(),
                endpoint_host: "embedding.example.test".into(),
            };
            Ok(match ask(&target, true, true, true, &context).await {
                Ok(choice) => {
                    CallToolResult::success(vec![ContentBlock::text(format!("{choice:?}"))])
                }
                Err(error) => super::super::error_result(&error),
            }
            .into())
        }
    }

    struct ConsentClient {
        capability: Option<ElicitationCapability>,
        result: ElicitResult,
        calls: Arc<AtomicUsize>,
    }

    struct WaitingClient {
        prompted: Arc<tokio::sync::Notify>,
        cancelled: Arc<tokio::sync::Notify>,
        signal: tokio_util::sync::CancellationToken,
        request_id: std::sync::Mutex<Option<RequestId>>,
    }

    impl ClientHandler for WaitingClient {
        fn get_info(&self) -> ClientInfo {
            let mut capabilities = ClientCapabilities::default();
            capabilities.elicitation =
                Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
            ClientInfo::new(capabilities, Implementation::new("waiting-client", "1"))
        }

        async fn create_elicitation(
            &self,
            _request: ElicitRequestParams,
            context: RequestContext<RoleClient>,
        ) -> Result<ElicitResult, rmcp::ErrorData> {
            *self.request_id.lock().expect("request id") = Some(context.id);
            self.prompted.notify_one();
            self.signal.cancelled().await;
            Ok(ElicitResult::new(ElicitationAction::Cancel))
        }

        fn on_cancelled(
            &self,
            params: CancelledNotificationParam,
            _context: rmcp::service::NotificationContext<RoleClient>,
        ) -> impl Future<Output = ()> {
            assert_eq!(
                params.request_id,
                *self.request_id.lock().expect("request id")
            );
            self.signal.cancel();
            self.cancelled.notify_one();
            std::future::ready(())
        }
    }

    #[tokio::test]
    async fn cancelling_the_tool_also_cancels_the_pending_consent_form() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let prompted = Arc::new(tokio::sync::Notify::new());
            let cancelled = Arc::new(tokio::sync::Notify::new());
            let (server_io, client_io) = tokio::io::duplex(8192);
            let task = tokio::spawn(async move {
                ConsentServer
                    .serve(server_io)
                    .await
                    .expect("server")
                    .waiting()
                    .await
            });
            let client = WaitingClient {
                prompted: prompted.clone(),
                cancelled: cancelled.clone(),
                signal: tokio_util::sync::CancellationToken::new(),
                request_id: std::sync::Mutex::new(None),
            }
            .serve(client_io)
            .await
            .expect("client");
            let handle = client
                .send_cancellable_request(
                    ClientRequest::CallToolRequest(Request::new(CallToolRequestParams::new(
                        "consent",
                    ))),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .expect("request");
            prompted.notified().await;
            handle
                .cancel(Some("cancel tool".into()))
                .await
                .expect("cancel");
            cancelled.notified().await;
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("server stop");
        })
        .await
        .expect("consent cancellation must terminate");
    }

    impl ClientHandler for ConsentClient {
        fn get_info(&self) -> ClientInfo {
            let mut capabilities = ClientCapabilities::default();
            capabilities.elicitation = self.capability.clone();
            ClientInfo::new(capabilities, Implementation::new("test-client", "1"))
        }

        fn create_elicitation(
            &self,
            request: ElicitRequestParams,
            _context: RequestContext<RoleClient>,
        ) -> impl Future<Output = Result<ElicitResult, rmcp::ErrorData>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } = request
            else {
                panic!("form expected");
            };
            assert!(message.contains("https://embedding.example.test/v1"));
            assert!(message.contains("remote/test-model"));
            assert!(message.contains("consent-source"));
            assert!(message.contains("Query text sent: true"));
            assert!(message.contains("indexing/refresh: true"));
            let schema = serde_json::json!(requested_schema);
            assert_eq!(schema["required"], serde_json::json!(["choice"]));
            assert_eq!(
                schema["properties"]["choice"]["enum"],
                serde_json::json!(["once", "workspace", "fts_only", "cancel"])
            );
            std::future::ready(Ok(self.result.clone()))
        }
    }

    #[tokio::test]
    async fn wire_consent_checks_capability_and_discloses_scope() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for capability in [
                None,
                Some(ElicitationCapability::new().with_url(UrlElicitationCapability::new())),
                Some(ElicitationCapability::new()),
                Some(ElicitationCapability::new().with_form(FormElicitationCapability::new())),
            ] {
                for choice in ["once", "workspace", "fts_only", "cancel"] {
                    let supported = capability
                        .as_ref()
                        .is_some_and(|cap| cap.form.is_some() || cap.url.is_none());
                    let calls = Arc::new(AtomicUsize::new(0));
                    let (server_io, client_io) = tokio::io::duplex(8192);
                    let task = tokio::spawn(async move {
                        ConsentServer
                            .serve(server_io)
                            .await
                            .expect("server")
                            .waiting()
                            .await
                    });
                    let client = ConsentClient {
                        capability: capability.clone(),
                        result: ElicitResult::new(ElicitationAction::Accept)
                            .with_content(serde_json::json!({"choice": choice})),
                        calls: calls.clone(),
                    }
                    .serve(client_io)
                    .await
                    .expect("client");
                    let result = client
                        .call_tool(CallToolRequestParams::new("consent"))
                        .await
                        .expect("call");
                    assert_eq!(
                        result.is_error.unwrap_or(false),
                        !supported || choice == "cancel"
                    );
                    assert_eq!(calls.load(Ordering::SeqCst), usize::from(supported));
                    client.cancel().await.expect("close client");
                    task.await.expect("server task").expect("server stop");
                }
            }
        })
        .await
        .expect("protocol tests must terminate");
    }

    #[test]
    fn once_consent_records_only_the_disclosed_destination() {
        let target = IndexAuthorization {
            root: std::env::temp_dir().join("consent-workspace"),
            workspace_roots: vec![std::env::temp_dir().join("consent-workspace")],
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://embedding.example.test/v1".into(),
            endpoint_host: "embedding.example.test".into(),
        };
        let mut approved = Vec::new();
        apply(Decision::Once, &target, &mut approved).expect("once");
        assert_eq!(approved, vec![target]);
    }

    #[test]
    fn only_explicit_valid_acceptance_grants_consent() {
        for action in [
            ElicitationAction::Accept,
            ElicitationAction::Decline,
            ElicitationAction::Cancel,
        ] {
            for choice in ["once", "workspace", "fts_only", "cancel", "unexpected"] {
                let result = ElicitResult::new(action.clone())
                    .with_content(serde_json::json!({"choice": choice}));
                assert_eq!(
                    decision(&result, true).is_ok(),
                    action == ElicitationAction::Accept
                        && matches!(choice, "once" | "workspace" | "fts_only")
                );
                assert_eq!(
                    decision(&result, false).is_ok(),
                    action == ElicitationAction::Accept && matches!(choice, "once" | "workspace")
                );
            }
        }
        assert!(decision(&ElicitResult::new(ElicitationAction::Accept), true).is_err());
        assert!(
            decision(
                &ElicitResult::new(ElicitationAction::Accept)
                    .with_content(serde_json::json!({"choice": true})),
                true
            )
            .is_err()
        );
    }
}
