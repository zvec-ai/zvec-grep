//! Storage read primitives, optionally grouped under one store lock.

use std::collections::{HashMap, HashSet};

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
                after_edge_read: None,
                #[cfg(test)]
                entity_file_reads: None,
                #[cfg(test)]
                after_entity_file_read: None,
            })
        })
    }
}

/// Reads borrowing a locked store; query selection and ranking belong to the caller.
pub(crate) struct ReadView<'a> {
    state: &'a StoreState,
    #[cfg(test)]
    after_edge_read: Option<&'a dyn Fn(&str)>,
    #[cfg(test)]
    entity_file_reads: Option<&'a std::cell::Cell<usize>>,
    #[cfg(test)]
    after_entity_file_read: Option<&'a dyn Fn()>,
}

impl ReadView<'_> {
    /// Observe reads inside the held snapshot for deterministic concurrency tests.
    #[cfg(test)]
    pub(crate) fn observe_reads<'a>(
        &'a self,
        after_edge_read: &'a dyn Fn(&str),
        entity_file_reads: &'a std::cell::Cell<usize>,
        after_entity_file_read: Option<&'a dyn Fn()>,
    ) -> ReadView<'a> {
        ReadView {
            state: self.state,
            after_edge_read: Some(after_edge_read),
            entity_file_reads: Some(entity_file_reads),
            after_entity_file_read,
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
        let filter = self
            .state
            .fragments
            .build_filter(filter, &|path| self.state.directories.get(path))?;
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

    pub(crate) fn visible_endpoint_files(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, FileRecord>> {
        #[cfg(test)]
        if let Some(reads) = self.entity_file_reads {
            reads.set(reads.get() + 1);
        }
        let files = visible_endpoint_files(self.state, ids)?;
        #[cfg(test)]
        if let Some(after_read) = self.after_entity_file_read {
            after_read();
        }
        Ok(files)
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

    pub(crate) fn neighborhood<T>(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
        read: impl FnOnce(&mut dyn Iterator<Item = EngineResult<FileEdge>>) -> EngineResult<T>,
    ) -> EngineResult<T> {
        self.state
            .graph
            .as_ref()
            .map_err(|error| EngineError::from_report(error.clone()))?
            .scan_neighborhood(id, direction, kinds, |rows| {
                let mut rows = rows.map(|row| {
                    let edge = row.map_err(graph_error)?;
                    #[cfg(test)]
                    if let Some(after_read) = self.after_edge_read {
                        after_read(id);
                    }
                    Ok(edge)
                });
                read(&mut rows)
            })
            .map_err(graph_error)?
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
        let representation = filter.and_then(|filter| filter.representation);
        let filter = self
            .state
            .fragments
            .build_filter(filter, &|path| self.state.directories.get(path))?;
        self.state.fragments.search_fts_for_representation(
            query,
            limit,
            filter.as_deref(),
            representation,
        )
    }
}

/// Resolve endpoint ownership and Indexed visibility under the caller's store lock.
/// File endpoints use only the canonical `f{file_id}` spelling, including files without entities.
pub(super) fn visible_endpoint_files(
    state: &StoreState,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, FileRecord>> {
    let mut owners = state.entities.fetch_file_ids(ids)?;
    for id in ids {
        if let Some(file_id) = id
            .as_str()
            .strip_prefix('f')
            .and_then(|id| id.parse::<u32>().ok())
            && id.as_str() == format!("f{file_id}")
        {
            owners.insert(id.clone(), FileId::new(file_id));
        }
    }
    let file_ids = owners
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = state.files.fetch(&file_ids)?;
    Ok(owners
        .into_iter()
        .filter_map(|(id, file_id)| {
            files
                .get(&file_id)
                .filter(|file| file.index_status().is_indexed())
                .map(|file| (id, file.clone()))
        })
        .collect())
}
