use super::*;
use crate::{IndexOperationResult, IndexRuntimeSnapshot, McpToolset, ZvecGrepMcpServer};
use async_trait::async_trait;
use rmcp::{ClientHandler, ServiceExt, model::*};
use std::{collections::VecDeque, sync::Arc};

#[derive(Default)]
struct Jobs(
    Mutex<Vec<IndexOptions>>,
    Mutex<Option<crate::IndexOperationError>>,
);

struct ExistingIndex;

#[async_trait]
impl IndexOperationProvider for ExistingIndex {
    async fn submit_index(
        &self,
        _: IndexOptions,
        _: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        panic!("existing indexes must never trigger initialization");
    }

    async fn drop_index(&self, _: InfoOptions) -> Result<bool, EngineError> {
        panic!("unexpected drop");
    }

    async fn search(
        &self,
        _: &ZvecGrep,
        _: ContextOptions,
    ) -> Result<crate::ContextResult, EngineError> {
        Err(EngineError::not_found(
            "The configured remote embedding model is unavailable",
        ))
    }

    async fn info(
        &self,
        engine: &ZvecGrep,
        options: InfoOptions,
    ) -> Result<crate::InfoResult, EngineError> {
        let mut info = engine.info(options).await?;
        info.indexed = true;
        Ok(info)
    }
}

#[async_trait]
impl IndexOperationProvider for Jobs {
    fn supports_background_indexing(&self) -> bool {
        true
    }

    async fn submit_index(
        &self,
        mut options: IndexOptions,
        wait: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        assert!(!wait, "MCP first indexing must be detached");
        assert!(
            options.signal.is_none(),
            "accepted jobs must survive request completion"
        );
        let root =
            std::fs::canonicalize(options.root.as_ref().expect("root")).expect("canonical root");
        options.root = Some(root.clone());
        self.0.lock().expect("jobs").push(options);
        Ok(IndexOperationResult {
            root,
            job_id: "setup-job".into(),
            state: IndexOperationState::Queued,
            reused: false,
            error: None,
            result: None,
        })
    }

    async fn drop_index(&self, _: InfoOptions) -> Result<bool, EngineError> {
        panic!("setup must never drop an index")
    }

    fn runtime_snapshot(&self, root: &Path) -> Option<IndexRuntimeSnapshot> {
        self.0
            .lock()
            .expect("jobs")
            .iter()
            .find(|options| options.root.as_deref() == Some(root))?;
        let error = self.1.lock().expect("job error").clone();
        Some(IndexRuntimeSnapshot {
            index_status: None,
            completion_baseline: None,
            watcher_active: false,
            dirty_revision: 0,
            indexed_revision: 0,
            active_job_id: Some("setup-job".into()),
            job_state: Some(if error.is_some() {
                IndexOperationState::Failed
            } else {
                IndexOperationState::Queued
            }),
            progress: None,
            error,
        })
    }
}

struct Client {
    version: ProtocolVersion,
    answers: Mutex<VecDeque<ElicitResult>>,
    forms: bool,
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        let mut capabilities = ClientCapabilities::default();
        if self.forms {
            capabilities.elicitation =
                Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
        }
        let mut info = ClientInfo::new(capabilities, Implementation::new("setup-test-client", "1"));
        info.protocol_version = self.version.clone();
        info
    }

    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<rmcp::RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, ErrorData>> {
        let ElicitRequestParams::FormElicitationParams {
            requested_schema, ..
        } = request
        else {
            panic!("form required");
        };
        let schema = json!(requested_schema);
        let properties = schema["properties"].as_object().expect("form properties");
        for property in properties.values() {
            if self.version < ProtocolVersion::V_2025_11_25 {
                assert!(property["enum"].is_array(), "legacy choices: {property}");
                assert!(
                    property["enumNames"].is_array(),
                    "legacy titles: {property}"
                );
                assert!(property.get("oneOf").is_none());
            } else {
                assert!(property["oneOf"].is_array(), "titled choices: {property}");
            }
        }
        std::future::ready(Ok(self
            .answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("expected interaction")))
    }
}

fn request(root: &Path) -> CallToolRequestParams {
    CallToolRequestParams::new("zvec_grep_search").with_arguments(
        json!({"root": root, "fts": "needle"})
            .as_object()
            .expect("arguments")
            .clone(),
    )
}

fn answer(request: CallToolRequestParams, token: String, content: &Value) -> CallToolRequestParams {
    request.with_request_state(token).with_input_responses(
        serde_json::from_value(json!({
            "index_setup": {"action": "accept", "content": content}
        }))
        .expect("input responses"),
    )
}

fn value(response: CallToolResponse) -> Value {
    let result = match response {
        CallToolResponse::Complete(result) => result,
        other => panic!("expected complete response, got {other:?}"),
    };
    result.structured_content.expect("structured response")
}

fn setup_value(response: CallToolResponse, root: &Path) -> Value {
    let CallToolResponse::Complete(result) = &response else {
        panic!("expected setup result");
    };
    let ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected JSON text");
    };
    let text: Value = serde_json::from_str(&text.text).expect("JSON text");
    let result = value(response);
    assert_eq!(text, result);
    let canonical = std::fs::canonicalize(root).expect("canonical root");
    let expected = canonical.to_str().expect("Unicode temporary directory");
    #[cfg(windows)]
    let expected = expected
        .strip_prefix(r"\\?\")
        .expect("verbatim fixture path");
    assert_eq!(result["root"], expected);
    result
}

fn in_isolated_model_cache(test_name: &str) -> bool {
    const FIXTURE_ENV: &str = "ZG_MCP_SETUP_TEST_MODEL_CACHE";
    if let Some(cache) = std::env::var_os(FIXTURE_ENV) {
        assert_eq!(
            std::env::var_os("ZVEC_GREP_MODEL_CACHE"),
            Some(cache.clone())
        );
        assert!(
            std::fs::read_dir(cache)
                .expect("isolated model cache")
                .next()
                .is_none(),
            "setup protocol tests must start with an empty model cache"
        );
        return true;
    }
    // The MCP request has no model-cache override. Set it only in a child
    // process so parallel tests never mutate or scan the real CI model cache.
    let cache = tempfile::tempdir().expect("isolated model cache");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test_name, "--nocapture"])
        .env(FIXTURE_ENV, cache.path())
        .env("ZVEC_GREP_MODEL_CACHE", cache.path())
        .output()
        .expect("isolated setup test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed;"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        cache
            .path()
            .read_dir()
            .expect("model cache")
            .next()
            .is_none(),
        "setup protocol tests must not download models"
    );
    false
}

#[test]
fn model_and_authorization_forms_follow_the_negotiated_protocol() {
    let root = PathBuf::from("/workspace");
    let setup = IndexSetup {
        models: vec![zg_engine::api::index::setup::EmbeddingChoice {
            reference: "qwen/text-embedding-v4".into(),
            local: false,
            downloaded: false,
            configured: true,
            credential_available: true,
            download_bytes: None,
        }],
        default_model: Some("qwen/text-embedding-v4".into()),
    };
    let model = Phase::Model {
        root: root.clone(),
        setup: setup.clone(),
    };
    let authorization = Phase::Authorization {
        options: Box::default(),
        target: IndexAuthorization {
            workspace_roots: vec![root.clone()],
            root,
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://embedding.example.test/v1".into(),
            endpoint_host: "embedding.example.test".into(),
        },
        setup,
    };
    for version in [
        None,
        Some(ProtocolVersion::V_2025_06_18),
        Some(ProtocolVersion::V_2025_11_25),
        Some(ProtocolVersion::V_2026_07_28),
    ] {
        for (phase, field, values, titles) in [
            (
                &model,
                "model",
                json!(["qwen/text-embedding-v4"]),
                json!(["Remote · qwen/text-embedding-v4 · Credential available"]),
            ),
            (
                &authorization,
                "decision",
                json!(["allow_once", "allow_workspace", "back"]),
                json!([
                    "Allow once",
                    "Allow for this workspace",
                    "Back to model selection"
                ]),
            ),
        ] {
            let (message, schema) = form(phase, version.as_ref());
            let request = ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema: serde_json::from_value(schema).expect("valid elicitation schema"),
            };
            let wire = json!(request);
            let property = &wire["requestedSchema"]["properties"][field];
            if version
                .as_ref()
                .is_some_and(|version| *version >= ProtocolVersion::V_2025_11_25)
            {
                let choices = property["oneOf"].as_array().expect("titled choices");
                assert_eq!(
                    choices
                        .iter()
                        .map(|choice| choice["const"].clone())
                        .collect::<Value>(),
                    values
                );
                assert_eq!(
                    choices
                        .iter()
                        .map(|choice| choice["title"].clone())
                        .collect::<Value>(),
                    titles
                );
                assert!(property.get("enumNames").is_none());
            } else {
                assert_eq!(property["enum"], values);
                assert_eq!(property["enumNames"], titles);
                assert!(property.get("oneOf").is_none());
            }
            if field == "model" {
                assert_eq!(property["default"], "qwen/text-embedding-v4");
            }
        }
    }
}

#[tokio::test]
async fn an_existing_index_preserves_the_original_model_not_found_error() {
    if !in_isolated_model_cache(
        "setup::tests::an_existing_index_preserves_the_original_model_not_found_error",
    ) {
        return;
    }
    let root = tempfile::tempdir().expect("workspace");
    let server = ZvecGrepMcpServer::build_with_index_operations(
        Arc::new(ZvecGrep::new()),
        McpToolset::Agent,
        None,
        Arc::new(ExistingIndex),
    );
    let (server_io, client_io) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .expect("server")
            .waiting()
            .await
    });
    let client = Client {
        version: ProtocolVersion::V_2026_07_28,
        answers: Mutex::default(),
        forms: true,
    }
    .serve(client_io)
    .await
    .expect("client");
    let CallToolResponse::Complete(result) = client
        .call_tool_once(request(root.path()))
        .await
        .expect("search")
    else {
        panic!("original error must be preserved");
    };
    assert_eq!(result.is_error, Some(true));
    assert!(
        serde_json::to_string(&result)
            .expect("error result")
            .contains("configured remote embedding model is unavailable")
    );
    assert!(!root.path().join(".zvec-grep").exists());
    client.cancel().await.expect("close client");
    task.await.expect("server task").expect("stop server");
}

#[tokio::test]
async fn modern_setup_accepts_key_reordering_binds_arguments_and_rejects_replay() {
    if !in_isolated_model_cache(
        "setup::tests::modern_setup_accepts_key_reordering_binds_arguments_and_rejects_replay",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let root = tempfile::tempdir().expect("workspace");
        let jobs = Arc::new(Jobs::default());
        let server = ZvecGrepMcpServer::build_with_index_operations(
            Arc::new(ZvecGrep::new()),
            McpToolset::Agent,
            None,
            jobs.clone(),
        );
        let (server_io, client_io) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .expect("server")
                .waiting()
                .await
        });
        let client = Client {
            version: ProtocolVersion::V_2026_07_28,
            answers: Mutex::default(),
            forms: true,
        }
        .serve(client_io)
        .await
        .expect("client");
        let mut original = request(root.path());
        original.arguments.as_mut().expect("arguments").insert(
            "globs".into(),
            json!([
                {"pattern": "*.rs", "caseInsensitive": false},
                {"pattern": "!target/**", "caseInsensitive": false}
            ]),
        );
        let CallToolResponse::InputRequired(required) = client
            .call_tool_once(original.clone())
            .await
            .expect("model prompt")
        else {
            panic!("model selection required");
        };
        assert!(jobs.0.lock().expect("jobs").is_empty());
        assert!(!root.path().join(".zvec-grep").exists());
        let mut retry = answer(
            original.clone(),
            required.request_state.expect("state"),
            &json!({"model": "local/potion-code-16m-v2"}),
        );
        retry.arguments = retry
            .arguments
            .map(|arguments| arguments.into_iter().rev().collect());
        for rule in retry.arguments.as_mut().expect("arguments")["globs"]
            .as_array_mut()
            .expect("rules")
        {
            let fields = rule.as_object().expect("rule").clone();
            *rule = Value::Object(fields.into_iter().rev().collect());
        }
        assert_ne!(
            serde_json::to_vec(&retry.arguments).expect("reordered arguments"),
            serde_json::to_vec(&original.arguments).expect("original arguments"),
            "the fixture must change serialized object key order"
        );
        let mut changed = retry.clone();
        changed
            .arguments
            .as_mut()
            .expect("args")
            .insert("fts".into(), json!("changed"));
        assert!(client.call_tool_once(changed).await.is_err());
        let mut changed_value = retry.clone();
        changed_value.arguments.as_mut().expect("arguments")["globs"][0]["caseInsensitive"] =
            true.into();
        let mut changed_array = retry.clone();
        changed_array.arguments.as_mut().expect("arguments")["globs"]
            .as_array_mut()
            .expect("rules")
            .reverse();
        for changed in [changed_value, changed_array] {
            assert!(client.call_tool_once(changed).await.is_err());
        }
        let result = setup_value(
            client
                .call_tool_once(retry.clone())
                .await
                .expect("accepted setup"),
            root.path(),
        );
        assert_eq!(result["status"], "indexing");
        assert_eq!(result["job_id"], "setup-job");
        assert_eq!(jobs.0.lock().expect("jobs").len(), 1);
        assert!(
            client.call_tool_once(retry).await.is_err(),
            "state is single use"
        );
        let status = setup_value(
            client
                .call_tool_once(original.clone())
                .await
                .expect("pending index"),
            root.path(),
        );
        assert_eq!(status["state"], "queued");
        assert_eq!(
            jobs.0.lock().expect("jobs").len(),
            1,
            "pending queries must not submit another job"
        );
        *jobs.1.lock().expect("job error") = Some(crate::IndexOperationError {
            report: EngineError::storage_failure("injected build failure").into_report(),
            retryable: true,
        });
        let failed = setup_value(
            client.call_tool_once(original).await.expect("failed index"),
            root.path(),
        );
        assert_eq!(failed["status"], "indexing_failed");
        assert_eq!(
            jobs.0.lock().expect("jobs")[0].root,
            Some(std::fs::canonicalize(root.path()).expect("canonical root")),
            "formatting replies must preserve the internal job location"
        );
        client.cancel().await.expect("close client");
        task.await.expect("server task").expect("stop server");
    })
    .await
    .expect("setup test terminates");
}

#[tokio::test]
async fn cancelled_or_expired_setup_never_submits_an_index() {
    if !in_isolated_model_cache("setup::tests::cancelled_or_expired_setup_never_submits_an_index") {
        return;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let root = tempfile::tempdir().expect("workspace");
        let jobs = Arc::new(Jobs::default());
        let server = ZvecGrepMcpServer::build_with_index_operations(
            Arc::new(ZvecGrep::new()),
            McpToolset::Agent,
            None,
            jobs.clone(),
        );
        let state = server.setup_state.clone();
        let (server_io, client_io) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .expect("server")
                .waiting()
                .await
        });
        let client = Client {
            version: ProtocolVersion::V_2026_07_28,
            answers: Mutex::default(),
            forms: true,
        }
        .serve(client_io)
        .await
        .expect("client");
        let original = request(root.path());
        let CallToolResponse::InputRequired(required) = client
            .call_tool_once(original.clone())
            .await
            .expect("model prompt")
        else {
            panic!("model prompt");
        };
        let token = required.request_state.expect("state");
        let cancelled = original
            .clone()
            .with_request_state(token)
            .with_input_responses(
                serde_json::from_value(json!({"index_setup": {"action": "cancel"}}))
                    .expect("cancel response"),
            );
        let CallToolResponse::Complete(result) = client
            .call_tool_once(cancelled)
            .await
            .expect("cancelled setup")
        else {
            panic!("cancel result");
        };
        assert_eq!(result.is_error, Some(true));
        let CallToolResponse::InputRequired(required) = client
            .call_tool_once(original.clone())
            .await
            .expect("new prompt")
        else {
            panic!("model prompt");
        };
        let token = required.request_state.expect("state");
        state
            .pending
            .lock()
            .expect("pending states")
            .get_mut(&token)
            .expect("pending state")
            .expires = Instant::now();
        assert!(
            client
                .call_tool_once(answer(
                    original,
                    token,
                    &json!({"model": "local/potion-code-16m-v2"})
                ))
                .await
                .is_err()
        );
        assert!(jobs.0.lock().expect("jobs").is_empty());
        assert!(!root.path().join(".zvec-grep").exists());
        client.cancel().await.expect("close client");
        task.await.expect("server task").expect("stop server");
    })
    .await
    .expect("setup test terminates");
}

#[tokio::test]
async fn legacy_forms_can_select_models_and_unsupported_clients_receive_instructions() {
    if !in_isolated_model_cache(
        "setup::tests::legacy_forms_can_select_models_and_unsupported_clients_receive_instructions",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        for (version, forms) in [
            (ProtocolVersion::V_2025_06_18, true),
            (ProtocolVersion::V_2025_06_18, false),
            (ProtocolVersion::V_2025_11_25, true),
            (ProtocolVersion::V_2025_11_25, false),
        ] {
            let root = tempfile::tempdir().expect("workspace");
            let jobs = Arc::new(Jobs::default());
            let server = ZvecGrepMcpServer::build_with_index_operations(
                Arc::new(ZvecGrep::new()),
                McpToolset::Agent,
                None,
                jobs.clone(),
            );
            let (server_io, client_io) = tokio::io::duplex(65536);
            let task = tokio::spawn(async move {
                server
                    .serve(server_io)
                    .await
                    .expect("server")
                    .waiting()
                    .await
            });
            let mut selection = ElicitResult::new(ElicitationAction::Accept);
            selection.content = Some(json!({"model": "local/potion-code-16m-v2"}));
            let client = Client {
                version,
                answers: Mutex::new(VecDeque::from([selection])),
                forms,
            }
            .serve(client_io)
            .await
            .expect("client");
            let response = client
                .call_tool_once(request(root.path()))
                .await
                .expect("search");
            if forms {
                assert_eq!(value(response)["status"], "indexing");
                assert_eq!(jobs.0.lock().expect("jobs").len(), 1);
            } else {
                let CallToolResponse::Complete(result) = response else {
                    panic!("instructions");
                };
                assert_eq!(result.is_error, Some(true));
                assert!(jobs.0.lock().expect("jobs").is_empty());
            }
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("stop server");
        }
    })
    .await
    .expect("setup test terminates");
}
