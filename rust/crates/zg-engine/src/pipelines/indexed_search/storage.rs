//! Read operations used by indexed search, independently of indexing writes.

use std::path::PathBuf;

use crate::{
    EngineResult,
    domain::FileId,
    storage::{
        IndexStore,
        types::{StorageSearchFilter, StorageSearchHit, StoredFileAttributes, StoredSearchData},
    },
};

pub(crate) trait SearchStorage: Send + Sync {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>>;
    fn list_file_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>>;
    fn has_non_unicode_file_names(&self) -> EngineResult<bool>;
    fn load_search_hits(&self, hits: &[StorageSearchHit]) -> EngineResult<StoredSearchData>;
    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>>;
    fn search_vector(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>>;
}

impl SearchStorage for IndexStore {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        self.list_file_paths()
    }

    fn list_file_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>> {
        self.list_file_attributes()
    }

    fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        self.has_non_unicode_file_names()
    }

    fn load_search_hits(&self, hits: &[StorageSearchHit]) -> EngineResult<StoredSearchData> {
        self.load_search_hits(hits)
    }

    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.search_fts(query, limit, filter)
    }

    fn search_vector(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.search_vector(model, vector, limit, filter)
    }
}
