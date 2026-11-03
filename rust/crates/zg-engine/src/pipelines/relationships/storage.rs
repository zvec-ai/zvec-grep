//! Storage reads used by the relationship pipeline; no matching or ranking policy.

use std::path::PathBuf;

use crate::{
    EngineResult,
    domain::FileId,
    storage::{IndexStore, ReadView},
};

/// The pipeline chooses which reads must share a snapshot; each call releases the lock.
pub(super) trait RelationshipStorage {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>>;
    fn has_non_unicode_file_names(&self) -> EngineResult<bool>;
    fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&ReadView<'_>) -> EngineResult<T>,
    ) -> EngineResult<T>;
}

impl RelationshipStorage for IndexStore {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        self.list_file_paths()
    }
    fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        self.has_non_unicode_file_names()
    }
    fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&ReadView<'_>) -> EngineResult<T>,
    ) -> EngineResult<T> {
        self.read_snapshot(operation)
    }
}
