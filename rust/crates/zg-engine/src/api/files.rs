//! Metadata-only file discovery for managed ripgrep.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::context::options::RgGlob;

/// File selection without patterns, content reads, or an index.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct FilesOptions {
    /// Working directory used to resolve relative input paths and globs.
    pub root: Option<PathBuf>,
    /// Files or directories to enumerate. Empty selects the working directory.
    pub paths: Vec<PathBuf>,
    pub globs: Vec<RgGlob>,
    pub glob_case_insensitive: bool,
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
    pub hidden: bool,
    pub no_ignore: bool,
    pub no_ignore_dot: bool,
    pub no_ignore_files: bool,
    pub no_ignore_global: bool,
    pub no_ignore_parent: bool,
    pub no_ignore_vcs: bool,
    pub ignore_files: Vec<PathBuf>,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,
    pub follow: bool,
    pub one_file_system: bool,
    pub modified_after_epoch_ms: Option<u64>,
    pub modified_before_epoch_ms: Option<u64>,
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// All discovered paths, with traversal errors retained alongside partial results.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilesResult {
    pub paths: Vec<PathBuf>,
    pub errors: Vec<String>,
}
