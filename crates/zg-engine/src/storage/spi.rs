//! Storage interfaces consumed by indexing, search, and workspace lifecycle services.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    EngineError,
    domain::{Entity, EntityFragment, EntityId, FileId, SourceFile, SymbolType},
    models::EmbeddingMetric,
};

pub(crate) type StorageResult<T> = Result<T, EngineError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceIndexEmbeddingSchema {
    pub provider: String,
    pub model: String,
    pub dimension: usize,
    pub metric: EmbeddingMetric,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceIndexStorageOptions {
    ReadOnly {
        storage_path: PathBuf,
    },
    ReadWrite {
        storage_path: PathBuf,
        embedding: WorkspaceIndexEmbeddingSchema,
    },
}

impl WorkspaceIndexStorageOptions {
    pub(crate) fn storage_path(&self) -> &Path {
        match self {
            Self::ReadOnly { storage_path } | Self::ReadWrite { storage_path, .. } => storage_path,
        }
    }

    pub(crate) const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileIndexStatus {
    pub indexed_epoch_ms: Option<u64>,
    pub entity_count: usize,
    pub token_count: Option<usize>,
    pub truncated_fragment_count: Option<usize>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredFile {
    pub source: SourceFile,
    pub index_status: Option<FileIndexStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredEntity {
    pub entity: Entity,
    pub file: StoredFile,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexedFragment {
    pub fragment: EntityFragment,
    pub vector: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileIndexDiagnostics {
    pub truncated_fragment_count: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StorageSearchFilter {
    pub file_ids: Option<Vec<FileId>>,
    pub entity_ids: Option<Vec<EntityId>>,
    pub symbol_names: Option<Vec<String>>,
    pub symbol_types: Option<Vec<SymbolType>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageSearchPath {
    Fts,
    Vector,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StorageSearchHit {
    pub fragment: EntityFragment,
    pub file: StoredFile,
    pub path: StorageSearchPath,
    pub score: f64,
}

/// Opens and manages concrete workspace-index storage instances.
pub(crate) trait WorkspaceIndexStorageFactory: Send + Sync {
    fn open(
        &self,
        options: WorkspaceIndexStorageOptions,
    ) -> StorageResult<Box<dyn WorkspaceIndexStorage>>;

    fn exists(&self, storage_path: &Path) -> StorageResult<bool>;

    fn delete(&self, storage_path: &Path) -> StorageResult<()>;
}

/// Persistence operations required by the indexing and indexed-search pipelines.
#[async_trait]
pub(crate) trait WorkspaceIndexStorage: Send + Sync {
    fn is_read_only(&self) -> bool;

    fn list_files(&self) -> StorageResult<Vec<StoredFile>>;

    fn get_entity(&self, entity_id: &EntityId) -> StorageResult<Option<StoredEntity>>;

    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>>;

    fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>>;

    fn replace_file(
        &self,
        file: &StoredFile,
        entries: &[IndexedFragment],
        diagnostics: Option<&FileIndexDiagnostics>,
    ) -> StorageResult<()>;

    fn mark_file_failed(&self, file: &StoredFile, error: &str) -> StorageResult<()>;

    fn delete_file(&self, file_id: &FileId) -> StorageResult<()>;

    async fn finalize_writes(&self) -> StorageResult<()>;

    fn close(&self) -> StorageResult<()>;
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
