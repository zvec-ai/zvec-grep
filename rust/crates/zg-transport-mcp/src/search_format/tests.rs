use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock},
};
use zg_cli::{
    ColorMode, OutputOptions, PreviewMode, write_context_status, write_context_with_options,
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

fn cli_output(result: &ContextResult, preview: PreviewMode) -> String {
    let mut bytes = Vec::new();
    write_context_with_options(
        &mut bytes,
        result,
        OutputOptions {
            preview,
            color: ColorMode::Never,
            ..OutputOptions::default()
        },
        false,
    )
    .expect("CLI search output");
    let mut diagnostics = Vec::new();
    write_context_status(&mut diagnostics, result).expect("CLI search diagnostics");
    let body = String::from_utf8(bytes).expect("UTF-8");
    if diagnostics.is_empty() {
        format!("freshness: fresh\n{body}")
    } else {
        let diagnostics = String::from_utf8(diagnostics).expect("UTF-8");
        format!("freshness: possibly_stale\n{diagnostics}\n{body}")
    }
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
                "  1: # Heading\n  2: prefix needle"
            } else {
                "  2: needle"
            };
            let expected = format!("freshness: fresh\n#1 sample.md:1-2\n{lines}\n");
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

fn generated_outline_cases() -> Vec<McpSearchPresentationCase> {
    [true, false]
        .into_iter()
        .map(|has_signature| {
            let mut value = fixtures().remove(0).result;
            value["items"].as_array_mut().expect("items").truncate(1);
            let item = &mut value["items"][0];
            item["rank"] = 1.into();
            item["outline"] =
                "function resolveContext() {\n\ncalls: first, second".into();
            item["metadata"] = serde_json::json!({
                "kind": "code", "symbol_type": "function",
                "symbol_name": "resolveContext", "scope": "Workspace"
            });
            let source = if has_signature {
                "function resolveContext() { first(); second(); }"
            } else {
                "  first(); second();"
            };
            item["content"] = source.into();
            let expected = format!(
                "freshness: fresh\n#1 src/sample.ts:10-33\nsymbol: [function] resolveContext scope: Workspace\n  10: {source}\n"
            );
            McpSearchPresentationCase {
                id: format!("generated-outline-source-signature-{has_signature}"),
                result: value,
                expected_short: expected.clone(),
                expected_full: expected,
            }
        })
        .collect()
}

fn grouped_cases() -> Vec<McpSearchPresentationCase> {
    let mut case = fixtures().remove(1);
    case.id = "separate-query-group-results".into();
    let items = case.result["items"].as_array().expect("items").clone();
    case.result["items"]
        .as_array_mut()
        .expect("items")
        .truncate(1);
    case.result["group_results"] = serde_json::json!([
        {"id": "Q1", "query": "call chain", "role": "primary", "items": [items[0]]},
        {"id": "Q2", "query": "Design", "role": "supplemental", "items": [items[1]]},
        {"id": "Q3", "query": "missing", "role": "supplemental", "items": []}
    ]);
    vec![case]
}

fn global_staleness_cases() -> Vec<McpSearchPresentationCase> {
    [true, false]
        .into_iter()
        .flat_map(|empty| {
            [false, true].into_iter().map(move |indexing| {
                let mut case = fixtures().remove(0);
                case.id = format!("global-staleness-empty-{empty}-indexing-{indexing}");
                case.result["freshness"] = if indexing { "fresh" } else { "possibly_stale" }.into();
                case.result["background_refresh"] = serde_json::Value::Null;
                case.result["indexing"] = if indexing {
                    serde_json::json!({"state": "running", "completed": 2, "total": 3})
                } else {
                    serde_json::Value::Null
                };
                if empty {
                    case.result["items"].as_array_mut().expect("items").clear();
                }
                case
            })
        })
        .collect()
}

#[test]
fn global_staleness_is_visible_without_stale_hits() {
    for case in global_staleness_cases() {
        let result: ContextResult = serde_json::from_value(case.result).expect("context");
        assert!(!result.has_stale_items());
        let diagnostics = if result.indexing.is_some() {
            "status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: running (2/3)\n"
        } else {
            "status: possibly_stale\n"
        };
        for preview in [SearchPreview::Short, SearchPreview::Full] {
            let text = format_search_result(&result, preview);
            assert!(
                text.starts_with(&format!("freshness: possibly_stale\n{diagnostics}\n")),
                "{} {preview:?}: {text}",
                case.id
            );
            if result.items.is_empty() {
                assert_eq!(
                    text,
                    format!("freshness: possibly_stale\n{diagnostics}\nNo matches.\n")
                );
            }
        }
    }
}

#[test]
fn source_previews_hide_generated_call_outlines_and_preserve_symbols() {
    for case in generated_outline_cases() {
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
fn supported_previews_match_cli_without_extra_fields() {
    for fixture in fixtures()
        .into_iter()
        .chain(grouped_cases())
        .chain(global_staleness_cases())
    {
        let result: ContextResult =
            serde_json::from_value(fixture.result).expect("context fixture");
        let original = result.clone();
        assert_eq!(
            format_search_result(&result, SearchPreview::Short),
            cli_output(&result, PreviewMode::Short),
            "{} short",
            fixture.id
        );
        assert_eq!(
            format_search_result(&result, SearchPreview::Full),
            cli_output(&result, PreviewMode::Full),
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
fn search_metadata_does_not_add_output_fields() {
    let mut result: ContextResult =
        serde_json::from_value(fixtures().remove(0).result).expect("fixture");
    let expected = cli_output(&result, PreviewMode::Short);
    for refresh in ["off", "idle", "scheduled", "failed"] {
        result.freshness = Some("served_from_current_index".to_owned());
        result.background_refresh = Some(refresh.to_owned());
        let text = format_search_result(&result, SearchPreview::Short);
        assert_eq!(text, expected, "{refresh}");
    }
    result.freshness = Some("fresh".to_owned());
    result.background_refresh = Some("idle".to_owned());
    let text = format_search_result(&result, SearchPreview::Short);
    assert_eq!(text, expected);
    result.freshness = Some("possibly_stale".to_owned());
    assert_eq!(
        format_search_result(&result, SearchPreview::Short),
        format!(
            "freshness: possibly_stale\nstatus: possibly_stale\nresults: served_from_current_index\nbackground_refresh: idle\n\n{}",
            expected
                .strip_prefix("freshness: fresh\n")
                .expect("fresh header")
        )
    );
    result.freshness = None;
    result.background_refresh = None;
    result.items[0].status = zg_engine::api::context::result::ContextItemStatus::PossiblyStale;
    let text = format_search_result(&result, SearchPreview::Short);
    assert_eq!(text, cli_output(&result, PreviewMode::Short));
    assert!(text.contains("status: possibly_stale"), "{text}");
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
async fn public_tool_preview_matches_cli_without_changing_retrieval() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let cases = fixtures().into_iter().chain(source_range_cases()).chain(generated_outline_cases()).chain(grouped_cases()).chain(global_staleness_cases()).collect::<Vec<_>>();
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
                    let expected = cli_output(&original, if preview == Some("full") { PreviewMode::Full } else { PreviewMode::Short });
                    assert_eq!(content.text, expected, "{toolset} {} {preview:?}", fixture.id);
                }
                let requests = backend.requests.lock().expect("requests");
                assert_eq!(requests.len(), 3);
                assert_eq!(requests[0], requests[1]);
                assert_eq!(requests[0], requests[2]);
                assert!(requests[0].get("preview").is_none());
                assert_eq!(*backend.result.lock().expect("result"), original);
            }
            let before = backend.requests.lock().expect("requests").len();
            for preview in ["none", "detail"] {
                let mut invalid = CallToolRequestParams::new("zvec_grep_search");
                invalid.arguments = serde_json::json!({"root": std::env::temp_dir(), "fts": "fixture", "preview": preview}).as_object().cloned();
                let invalid = client.call_tool(invalid).await;
                assert!(invalid.is_err() || invalid.expect("error result").is_error == Some(true));
            }
            assert_eq!(backend.requests.lock().expect("requests").len(), before);
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("server stop");
        }
    }).await.expect("tool tests terminate");
}
