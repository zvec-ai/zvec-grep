use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock},
};
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{ContextOptions, ContextResult},
        index::IndexOptions,
        info::InfoOptions,
    },
};
use zg_testkit::{McpSearchPresentationCase, load_mcp_search_cases};

use super::{SearchPreview, format_search_result};
use crate::{IndexOperationProvider, IndexOperationResult, McpToolset, ZvecGrepMcpServer};

fn fixtures() -> Vec<McpSearchPresentationCase> {
    load_mcp_search_cases(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/mcp/search-presentation.json"),
    )
    .expect("captured Node.js MCP fixtures")
}

fn source_range_cases() -> Vec<McpSearchPresentationCase> {
    let mut cases = Vec::new();
    for trailing_newline in [false, true] {
        for whole_entity in [false, true] {
            let source = format!(
                "# Heading\nprefix needle{}",
                if trailing_newline { "\n" } else { "" }
            );
            let entity = serde_json::json!({
                "kind": "text", "start_line": 1,
                "end_line": if trailing_newline { 3 } else { 2 },
                "start_byte_offset": 0, "end_byte_offset": source.len(),
                "start_byte_column": 0,
                "end_byte_column": if trailing_newline { 0 } else { 13 }
            });
            let mut excerpt = entity.clone();
            excerpt["start_line"] = 2.into();
            excerpt["start_byte_offset"] = 17.into();
            excerpt["start_byte_column"] = 7.into();
            let mut value = fixtures().remove(0).result;
            value["items"].as_array_mut().expect("items").truncate(1);
            let item = &mut value["items"][0];
            item["rank"] = 1.into();
            item["relative_path"] = "sample.md".into();
            item["range"] = entity.clone();
            item["excerpt_range"] = excerpt.clone();
            item["content_range"] = if whole_entity { entity } else { excerpt };
            item["content"] = if whole_entity {
                source.clone()
            } else {
                source[17..].to_owned()
            }
            .into();
            let lines = if whole_entity {
                "1\t# Heading\n2\tprefix needle"
            } else {
                "2\tneedle"
            };
            let expected = format!(
                "freshness: fresh\n#1 matchedBy=fts+vector sample.md:1-2\nmatched: 2\nsource:\n{lines}{}",
                if trailing_newline { "\n3\t" } else { "" }
            );
            cases.push(McpSearchPresentationCase {
                id: format!(
                    "explicit-content-range-whole-{whole_entity}-newline-{trailing_newline}"
                ),
                result: value,
                expected_short: expected.clone(),
                expected_full: expected,
            });
        }
    }
    cases
}

#[test]
fn engine_content_range_controls_source_line_numbers() {
    for case in source_range_cases() {
        let result: ContextResult = serde_json::from_value(case.result).expect("context");
        for preview in [SearchPreview::Short, SearchPreview::Full] {
            assert_eq!(
                format_search_result(&result, preview),
                case.expected_full,
                "{} {preview:?}",
                case.id
            );
        }
    }
}

#[test]
fn content_coordinates_are_required_at_the_engine_boundary() {
    let mut value = source_range_cases().remove(0).result;
    value["items"][0]
        .as_object_mut()
        .expect("item")
        .remove("content_range");
    let error =
        serde_json::from_value::<ContextResult>(value).expect_err("missing content coordinates");
    assert!(error.to_string().contains("content_range"));
}

#[test]
fn captured_node_search_presentation() {
    for fixture in fixtures() {
        let result: ContextResult =
            serde_json::from_value(fixture.result).expect("context fixture");
        let original = result.clone();
        assert_eq!(
            format_search_result(&result, SearchPreview::Short),
            fixture.expected_short,
            "{} short",
            fixture.id
        );
        assert_eq!(
            format_search_result(&result, SearchPreview::Full),
            fixture.expected_full,
            "{} full",
            fixture.id
        );
        assert_eq!(
            result, original,
            "formatting must not mutate retrieved evidence"
        );
    }
}

#[test]
fn freshness_never_uses_provenance_as_a_status() {
    let mut result: ContextResult =
        serde_json::from_value(fixtures().remove(0).result).expect("fixture");
    for refresh in ["off", "scheduled", "failed"] {
        result.freshness = Some("served_from_current_index".to_owned());
        result.background_refresh = Some(refresh.to_owned());
        let text = format_search_result(&result, SearchPreview::Short);
        assert!(text.starts_with(&format!("freshness: possibly_stale\nresults: served_from_current_index\nbackground_refresh: {refresh}\n")));
        assert!(!text.contains("freshness: served_from_current_index"));
    }
    result.freshness = Some("fresh".to_owned());
    result.background_refresh = Some("idle".to_owned());
    let text = format_search_result(&result, SearchPreview::Short);
    assert!(text.starts_with("freshness: fresh\n#1"));
    assert!(!text.contains("background_refresh:"));
    result.freshness = None;
    result.background_refresh = None;
    result.items[0].status = zg_engine::api::context::result::ContextItemStatus::PossiblyStale;
    assert!(
        format_search_result(&result, SearchPreview::Short)
            .starts_with("freshness: possibly_stale\n")
    );
}

struct FixedSearch {
    result: Mutex<ContextResult>,
    requests: Mutex<Vec<serde_json::Value>>,
}

#[async_trait]
impl IndexOperationProvider for FixedSearch {
    async fn submit_index(
        &self,
        _options: IndexOptions,
        _wait: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        panic!("preview must not submit an index operation");
    }

    async fn drop_index(&self, _options: InfoOptions) -> Result<bool, EngineError> {
        panic!("preview must not drop an index");
    }

    async fn search(
        &self,
        _engine: &ZvecGrep,
        request: ContextOptions,
    ) -> Result<ContextResult, EngineError> {
        self.requests
            .lock()
            .expect("requests")
            .push(serde_json::to_value(request).expect("search request"));
        Ok(self.result.lock().expect("result").clone())
    }
}

#[tokio::test]
async fn public_tool_preview_matches_node_without_changing_retrieval() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let cases = fixtures().into_iter().chain(source_range_cases()).collect::<Vec<_>>();
        for toolset in [McpToolset::Agent, McpToolset::Full] {
            let backend = Arc::new(FixedSearch {
                result: Mutex::new(serde_json::from_value(cases[0].result.clone()).expect("fixture")),
                requests: Mutex::new(Vec::new()),
            });
            let server = ZvecGrepMcpServer::build_with_index_operations(Arc::new(ZvecGrep::new()), toolset, None, backend.clone());
            let tool = server.listed_tools().into_iter().find(|tool| tool.name == "zvec_grep_search").expect("search tool");
            assert_eq!(tool.input_schema["properties"]["preview"]["enum"], serde_json::json!(["short", "full"]));
            assert!(!tool.input_schema["required"].as_array().expect("required fields").contains(&serde_json::json!("preview")));
            assert!(tool.description.as_deref().expect("description").contains("preview: \"full\""));
            let (server_io, client_io) = tokio::io::duplex(4096);
            let task = tokio::spawn(async move { server.serve(server_io).await.expect("server").waiting().await });
            let client = ().serve(client_io).await.expect("client");
            for fixture in &cases {
                let original: ContextResult = serde_json::from_value(fixture.result.clone()).expect("fixture");
                *backend.result.lock().expect("result") = original.clone();
                backend.requests.lock().expect("requests").clear();
                for preview in [None, Some("short"), Some("full")] {
                    let mut args = serde_json::json!({"root": std::env::temp_dir(), "fts": "fixture", "autoUpdate": false, "limit": 2});
                    if let Some(preview) = preview { args["preview"] = preview.into(); }
                    let mut request = CallToolRequestParams::new("zvec_grep_search");
                    request.arguments = args.as_object().cloned();
                    let reply = client.call_tool(request).await.expect("tool result");
                    assert!(!reply.is_error.unwrap_or(false));
                    assert!(reply.structured_content.is_none());
                    assert_eq!(reply.content.len(), 1);
                    let ContentBlock::Text(content) = &reply.content[0] else { panic!("search returns text") };
                    let expected = if preview == Some("full") { &fixture.expected_full } else { &fixture.expected_short };
                    assert_eq!(&content.text, expected, "{toolset} {} {preview:?}", fixture.id);
                }
                let requests = backend.requests.lock().expect("requests");
                assert_eq!(requests.len(), 3);
                assert_eq!(requests[0], requests[1]);
                assert_eq!(requests[0], requests[2]);
                assert!(requests[0].get("preview").is_none());
                assert_eq!(*backend.result.lock().expect("result"), original);
            }
            let before = backend.requests.lock().expect("requests").len();
            let mut invalid = CallToolRequestParams::new("zvec_grep_search");
            invalid.arguments = serde_json::json!({"root": std::env::temp_dir(), "fts": "fixture", "preview": "none"}).as_object().cloned();
            let invalid = client.call_tool(invalid).await;
            assert!(invalid.is_err() || invalid.expect("error result").is_error == Some(true));
            assert_eq!(backend.requests.lock().expect("requests").len(), before);
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("server stop");
        }
    }).await.expect("tool tests terminate");
}
