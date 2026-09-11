use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::LineColumnRange;

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
    pub fixed_strings: bool,
    pub ignore_case: bool,
    pub word_regexp: bool,
    pub before_context: usize,
    pub after_context: usize,
    pub hidden: bool,
    pub no_ignore: bool,
    pub follow: bool,
    pub globs: Vec<String>,
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
    pub range: LineColumnRange,
    pub excerpt_range: Option<LineColumnRange>,
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
