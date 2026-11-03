//! Storage reads used by the relationship pipeline; no matching or ranking policy.

use std::{collections::HashMap, path::PathBuf};

use crate::{
    EngineResult,
    domain::{CodeMetadata, Entity, EntityId, FileEdge, FileId, FileRecord, GraphEdgeKind},
    storage::{
        IndexStore, ReadView,
        graph::Direction,
        types::{StorageSearchFilter, StorageSearchHit},
    },
};

/// The pipeline chooses which reads must share a snapshot; each call releases the lock.
pub(super) trait RelationshipStorage {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>>;
    fn has_non_unicode_file_names(&self) -> EngineResult<bool>;
    fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&dyn RelationshipRead) -> EngineResult<T>,
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
        operation: impl FnOnce(&dyn RelationshipRead) -> EngineResult<T>,
    ) -> EngineResult<T> {
        self.read_snapshot(|view| operation(view))
    }
}

pub(super) trait RelationshipRead {
    fn find_symbol_entity_ids(
        &self,
        name: &str,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<EntityId>>;
    fn fetch_code_metadata(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, (FileId, CodeMetadata)>>;
    fn fetch_entities(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, Entity>>;
    fn fetch_entity_file_ids(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, FileId>>;
    fn fetch_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>>;
    fn ensure_graph(&self) -> EngineResult<()>;
    fn graph_neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> EngineResult<Vec<FileEdge>>;
    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>>;
}

impl RelationshipRead for ReadView<'_> {
    fn find_symbol_entity_ids(
        &self,
        name: &str,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<EntityId>> {
        self.find_symbol_entity_ids(name, filter)
    }
    fn fetch_code_metadata(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, (FileId, CodeMetadata)>> {
        self.fetch_code_metadata(ids)
    }
    fn fetch_entities(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, Entity>> {
        self.fetch_entities(ids)
    }
    fn fetch_entity_file_ids(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, FileId>> {
        self.fetch_entity_file_ids(ids)
    }
    fn fetch_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        self.fetch_files(ids)
    }
    fn ensure_graph(&self) -> EngineResult<()> {
        self.ensure_graph()
    }
    fn graph_neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> EngineResult<Vec<FileEdge>> {
        self.graph_neighborhood(id, direction, kinds)
    }
    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.search_fts(query, limit, filter)
    }
}
