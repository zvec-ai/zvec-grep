//! Public indexed-search presentation with CLI content and explicit MCP freshness.

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zg_cli::{
    ColorMode, OutputOptions, PreviewMode, write_context_status, write_context_with_options,
};
use zg_engine::api::context::ContextResult;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum SearchPreview {
    #[default]
    Short,
    Full,
}

pub(crate) fn format_search_result(reply: &ContextResult, preview: SearchPreview) -> String {
    // Global diagnostics precede the body so they describe the entire search.
    let mut diagnostics = Vec::new();
    write_context_status(&mut diagnostics, reply)
        .expect("writing search diagnostics to a byte buffer cannot fail");
    let freshness = if diagnostics.is_empty() {
        "fresh"
    } else {
        "possibly_stale"
    };
    let mut bytes = format!("freshness: {freshness}\n").into_bytes();
    if !diagnostics.is_empty() {
        bytes.extend(diagnostics);
        bytes.push(b'\n');
    }
    write_context_with_options(
        &mut bytes,
        reply,
        OutputOptions {
            preview: match preview {
                SearchPreview::Short => PreviewMode::Short,
                SearchPreview::Full => PreviewMode::Full,
            },
            color: ColorMode::Never,
            ..OutputOptions::default()
        },
        false,
    )
    .expect("writing search output to a byte buffer cannot fail");
    String::from_utf8(bytes).expect("search output is UTF-8")
}

pub(crate) fn search_result(reply: &ContextResult, preview: SearchPreview) -> CallToolResult {
    let mut text = format_search_result(reply, preview);
    for error in &reply.diagnostics.graph_errors {
        use std::fmt::Write;
        writeln!(text, "\nCall graph expansion failed for {error}").expect("writing to a string");
    }
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    if !reply.diagnostics.search_trace.is_empty() {
        result.structured_content = Some(serde_json::json!({
            "searchTrace": reply.diagnostics.search_trace,
            "timings": reply.diagnostics.timings,
        }));
    }
    result
}

#[cfg(test)]
mod tests;
