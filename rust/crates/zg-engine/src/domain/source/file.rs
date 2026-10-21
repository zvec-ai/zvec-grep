use std::fmt;

use crate::{EngineError, EngineResult};

use super::path::SourcePath;

/// A file relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileRecord {
    id: FileId,
    relative_path: SourcePath,
    snapshot: FileSnapshot,
    index_status: FileIndexStatus,
}

impl FileRecord {
    pub(crate) fn new(
        id: FileId,
        relative_path: SourcePath,
        snapshot: FileSnapshot,
        index_status: FileIndexStatus,
    ) -> EngineResult<Self> {
        Self::validate_state(&relative_path, &snapshot, &index_status)?;

        Ok(Self {
            id,
            relative_path,
            snapshot,
            index_status,
        })
    }

    pub(crate) fn id(&self) -> FileId {
        self.id
    }

    pub(crate) fn relative_path(&self) -> &SourcePath {
        &self.relative_path
    }

    pub(crate) fn snapshot(&self) -> &FileSnapshot {
        &self.snapshot
    }

    pub(crate) fn index_status(&self) -> &FileIndexStatus {
        &self.index_status
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: FileSnapshot) -> EngineResult<()> {
        Self::validate_state(&self.relative_path, &snapshot, &self.index_status)?;

        self.snapshot = snapshot;
        Ok(())
    }

    pub(crate) fn set_index_status(&mut self, index_status: FileIndexStatus) -> EngineResult<()> {
        Self::validate_state(&self.relative_path, &self.snapshot, &index_status)?;

        self.index_status = index_status;
        Ok(())
    }

    fn validate_state(
        relative_path: &SourcePath,
        snapshot: &FileSnapshot,
        index_status: &FileIndexStatus,
    ) -> EngineResult<()> {
        if index_status.is_indexed() && snapshot.content_hash.is_none() {
            return Err(EngineError::invalid_argument(format!(
                "indexed files must have a content hash: {}",
                relative_path.display()
            )));
        }
        Ok(())
    }
}

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
pub(crate) struct FileSnapshot {
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub content_hash: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileIndexStatus {
    NotIndexed,
    Indexed {
        indexed_epoch_ms: u64,
        entity_count: u64,
    },
    Deleting,
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
            Self::NotIndexed | Self::Deleting | Self::Failed { .. } => None,
        }
    }

    pub(crate) const fn entity_count(&self) -> u64 {
        match self {
            Self::Indexed { entity_count, .. } => *entity_count,
            Self::NotIndexed | Self::Deleting | Self::Failed { .. } => 0,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error } => Some(error),
            Self::NotIndexed | Self::Indexed { .. } | Self::Deleting => None,
        }
    }
}
