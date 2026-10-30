//! Public indexed-search presentation, shared with the CLI without changing retrieval.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zg_cli::{ColorMode, OutputOptions, PreviewMode, write_context_with_options};
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
    let mut bytes = Vec::new();
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

#[cfg(test)]
mod tests;
