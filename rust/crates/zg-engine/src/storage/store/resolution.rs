//! Per-operation resolver access; no store lock escapes these methods.
//!
//! The indexing service must recover unfinished files and retain its workspace
//! write lock from the first reference/candidate read through the last writeback.
//! Individual calls do not protect a resolution from intervening file updates.

use std::collections::{HashMap, HashSet};

use super::{IndexStore, StoreState, graph_error};
use crate::{
    EngineError, EngineResult,
    domain::{EntityId, FileId},
    storage::{
        graph::{PendingRefPage, Resolution, ResolutionStats},
        types::ResolutionCandidate,
    },
};

// The resolver will consume these entry points in its integration PR.
#[cfg_attr(not(test), allow(dead_code))]
impl IndexStore {
    /// Read one bounded page, hiding refs whose source file is not Indexed.
    /// A filtered-out page may be empty while `next_cursor` still advances.
    /// Requires writable storage and the outer workspace write lock.
    pub(crate) fn list_pending_refs(
        &self,
        limit: usize,
        cursor: i64,
    ) -> EngineResult<PendingRefPage> {
        self.write(|state| {
            let mut page = state
                .graph
                .as_ref()
                .expect("writable graph")
                .list_pending_refs(limit, cursor)
                .map_err(graph_error)?;
            let ids = page
                .refs
                .iter()
                .map(|r| r.reference.owner_id.clone())
                .collect::<Vec<_>>();
            let visible = visible_endpoints(state, &ids)?;
            page.refs.retain(|r| {
                visible
                    .get(&r.reference.owner_id)
                    .is_some_and(|file| file.get() == r.file_id)
            });
            Ok(page)
        })
    }

    /// Return every visible exact-name definition, deduplicated across fragments/models.
    /// The resolver selects scope, language and other constraints from metadata.
    /// No search-result cap may be used to infer uniqueness.
    pub(crate) fn find_resolution_candidates(
        &self,
        name: &str,
    ) -> EngineResult<Vec<ResolutionCandidate>> {
        if name.trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "reference name must not be empty",
            ));
        }
        self.read(|state| {
            let ids = state.fragments.find_symbol_entity_ids(name, None)?;
            let mut result = Vec::new();
            for batch in ids.chunks(128) {
                let mut metadata = state.entities.fetch_code_metadata(batch)?;
                let file_ids = metadata
                    .values()
                    .map(|(file_id, _)| *file_id)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let files = state.files.fetch(&file_ids)?;
                for id in batch {
                    if let Some((file_id, metadata)) = metadata.remove(id)
                        && metadata.symbol_name.as_deref() == Some(name)
                        && let Some(file) = files.get(&file_id)
                        && file.index_status().is_indexed()
                    {
                        result.push(ResolutionCandidate {
                            entity_id: id.clone(),
                            file: file.clone(),
                            metadata,
                        });
                    }
                }
            }
            Ok(result)
        })
    }

    /// Recheck current pending rows and both endpoints before one atomic graph writeback.
    /// This catches missing/hidden endpoints, not changes to name uniqueness: callers
    /// must retain the outer workspace write lock throughout lookup and computation.
    pub(crate) fn apply_resolutions(
        &self,
        resolutions: &[Resolution],
    ) -> EngineResult<ResolutionStats> {
        self.write(|state| {
            let ids = resolutions.iter().map(|r| r.ref_id).collect::<Vec<_>>();
            let refs = state
                .graph
                .as_ref()
                .expect("writable graph")
                .get_pending_refs(&ids)
                .map_err(graph_error)?;
            let endpoints = refs
                .iter()
                .map(|r| r.reference.owner_id.clone())
                .chain(resolutions.iter().map(|r| r.target_id.clone()))
                .collect::<Vec<_>>();
            let visible = visible_endpoints(state, &endpoints)?;
            let eligible = refs
                .iter()
                .filter(|r| {
                    visible
                        .get(&r.reference.owner_id)
                        .is_some_and(|file| file.get() == r.file_id)
                })
                .map(|r| r.id)
                .collect::<HashSet<_>>();
            let valid = resolutions
                .iter()
                .filter(|r| eligible.contains(&r.ref_id) && visible.contains_key(&r.target_id))
                .cloned()
                .collect::<Vec<_>>();
            let mut stats = state
                .graph
                .as_mut()
                .expect("writable graph")
                .apply_resolutions(&valid)
                .map_err(graph_error)?;
            stats.stale += resolutions.len() - valid.len();
            Ok(stats)
        })
    }
}

fn visible_endpoints(state: &StoreState, ids: &[String]) -> EngineResult<HashMap<String, FileId>> {
    let entity_ids = ids
        .iter()
        .cloned()
        .map(EntityId::from_string)
        .collect::<Vec<_>>();
    let mut owners = state
        .entities
        .fetch_file_ids(&entity_ids)?
        .into_iter()
        .map(|(id, file_id)| (id.as_str().to_owned(), file_id))
        .collect::<HashMap<_, _>>();
    for id in ids {
        if let Some(file_id) = id.strip_prefix('f').and_then(|id| id.parse::<u32>().ok())
            && *id == format!("f{file_id}")
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
    owners.retain(|_, file_id| {
        files
            .get(file_id)
            .is_some_and(|file| file.index_status().is_indexed())
    });
    Ok(owners)
}
