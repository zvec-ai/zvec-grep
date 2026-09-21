//! Data exchanged with index storage operations.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::domain::{
    Entity, EntityFragment, EntityId, FileId, FileRecord, FragmentId, SourcePath, SymbolType,
    model::EmbeddingModelInfo,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceIndexStorageOptions {
    ReadOnly {
        storage_path: PathBuf,
    },
    ReadWrite {
        storage_path: PathBuf,
        embeddings: Vec<EmbeddingModelInfo>,
    },
}

impl WorkspaceIndexStorageOptions {
    pub(crate) fn storage_path(&self) -> &Path {
        match self {
            Self::ReadOnly { storage_path, .. } | Self::ReadWrite { storage_path, .. } => {
                storage_path
            }
        }
    }

    pub(crate) const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredEntity {
    pub entity: Entity,
    pub file: FileRecord,
}

/// File attributes needed by query filters, without content hashes or index status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredFileAttributes {
    pub id: FileId,
    pub relative_path: PathBuf,
    pub modified_epoch_ms: Option<u64>,
}

impl From<&FileRecord> for StoredFileAttributes {
    fn from(file: &FileRecord) -> Self {
        Self {
            id: file.id,
            relative_path: file.relative_path.to_path_buf(),
            modified_epoch_ms: file.snapshot.modified_epoch_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexedFragment {
    pub entity_id: EntityId,
    pub fragment_id: FragmentId,
    /// Unique configured embedding model reference (provider/name).
    pub model: String,
    pub vector: Vec<f32>,
    pub fts_text: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StorageSearchFilter {
    pub path: Option<StoragePathFilter>,
    pub file_ids: Option<Vec<FileId>>,
    pub entity_ids: Option<Vec<EntityId>>,
    pub symbol_names: Option<Vec<String>>,
    pub symbol_types: Option<Vec<SymbolType>>,
}

/// Boolean predicates over metadata projected into each model collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoragePathFilter {
    All,
    None,
    Directory(SourcePath),
    FileNameExact(String),
    FileNamePrefix(String),
    FileNameSuffix(String),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageSearchPath {
    Fts,
    Vector,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StorageSearchHit {
    pub document_id: String,
    pub entity_id: EntityId,
    pub file_id: FileId,
    pub path: StorageSearchPath,
    pub score: f64,
}

#[derive(Default)]
pub(crate) struct StoredSearchData {
    pub entities: HashMap<EntityId, StoredEntity>,
    pub fragments: HashMap<String, EntityFragment>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::WorkspaceIndexStorageOptions;

    #[test]
    fn storage_options_keep_mode_and_path_together() {
        let path = PathBuf::from("workspace-index");
        let options = WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.clone(),
        };

        assert!(options.is_read_only());
        assert_eq!(options.storage_path(), path);
    }
}
