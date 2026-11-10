//! Managed-rg presentation preserves file paths, all matches and requested context.

use rmcp::model::{CallToolResult, ContentBlock};
use std::collections::BTreeMap;
use zg_engine::api::{
    context::{
        ContextResult,
        result::{ContextCoverage, ContextItem, EmptyReason},
    },
    files::FilesResult,
};

pub(crate) fn files(reply: &FilesResult, limit: Option<usize>, null: bool) -> CallToolResult {
    let count = limit.unwrap_or(reply.paths.len()).min(reply.paths.len());
    let mut paths = String::new();
    for path in &reply.paths[..count] {
        paths.push_str(&path.to_string_lossy());
        paths.push(if null { '\0' } else { '\n' });
    }
    let mut content = vec![ContentBlock::text(paths)];
    if count < reply.paths.len() {
        content.push(ContentBlock::text("More files were omitted by the explicit output bound. Remove or increase the trailing `head` bound to see them."));
    }
    content.extend(
        reply
            .errors
            .iter()
            .map(|error| ContentBlock::text(format!("Error: {error}"))),
    );
    if reply.errors.is_empty() {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    }
}

pub(crate) fn format(reply: &ContextResult) -> String {
    let mut lines = Vec::new();
    let mut items: Vec<_> = reply.items.iter().collect();
    items.sort_by_key(|item| item.rank);
    let mut files: Vec<Vec<&ContextItem>> = Vec::new();
    for item in items {
        if let Some(group) = files
            .iter_mut()
            .find(|group| group[0].relative_path == item.relative_path)
        {
            group.push(item);
        } else {
            files.push(vec![item]);
        }
    }
    if files.is_empty() {
        lines.push(
            if reply.diagnostics.empty_reason == Some(EmptyReason::NoSearchableFiles) {
                "No searchable files."
            } else {
                "No matches."
            }
            .to_owned(),
        );
    }
    for file in files {
        lines.push(
            zg_cli::output_path(&file[0].relative_path)
                .display()
                .to_string(),
        );
        lines.extend(source(&file).into_iter().map(|text| format!("  {text}")));
    }
    if reply.coverage == ContextCoverage::RgTruncated {
        lines.push("\nMore matches were omitted by the explicit output bound. Remove or increase the trailing `head` bound to see them.".to_owned());
    }
    lines.join("\n")
}

fn source(items: &[&ContextItem]) -> Vec<String> {
    let mut numbered: BTreeMap<usize, (bool, String)> = BTreeMap::new();
    let mut unnumbered = Vec::new();
    for item in items {
        let matched = item.excerpt_range.as_ref().unwrap_or(&item.range);
        // A trailing newline is not an extra source line in rg's line-oriented output.
        for (offset, text) in item.content.lines().enumerate() {
            if let Some(first) = item.content_range.start_line() {
                let line = first + offset;
                let hit = matched.start_line().is_none_or(|first| line >= first)
                    && matched.last_line().is_none_or(|last| line <= last);
                numbered
                    .entry(line)
                    .and_modify(|(previous_hit, _)| *previous_hit |= hit)
                    .or_insert((hit, text.to_owned()));
            } else {
                unnumbered.push(format!("\t{text}"));
            }
        }
    }
    numbered
        .into_iter()
        .map(|(line, (hit, text))| format!("{line}{}\t{text}", if hit { ':' } else { '-' }))
        .chain(unnumbered)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_rg_tool_returns_plain_code_and_markdown_matches() {
        use std::sync::Arc;

        use rmcp::{ServiceExt, model::CallToolRequestParams};

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let directory = tempfile::tempdir().expect("source directory");
            std::fs::write(directory.path().join("notes.md"), "# Orchard\nneedle\nafter\n")
                .expect("Markdown source");
            std::fs::write(
                directory.path().join("source.rs"),
                "pub fn orchard() {\n    let fruit = \"needle\";\n}\n",
            )
            .expect("code source");
            let engine = Arc::new(zg_engine::ZvecGrep::new());
            let server = crate::ZvecGrepMcpServer::build(
                Arc::clone(&engine),
                crate::McpToolset::Full,
                None,
            );
            let (server_io, client_io) = tokio::io::duplex(4096);
            let task = tokio::spawn(async move {
                server.serve(server_io).await.expect("server").waiting().await
            });
            let client = ().serve(client_io).await.expect("client");
            for command in [
                "rg -L needle .",
                "rg --follow needle .",
                "rg --files -L",
                "rg --files --follow",
            ] {
                let mut request = CallToolRequestParams::new("zvec_grep_rg");
                request.arguments = serde_json::json!({
                    "root": directory.path(),
                    "command": command
                })
                .as_object()
                .cloned();
                let error = client
                    .call_tool(request)
                    .await
                    .expect_err("MCP rg rejects symlink following");
                assert!(error.to_string().contains("--follow"), "{error}");
            }
            let mut files_request = CallToolRequestParams::new("zvec_grep_rg");
            files_request.arguments = serde_json::json!({
                "root": directory.path(),
                "command": "rg --files"
            })
            .as_object()
            .cloned();
            let files = client.call_tool(files_request).await.expect("file listing");
            assert!(!files.is_error.unwrap_or(false));
            let rmcp::model::ContentBlock::Text(content) = &files.content[0] else {
                panic!("file listing returns text")
            };
            assert_eq!(content.text, "notes.md\nsource.rs\n");
            let mut request = CallToolRequestParams::new("zvec_grep_rg");
            request.arguments = serde_json::json!({
                "root": directory.path(),
                "command": "rg -n -C 1 needle ."
            })
            .as_object()
            .cloned();
            let reply = client.call_tool(request).await.expect("rg tool result");
            assert!(!reply.is_error.unwrap_or(false));
            assert!(reply.structured_content.is_none());
            assert_eq!(reply.content.len(), 1);
            let rmcp::model::ContentBlock::Text(content) = &reply.content[0] else {
                panic!("rg returns text")
            };
            assert_eq!(
                content.text,
                "notes.md\n  1-\t# Orchard\n  2:\tneedle\n  3-\tafter\nsource.rs\n  1-\tpub fn orchard() {\n  2:\t    let fruit = \"needle\";\n  3-\t}"
            );
            assert!(!directory.path().join(".zvec-grep").exists());
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("server stop");
            engine.close();
        })
        .await
        .expect("rg protocol test terminates");
    }

    #[test]
    fn presents_structured_matches_as_plain_source_lines() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../compat/mcp/rg-presentation.json"))
                .expect("fixtures");
        let case = fixtures["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|case| case["id"] == "symbol")
            .expect("structured matches");
        for markdown in [false, true] {
            let mut value = case["result"].clone();
            if markdown {
                value["items"][0]["container"]["metadata"] = serde_json::json!({
                    "kind": "markdown", "heading": "Orchard", "level": 1, "scope": null
                });
            }
            let result: ContextResult = serde_json::from_value(value).expect("context");
            assert_eq!(
                format(&result),
                "src/sample.ts\n  10-\tbefore\n  11:\tneedle\n  12-\tafter"
            );
        }
    }

    #[test]
    fn captured_rg_presentation_uses_plain_matches() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../compat/mcp/rg-presentation.json"))
                .expect("fixtures");
        for case in fixtures["cases"].as_array().expect("cases") {
            let result: ContextResult =
                serde_json::from_value(case["result"].clone()).expect("context");
            // Plain rg results retain source coordinates without the Node symbol headers.
            let expected = match case["id"].as_str() {
                Some("symbol") => "src/sample.ts\n  10-\tbefore\n  11:\tneedle\n  12-\tafter",
                Some("symbol-overlap") => {
                    "src/sample.ts\n  10-\tbefore\n  11:\tneedle\n  12:\tafter"
                }
                _ => case["expected"].as_str().expect("expected"),
            };
            assert_eq!(format(&result), expected, "{}", case["id"]);
        }
    }

    #[test]
    fn preserves_long_lines_all_context_and_explicit_truncation() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/mcp/search-presentation.json");
        let mut result: ContextResult = serde_json::from_value(
            zg_testkit::load_mcp_search_cases(&path)
                .expect("fixtures")
                .remove(0)
                .result,
        )
        .expect("result");
        result.items.truncate(1);
        result.items[0].container = None;
        result.items[0].content = (1..=20)
            .map(|line| format!("line-{line}-{}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        result.coverage = ContextCoverage::RgTruncated;
        let text = format(&result);
        assert!(text.starts_with("src/sample.ts\n"));
        assert!(text.contains(&format!("line-20-{}", "x".repeat(200))));
        assert!(text.contains("More matches were omitted"));
        result.coverage = ContextCoverage::RgExhaustive;
        assert!(!format(&result).contains("More matches were omitted"));
    }
}
