use super::*;
use crate::{IndexOperationResult, IndexRuntimeSnapshot, McpToolset, ZvecGrepMcpServer};
use async_trait::async_trait;
use rmcp::{ClientHandler, ServiceExt, model::*};
use std::{collections::VecDeque, sync::Arc};

#[derive(Default)]
struct Jobs(Mutex<Vec<IndexOptions>>);

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
        Some(IndexRuntimeSnapshot {
            index_status: None,
            completion_baseline: None,
            watcher_active: false,
            dirty_revision: 0,
            indexed_revision: 0,
            active_job_id: Some("setup-job".into()),
            job_state: Some(IndexOperationState::Queued),
            progress: None,
            error: None,
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
        _: ElicitRequestParams,
        _: RequestContext<rmcp::RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, ErrorData>> {
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

#[tokio::test]
async fn an_existing_index_preserves_the_original_model_not_found_error() {
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
        let result = value(
            client
                .call_tool_once(retry.clone())
                .await
                .expect("accepted setup"),
        );
        assert_eq!(result["status"], "indexing");
        assert_eq!(result["job_id"], "setup-job");
        assert_eq!(jobs.0.lock().expect("jobs").len(), 1);
        assert!(
            client.call_tool_once(retry).await.is_err(),
            "state is single use"
        );
        let status = value(
            client
                .call_tool_once(original)
                .await
                .expect("pending index"),
        );
        assert_eq!(status["state"], "queued");
        assert_eq!(
            jobs.0.lock().expect("jobs").len(),
            1,
            "pending queries must not submit another job"
        );
        client.cancel().await.expect("close client");
        task.await.expect("server task").expect("stop server");
    })
    .await
    .expect("setup test terminates");
}

#[tokio::test]
async fn cancelled_or_expired_setup_never_submits_an_index() {
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
    tokio::time::timeout(Duration::from_secs(15), async {
        for forms in [true, false] {
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
                version: ProtocolVersion::V_2025_11_25,
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
