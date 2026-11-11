//! Storage operations used by indexing, with a small seam for pipeline tests.

use std::path::PathBuf;

use crate::{
    EngineResult,
    domain::{Entity, FileGraphResult, FileId, FileRecord},
    storage::{IndexStore, types::IndexedFragment},
};

pub(crate) trait IndexStorage: Send + Sync {
    /// Whether writes may carry a walk-time graph.
    fn graph_enabled(&self) -> bool;
    fn is_read_only(&self) -> bool;
    fn list_files(&self) -> EngineResult<Vec<FileRecord>>;
    fn count_fragments(&self) -> EngineResult<u64>;
    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>>;
    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        graph: &FileGraphResult,
    ) -> EngineResult<()>;
    fn update_file_snapshot(&self, file: &FileRecord) -> EngineResult<()>;
    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()>;
    fn delete_file(&self, file_id: FileId) -> EngineResult<()>;
    fn finalize_writes(&self) -> EngineResult<()>;
}

impl IndexStorage for IndexStore {
    fn graph_enabled(&self) -> bool {
        self.graph_enabled()
    }

    fn is_read_only(&self) -> bool {
        self.is_read_only()
    }

    fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        self.list_files()
    }

    fn count_fragments(&self) -> EngineResult<u64> {
        self.count_fragments()
    }

    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        self.resolve_file_ids(paths)
    }

    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        graph: &FileGraphResult,
    ) -> EngineResult<()> {
        self.replace_file_with_graph(file, entities, entries, graph)
    }

    fn update_file_snapshot(&self, file: &FileRecord) -> EngineResult<()> {
        self.update_file_snapshot(file)
    }

    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
        self.mark_file_failed(file, error)
    }

    fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        self.delete_file(file_id)
    }

    fn finalize_writes(&self) -> EngineResult<()> {
        self.finalize_writes()
    }
}
