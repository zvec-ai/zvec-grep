//! Storage read primitives, optionally grouped under one store lock.

use std::collections::HashMap;

use super::{IndexStore, StoreState, graph_error};
use crate::{
    EngineError, EngineResult,
    domain::{CodeMetadata, Entity, EntityId, FileEdge, FileId, FileRecord, GraphEdgeKind},
    storage::{
        fragments,
        graph::Direction,
        types::{StorageSearchFilter, StorageSearchHit},
    },
};

impl IndexStore {
    /// Read several tables without an in-process write between operations.
    /// Callers choose bounded groups of dependent reads, not an entire multi-result
    /// query. A graph edge and all of its endpoint reads must stay in one callback.
    /// The caller retains the workspace lock or writer lease for cross-process protection.
    /// Use only the view inside the callback: `IndexStore` methods would reacquire its mutex.
    pub(crate) fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&ReadView<'_>) -> EngineResult<T>,
    ) -> EngineResult<T> {
        self.read(|state| operation(&ReadView { state }))
    }
}

/// Raw reads borrowing a locked store; selection and visibility belong to the caller.
pub(crate) struct ReadView<'a> {
    state: &'a StoreState,
}

impl ReadView<'_> {
    pub(crate) fn find_symbol_entity_ids(
        &self,
        name: &str,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<EntityId>> {
        if fragments::empty_filter(filter) {
            return Ok(Vec::new());
        }
        let filter = fragments::build_filter(filter, &|path| self.state.directories.get(path))?;
        self.state
            .fragments
            .find_symbol_entity_ids(name, filter.as_deref())
    }

    pub(crate) fn fetch_code_metadata(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, (FileId, CodeMetadata)>> {
        self.state.entities.fetch_code_metadata(ids)
    }

    pub(crate) fn fetch_entities(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, Entity>> {
        self.state.entities.fetch(ids)
    }

    pub(crate) fn fetch_entity_file_ids(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, FileId>> {
        self.state.entities.fetch_file_ids(ids)
    }

    pub(crate) fn fetch_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        self.state.files.fetch(ids)
    }

    pub(crate) fn has_graph(&self) -> EngineResult<bool> {
        if let Some(error) = &self.state.graph_open_error {
            return Err(EngineError::from_report(error.clone()));
        }
        Ok(self.state.graph.is_some())
    }

    pub(crate) fn graph_neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> EngineResult<Vec<FileEdge>> {
        self.has_graph()?;
        self.state
            .graph
            .as_ref()
            .ok_or_else(|| EngineError::not_found("workspace graph is missing"))?
            .neighborhood(id, direction, kinds)
            .map_err(graph_error)
    }

    pub(crate) fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        if limit == 0 || fragments::empty_filter(filter) {
            return Ok(Vec::new());
        }
        let filter = fragments::build_filter(filter, &|path| self.state.directories.get(path))?;
        self.state
            .fragments
            .search_fts(query, limit, filter.as_deref())
    }
}
