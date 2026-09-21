use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::TextRange;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LexicalSearchRequest {
    pub root: Option<PathBuf>,
    pub patterns: Vec<String>,
    pub pattern_files: Vec<PathBuf>,
    pub paths: Vec<PathBuf>,
    pub limit: Option<usize>,
    pub options: LexicalOptions,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct LexicalOptions {
    pub matching: crate::api::context::options::RgOptions,
    pub hidden: bool,
    pub no_ignore: bool,
    pub follow: bool,
    pub globs: Vec<String>,
    pub insensitive_globs: Vec<String>,
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
    pub ignore_files: Vec<PathBuf>,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,
    pub modified_after_epoch_ms: Option<u64>,
    pub modified_before_epoch_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LexicalMatch {
    pub rank: usize,
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub range: TextRange,
    pub excerpt_range: Option<TextRange>,
    /// Source coordinates of the returned lines, which may extend beyond the match.
    pub content_range: TextRange,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LexicalDiagnostics {
    pub backend: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub ignored_directories: Vec<PathBuf>,
    pub missing_paths: Vec<PathBuf>,
    pub searched_paths: Vec<PathBuf>,
    pub limit: Option<usize>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LexicalSearchReply {
    pub root: PathBuf,
    pub coverage: LexicalCoverage,
    pub matches: Vec<LexicalMatch>,
    pub diagnostics: LexicalDiagnostics,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalCoverage {
    Exhaustive,
    Truncated,
}
