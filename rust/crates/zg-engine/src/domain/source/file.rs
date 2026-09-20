use std::fmt;

use crate::{EngineError, EngineResult};

use super::path::SourcePath;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FileId(u32);

impl FileId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileIndexStatus {
    NotIndexed,
    Indexed {
        indexed_epoch_ms: u64,
        entity_count: u64,
    },
    Failed {
        error: String,
    },
}

impl FileIndexStatus {
    pub(crate) const fn is_indexed(&self) -> bool {
        self.indexed_epoch_ms().is_some()
    }

    pub(crate) const fn indexed_epoch_ms(&self) -> Option<u64> {
        match self {
            Self::Indexed {
                indexed_epoch_ms, ..
            } => Some(*indexed_epoch_ms),
            Self::NotIndexed | Self::Failed { .. } => None,
        }
    }

    pub(crate) const fn entity_count(&self) -> u64 {
        match self {
            Self::Indexed { entity_count, .. } => *entity_count,
            Self::NotIndexed | Self::Failed { .. } => 0,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error } => Some(error),
            Self::NotIndexed | Self::Indexed { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub content_hash: Option<String>,
}

/// A file relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileRecord {
    pub id: FileId,
    pub relative_path: SourcePath,
    pub snapshot: FileSnapshot,
    pub index_status: FileIndexStatus,
}

impl FileRecord {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.index_status.is_indexed() && self.snapshot.content_hash.is_none() {
            return Err(EngineError::invalid_argument(format!(
                "indexed files must have a content hash: {}",
                self.relative_path.display()
            )));
        }
        Ok(())
    }
}
