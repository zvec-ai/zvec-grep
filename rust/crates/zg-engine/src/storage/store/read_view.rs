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
        self.read(|state| {
            operation(&ReadView {
                state,
                #[cfg(test)]
                after_neighborhood: None,
                #[cfg(test)]
                entity_file_reads: None,
            })
        })
    }
}

/// Raw reads borrowing a locked store; selection and visibility belong to the caller.
pub(crate) struct ReadView<'a> {
    state: &'a StoreState,
    #[cfg(test)]
    after_neighborhood: Option<&'a dyn Fn(&str)>,
    #[cfg(test)]
    entity_file_reads: Option<&'a std::cell::Cell<usize>>,
}

impl ReadView<'_> {
    /// Observe reads inside the held snapshot for deterministic concurrency tests.
    #[cfg(test)]
    pub(crate) fn observe_reads<'a>(
        &'a self,
        after_neighborhood: &'a dyn Fn(&str),
        entity_file_reads: &'a std::cell::Cell<usize>,
    ) -> ReadView<'a> {
        ReadView {
            state: self.state,
            after_neighborhood: Some(after_neighborhood),
            entity_file_reads: Some(entity_file_reads),
        }
    }

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
        #[cfg(test)]
        if let Some(reads) = self.entity_file_reads {
            reads.set(reads.get() + 1);
        }
        self.state.entities.fetch_file_ids(ids)
    }

    pub(crate) fn fetch_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        self.state.files.fetch(ids)
    }

    pub(crate) fn ensure_graph(&self) -> EngineResult<()> {
        self.state
            .graph
            .as_ref()
            .map(|_| ())
            .map_err(|error| EngineError::from_report(error.clone()))
    }

    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> EngineResult<Vec<FileEdge>> {
        let edges = self
            .state
            .graph
            .as_ref()
            .map_err(|error| EngineError::from_report(error.clone()))?
            .neighborhood(id, direction, kinds)
            .map_err(graph_error)?;
        #[cfg(test)]
        if let Some(after_neighborhood) = self.after_neighborhood {
            after_neighborhood(id);
        }
        Ok(edges)
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
