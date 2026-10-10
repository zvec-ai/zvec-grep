//! Read complete indexed content using a reference returned by a search.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::domain::{Content, ContentKind, FileFormat, ImageContent};

/// An indexed entity within a workspace's current storage generation.
///
/// Pass this value from a search result to [`crate::ZvecGrep::read_content`].
/// Rebuilding the index invalidates its references; incremental updates can
/// remove or replace individual entities. References do not retain historical data.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentRef {
    pub generation: String,
    pub entity_id: String,
}

/// Reads one entity's complete content without refreshing the index.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadContentOptions {
    /// Workspace root. `None` uses the process working directory.
    pub root: Option<PathBuf>,
    pub reference: ContentRef,
    #[serde(default)]
    pub lock_timeout_ms: Option<u64>,
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

impl ReadContentOptions {
    #[must_use]
    pub fn new(reference: ContentRef) -> Self {
        Self {
            root: None,
            reference,
            lock_timeout_ms: None,
            signal: None,
        }
    }
}

/// Complete content stored in the index and its absolute source path.
///
/// The source file is not read and may have changed since indexing. Images
/// retain their encoded bytes; their JSON representation uses base64.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentResult {
    pub path: PathBuf,
    pub content: Content,
}
