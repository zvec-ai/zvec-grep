use std::sync::{Arc, Mutex};

use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::json;
use zg_engine::api::relationships::{CallSite, SymbolDefinition, SymbolRelationships};

use super::*;

fn root() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

#[test]
fn relationship_inputs_preserve_engine_values_and_reject_unsafe_or_unknown_options() {
    let request = RelationshipInput {
        root: root(),
        symbol: "  Class::method  ".into(),
        limit: None,
        path: None,
    }
    .into_request()
    .expect("valid relationship request");
    assert_eq!(request.symbol, "  Class::method  ");
    assert_eq!(request.limit, None);
    for input in [
        json!({"root": root(), "symbol": "run", "limit": -1}),
        json!({"root": root(), "symbol": "run", "autoUpdate": true}),
        json!({"root": root(), "symbol": "run", "refresh": "wait"}),
    ] {
        assert!(serde_json::from_value::<RelationshipInput>(input).is_err());
    }
}

#[test]
fn relationship_tools_advertise_read_only_schemas_in_both_toolsets() {
    struct Status;
    impl ServerStatusProvider for Status {
        fn snapshot(&self) -> ServerStatusSnapshot {
            ServerStatusSnapshot::default()
        }
    }
    for server in [
        ZvecGrepMcpServer::agent_direct(Arc::new(ZvecGrep::new())),
        ZvecGrepMcpServer::full_direct(Arc::new(ZvecGrep::new()), Arc::new(Status)),
    ] {
        for name in ["callers", "callees"] {
            let tool = server
                .listed_tools()
                .into_iter()
                .find(|tool| tool.name == name)
                .expect("relationship tool");
            let value = serde_json::to_value(tool).expect("tool schema");
            assert_eq!(value["annotations"]["readOnlyHint"], true);
            assert_eq!(value["annotations"]["openWorldHint"], false);
            assert_eq!(value["inputSchema"]["additionalProperties"], false);
            assert_eq!(
                value["inputSchema"]["properties"]["limit"]["type"],
                "integer"
            );
            let properties = &value["inputSchema"]["properties"];
            assert_eq!(properties["limit"]["minimum"], 1);
            assert_eq!(properties["symbol"]["minLength"], 1);
            assert_eq!(
                properties["symbol"]["maxLength"],
                MAX_RELATIONSHIP_SYMBOL_CHARS
            );
            assert!(
                value["description"]
                    .as_str()
                    .expect("description")
                    .contains(&format!(
                        "more than {MAX_SYMBOL_MATCHES} indexed definitions"
                    ))
            );
            assert!(value["outputSchema"]["properties"]["matches"].is_object());
            assert!(value["outputSchema"]["properties"]["suggestions"].is_object());
        }
    }
}

struct RelationshipsOnly {
    calls: Mutex<Vec<(bool, RelationshipOptions)>>,
}

#[async_trait]
impl IndexOperationProvider for RelationshipsOnly {
    async fn submit_index(
        &self,
        _: IndexOptions,
        _: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        panic!("relationship reads must not submit index jobs")
    }

    async fn drop_index(&self, _: InfoOptions) -> Result<bool, EngineError> {
        panic!("relationship reads must not drop indexes")
    }

    async fn relationships(
        &self,
        _: &ZvecGrep,
        direction: RelationshipDirection,
        request: RelationshipOptions,
    ) -> Result<RelationshipResult, EngineError> {
        Ok(self.read(direction == RelationshipDirection::Callers, request))
    }
}

impl RelationshipsOnly {
    fn read(&self, incoming: bool, request: RelationshipOptions) -> RelationshipResult {
        assert_eq!(request.path, Some("src/subject.rs".into()));
        assert!(
            request
                .signal
                .as_ref()
                .is_some_and(|signal| !signal.is_cancelled())
        );
        let matches = vec![SymbolRelationships {
            symbol: SymbolDefinition {
                name: request.symbol.clone(),
                file_path: "src/subject.rs".into(),
                start_line: 3,
                end_line: 8,
            },
            truncated: true,
            edges: vec![
                CallSite {
                    file_path: if incoming {
                        "src/edge.rs"
                    } else {
                        "src/subject.rs"
                    }
                    .into(),
                    symbol: Some(SymbolDefinition {
                        name: if incoming { "Caller" } else { "Callee" }.into(),
                        file_path: "src/edge.rs".into(),
                        start_line: 10,
                        end_line: 20,
                    }),
                    line: Some(12),
                    column: Some(0),
                },
                CallSite {
                    file_path: "src/top_level.rs".into(),
                    symbol: None,
                    line: None,
                    column: None,
                },
            ],
        }];
        self.calls
            .lock()
            .expect("calls lock")
            .push((incoming, request));
        RelationshipResult {
            matches,
            suggestions: Vec::new(),
        }
    }
}

#[tokio::test]
async fn relationship_wire_calls_route_without_indexing_and_return_matching_json() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let provider = Arc::new(RelationshipsOnly {
            calls: Mutex::new(Vec::new()),
        });
        let server = ZvecGrepMcpServer::build_with_index_operations(
            Arc::new(ZvecGrep::new()),
            McpToolset::Agent,
            None,
            provider.clone(),
        );
        let (server_io, client_io) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .expect("server")
                .waiting()
                .await
        });
        let client = ().serve(client_io).await.expect("client");
        for name in ["callers", "callees"] {
            for arguments in [
                json!({"root": "relative", "symbol": "run"}),
                json!({"root": format!("{}/../other", root()), "symbol": "run"}),
                json!({"root": root(), "symbol": " "}),
                json!({"root": root(), "symbol": "x".repeat(MAX_RELATIONSHIP_SYMBOL_CHARS + 1)}),
                json!({"root": root(), "symbol": "::"}),
                json!({"root": root(), "symbol": "Foo."}),
                json!({"root": root(), "symbol": "run", "limit": 0}),
                json!({"root": root(), "symbol": "run", "path": ""}),
                json!({"root": root(), "symbol": "run", "path": root()}),
                json!({"root": root(), "symbol": "run", "path": "../main.rs"}),
                json!({"root": root(), "symbol": "run", "path": "src/../main.rs"}),
            ] {
                let error = client.call_tool(CallToolRequestParams::new(name)
                    .with_arguments(arguments.as_object().expect("arguments").clone()))
                    .await.expect_err("invalid parameters are protocol errors");
                assert!(matches!(error, rmcp::ServiceError::McpError(ref error)
                    if error.code == rmcp::model::ErrorCode::INVALID_PARAMS), "{error:?}");
            }
        }
        assert!(provider.calls.lock().expect("recorded calls").is_empty(), "invalid input must not reach the provider/daemon");
        for (name, endpoint) in [("callers", "Caller"), ("callees", "Callee")] {
            let response = client
                .call_tool(
                    CallToolRequestParams::new(name).with_arguments(
                        json!({"root": root(), "symbol": " Subject::run ", "path": "src/subject.rs", "limit": 2})
                            .as_object()
                            .expect("arguments")
                            .clone(),
                    ),
                )
                .await
                .expect("relationship call");
            let value = serde_json::to_value(response).expect("response JSON");
            assert_ne!(value["isError"], true);
            assert_eq!(value["structuredContent"]["suggestions"], json!([]));
            let matches = &value["structuredContent"]["matches"];
            assert_eq!(matches[0]["name"], " Subject::run ");
            assert_eq!(matches[0]["filePath"], "src/subject.rs");
            assert_eq!(matches[0]["truncated"], true);
            assert_eq!(matches[0]["edges"][0]["symbol"]["name"], endpoint);
            assert_eq!(matches[0]["edges"][0]["column"], 0);
            assert!(matches[0]["edges"][1]["symbol"].is_null());
            assert_eq!(matches[0]["edges"][1]["filePath"], "src/top_level.rs");
            assert_eq!(
                matches[0]["edges"][0]["filePath"],
                if name == "callers" {
                    "src/edge.rs"
                } else {
                    "src/subject.rs"
                }
            );
            let text = value["content"][0]["text"].as_str().expect("text result");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text).expect("text JSON"),
                value["structuredContent"]
            );
        }
        {
            let calls = provider.calls.lock().expect("recorded calls");
            assert_eq!(calls.len(), 2);
            assert!(calls[0].0);
            assert!(!calls[1].0);
            assert!(calls.iter().all(|(_, request)| request.limit == Some(2)));
        }
        client.cancel().await.expect("close client");
        task.await.expect("server task").expect("server stop");
    })
    .await
    .expect("MCP test terminates");
}
