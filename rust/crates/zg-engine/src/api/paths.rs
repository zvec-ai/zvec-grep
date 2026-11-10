//! Current file-path lookup without an index or model.

use std::path::PathBuf;

use super::context::options::QueryFilter;

#[derive(Clone, Debug, Default)]
pub struct PathsOptions {
    /// Exact file name, workspace-relative path suffix, or absolute path.
    pub query: String,
    /// Finds the nearest owning workspace; absent uses the current directory.
    pub root: Option<PathBuf>,
    /// Maximum paths to return, defaulting to 10.
    pub limit: Option<usize>,
    /// Intersects the workspace's saved scanning rules.
    pub filter: QueryFilter,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathsResult {
    pub root: PathBuf,
    /// Workspace-relative paths, with exact paths before suffix matches.
    pub paths: Vec<PathBuf>,
    pub truncated: bool,
}

/// Current evidence for a path-shaped query, without indexed or vector retrieval.
#[derive(Clone, Debug, PartialEq)]
pub enum PathLookupResult {
    Files(PathsResult),
    Text(Box<super::context::ContextResult>),
}
