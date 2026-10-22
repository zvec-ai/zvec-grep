//! Read-only relationship access under the store lock.
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use super::{IndexStore, StoreState, assert_usable, graph_error, io_error, lock_state};
use crate::storage::{
    fragments::build_filter,
    graph::{Direction, OpenMode, SqliteGraphStorage},
    types::{StoragePathFilter, StorageSearchFilter, StoredEntity},
};
use crate::{
    EngineError, EngineResult,
    api::relationships::MAX_SYMBOL_MATCHES,
    domain::{
        Entity, EntityId, FileEdge, FileId, FileIndexStatus, FileRecord, GraphEdgeKind, SourcePath,
    },
};

const MAX_SYMBOL_SUGGESTIONS: usize = 5;
const EDGE_BATCH_SIZE: usize = 256;

impl IndexStore {
    /// Keep symbol lookup, edge selection and endpoint hydration on one snapshot.
    /// In-process writers use this same mutex for file replacement and deletion.
    pub(crate) fn with_relationship_read<T>(
        &self,
        operation: impl FnOnce(&RelationshipReadSession<'_>) -> EngineResult<T>,
    ) -> EngineResult<T> {
        // Opening the graph here also reports a missing graph for unknown symbols.
        self.read_graph(|graph, state| operation(&RelationshipReadSession { state, graph }))
    }

    #[cfg(test)]
    pub(crate) fn find_symbols(
        &self,
        name: &str,
        scope: Option<&str>,
    ) -> EngineResult<Vec<StoredEntity>> {
        self.with_relationship_read(|session| session.find_symbols(name, scope, None))
    }

    fn read_graph<T>(
        &self,
        operation: impl FnOnce(&SqliteGraphStorage, &StoreState) -> EngineResult<T>,
    ) -> EngineResult<T> {
        let shared = self.shared()?;
        let state = lock_state(&shared)?;
        assert_usable(&state)?;
        match fs::metadata(&state.graph_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::not_found(
                    "workspace graph is missing; this index has no persisted relationship data",
                ));
            }
            Err(error) => {
                return Err(io_error(
                    "inspect workspace graph",
                    &state.graph_path,
                    &error,
                ));
            }
        }
        let graph =
            SqliteGraphStorage::open(&state.graph_path, OpenMode::ReadOnly).map_err(graph_error)?;
        graph
            .with_read_snapshot(|graph| operation(graph, &state))
            .map_err(graph_error)?
    }
}

/// Read-only access valid while the store mutex is held for the entire query.
pub(crate) struct RelationshipReadSession<'a> {
    state: &'a StoreState,
    graph: &'a SqliteGraphStorage,
}

impl RelationshipReadSession<'_> {
    /// Bounded best-effort suggestions, never relationship query roots.
    /// Do not require the requested scope: a misspelled scope is a useful retry case.
    pub(crate) fn suggest_symbols(
        &self,
        name: &str,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let query = name.to_lowercase();
        let path_filter = self.native_path_filter(path)?;
        let hits = self
            .state
            .fragments
            .search_fts(&query, 100, path_filter.as_deref())?;
        let mut seen = HashSet::new();
        let ids: Vec<_> = hits
            .into_iter()
            .map(|hit| hit.entity_id)
            .filter(|id| seen.insert(id.clone()))
            .collect();
        let mut metadata = self.state.entities.fetch_code_metadata(&ids)?;
        let mut candidates: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let (file_id, metadata) = metadata.remove(&id)?;
                let rank = suggestion_rank(&query, metadata.symbol_name.as_deref()?)?;
                Some((id, file_id, rank))
            })
            .collect();
        // Stable sort preserves FTS relevance within each name-match category.
        // Rank before limiting, and hydrate only the selected visible definitions.
        candidates.sort_by_key(|(_, _, rank)| *rank);
        let file_ids: Vec<_> = candidates
            .iter()
            .map(|(_, file_id, _)| *file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let files = self.state.files.fetch(&file_ids)?;
        let selected: Vec<_> = candidates
            .into_iter()
            .filter_map(|(id, file_id, _)| {
                let file = files.get(&file_id)?;
                visible_in_path(file, path).then(|| (id, file.clone()))
            })
            .take(MAX_SYMBOL_SUGGESTIONS)
            .collect();
        self.load_symbols(selected)
    }

    /// Resolve bounded roots and reuse their checked file records while hydrating.
    pub(crate) fn find_symbols(
        &self,
        name: &str,
        scope: Option<&str>,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let selected = self.select_symbols(name, scope, path, MAX_SYMBOL_MATCHES + 1)?;
        if selected.len() > MAX_SYMBOL_MATCHES {
            return Err(EngineError::invalid_argument(format!(
                "symbol query matches more than {MAX_SYMBOL_MATCHES} indexed definitions; use a narrower scope::name or path filter",
            )));
        }
        self.load_symbols(selected)
    }

    /// Exact bare-name candidates for a wrong scope, bounded without an overflow error.
    pub(crate) fn suggest_exact_symbols(
        &self,
        name: &str,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let selected = self.select_symbols(name, None, path, MAX_SYMBOL_SUGGESTIONS)?;
        self.load_symbols(selected)
    }

    /// Narrow native retrieval before loading any symbol metadata or applying caps.
    /// Check the exact file first: directory identities can outlive their files.
    fn native_path_filter(&self, path: Option<&Path>) -> EngineResult<Option<String>> {
        let Some(path) = path else { return Ok(None) };
        let path = SourcePath::new(path)?;
        let filter = if let Some(file_id) = self.state.files.find_id(&path)? {
            StorageSearchFilter {
                file_ids: Some(vec![file_id]),
                ..StorageSearchFilter::default()
            }
        } else {
            StorageSearchFilter {
                path: Some(StoragePathFilter::Directory(path)),
                ..StorageSearchFilter::default()
            }
        };
        build_filter(Some(&filter), &|path| self.state.directories.get(path))
    }

    fn select_symbols(
        &self,
        name: &str,
        scope: Option<&str>,
        path: Option<&Path>,
        limit: usize,
    ) -> EngineResult<Vec<(EntityId, FileRecord)>> {
        if name.is_empty() {
            return Err(EngineError::invalid_argument(
                "symbol name must not be empty",
            ));
        }
        let state = self.state;
        let path_filter = self.native_path_filter(path)?;
        let ids = state
            .fragments
            .find_symbol_entity_ids(name, path_filter.as_deref())?;
        let mut selected = Vec::new();
        'batches: for batch in ids.chunks(128) {
            let mut owners: HashMap<_, _> = state
                .entities
                .fetch_code_metadata(batch)?
                .into_iter()
                .filter_map(|(id, (file_id, metadata))| {
                    (metadata.symbol_name.as_deref() == Some(name)
                        && scope.is_none_or(|scope| {
                            metadata.scope.as_deref().unwrap_or("").replace('.', "::") == scope
                        }))
                    .then_some((id, file_id))
                })
                .collect();
            let file_ids: Vec<_> = owners
                .values()
                .copied()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let files = state.files.fetch(&file_ids)?;
            for id in batch {
                if let Some(file) = owners.remove(id).and_then(|file_id| files.get(&file_id))
                    && visible_in_path(file, path)
                {
                    selected.push((id.clone(), file.clone()));
                    if selected.len() == limit {
                        break 'batches;
                    }
                }
            }
        }
        Ok(selected)
    }

    fn load_symbols(
        &self,
        selected: Vec<(EntityId, FileRecord)>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let ids: Vec<_> = selected.iter().map(|(id, _)| id.clone()).collect();
        let mut entities = self.state.entities.fetch(&ids)?;
        // Selection already read and checked these records under the same store lock.
        Ok(selected
            .into_iter()
            .filter_map(|(id, file)| {
                let entity = entities.remove(&id)?;
                (entity.file_id() == file.id()).then_some(StoredEntity { entity, file })
            })
            .collect())
    }

    /// Fetch canonical entities and their indexed files without scanning either table.
    pub(crate) fn get_entities(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, StoredEntity>> {
        get_entities(self.state, ids)
    }

    /// Collect up to `limit` visible edges and their caller paths, advancing across invisible pages.
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
        limit: usize,
    ) -> EngineResult<Vec<(FileEdge, PathBuf)>> {
        let mut edges = Vec::new();
        let mut cursor = 0;
        let mut hydration_size = limit.clamp(1, EDGE_BATCH_SIZE);
        let mut checked = HashSet::new();
        let mut visible = HashMap::new();
        while edges.len() < limit {
            // Scan full pages even when only a few visible edges are still needed.
            let page = self
                .graph
                .neighborhood_page(id, direction, kinds, cursor, EDGE_BATCH_SIZE)
                .map_err(graph_error)?;
            let Some((last_id, _)) = page.last() else {
                break;
            };
            // Advance by scanned rows even when the entire page is invisible.
            cursor = *last_id;
            let exhausted = page.len() < EDGE_BATCH_SIZE;
            // SQLite can be ahead of zvec after a failed write. Filter both
            // endpoints before counting a row toward the requested limit.
            let mut rows = page.into_iter();
            while edges.len() < limit {
                let batch: Vec<_> = rows.by_ref().take(hydration_size).collect();
                if batch.is_empty() {
                    break;
                }
                let ids: Vec<_> = batch
                    .iter()
                    .flat_map(|(_, edge)| [&edge.source, &edge.target])
                    .map(|id| EntityId::from_string(id.clone()))
                    .filter(|id| checked.insert(id.clone()))
                    .collect();
                visible.extend(endpoint_paths(self.state, &ids)?);
                let before = edges.len();
                let remaining = limit - before;
                edges.extend(
                    batch
                        .into_iter()
                        .filter_map(|(_, edge)| {
                            let path = visible.get(&EntityId::from_string(edge.source.clone()))?;
                            visible.get(&EntityId::from_string(edge.target.clone()))?;
                            Some((edge, path.clone()))
                        })
                        .take(remaining),
                );
                // Small hydration batches serve ordinary queries without decoding unused
                // endpoints. Grow across invisible stretches without shrinking SQLite pages.
                hydration_size = if edges.len() == before {
                    hydration_size.saturating_mul(2).min(EDGE_BATCH_SIZE)
                } else {
                    (limit - edges.len()).clamp(1, EDGE_BATCH_SIZE)
                };
            }
            if exhausted {
                break;
            }
        }
        Ok(edges)
    }
}

fn visible_in_path(file: &FileRecord, path: Option<&Path>) -> bool {
    file.index_status().is_indexed()
        && path.is_none_or(|path| file.relative_path().as_path().starts_with(path))
}

/// Smaller ranks are better; reverse containment needs a meaningful name length.
fn suggestion_rank(query: &str, candidate: &str) -> Option<u8> {
    let candidate = candidate.to_lowercase();
    if candidate.is_empty() {
        None
    } else if candidate == query {
        Some(0)
    } else if candidate.starts_with(query) {
        Some(1)
    } else if candidate.contains(query) {
        Some(2)
    } else if candidate.chars().count() >= 3 && query.contains(&candidate) {
        Some(3)
    } else {
        None
    }
}

fn get_entities(
    state: &StoreState,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, StoredEntity>> {
    let entities = state.entities.fetch(ids)?;
    let file_ids = entities
        .values()
        .map(Entity::file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = state.files.fetch(&file_ids)?;
    Ok(entities
        .into_iter()
        .filter_map(|(id, entity)| {
            let file = files.get(&entity.file_id())?;
            if !matches!(file.index_status(), FileIndexStatus::Indexed { .. }) {
                return None;
            }
            Some((
                id,
                StoredEntity {
                    entity,
                    file: file.clone(),
                },
            ))
        })
        .collect())
}

fn endpoint_paths(
    state: &StoreState,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, PathBuf>> {
    let mut owners = state.entities.fetch_file_ids(ids)?;
    for id in ids {
        if let Some(file_id) = id
            .as_str()
            .strip_prefix('f')
            .and_then(|id| id.parse::<u32>().ok())
        {
            // Only canonical f{file_id} spellings represent implicit file endpoints.
            if id.as_str() == format!("f{file_id}") {
                owners.insert(id.clone(), FileId::new(file_id));
            }
        }
    }
    let file_ids: Vec<_> = owners
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let files = state.files.fetch(&file_ids)?;
    Ok(owners
        .into_iter()
        .filter_map(|(id, file_id)| {
            files
                .get(&file_id)
                .filter(|file| file.index_status().is_indexed())
                .map(|file| (id, file.relative_path().to_path_buf()))
        })
        .collect())
}
