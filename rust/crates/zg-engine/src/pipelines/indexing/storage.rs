//! Storage operations used by indexing, with a small seam for pipeline tests.

use std::path::PathBuf;

use crate::{
    EngineResult,
    domain::{Entity, FileId, FileRecord},
    storage::{IndexStore, types::IndexedFragment},
};

pub(crate) trait IndexStorage: Send + Sync {
    fn is_read_only(&self) -> bool;
    fn list_files(&self) -> EngineResult<Vec<FileRecord>>;
    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>>;
    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()>;
    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()>;
    fn delete_file(&self, file_id: FileId) -> EngineResult<()>;
    fn checkpoint(&self) -> EngineResult<()>;
}

impl IndexStorage for IndexStore {
    fn is_read_only(&self) -> bool {
        self.is_read_only()
    }

    fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        self.list_files()
    }

    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        self.resolve_file_ids(paths)
    }

    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        self.replace_file(file, entities, entries)
    }

    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
        self.mark_file_failed(file, error)
    }

    fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        self.delete_file(file_id)
    }

    fn checkpoint(&self) -> EngineResult<()> {
        self.checkpoint()
    }
}
