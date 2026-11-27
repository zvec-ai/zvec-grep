//! Request-local stage collection; no persistence or benchmark scoring.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsStr,
    path::PathBuf,
    time::Instant,
};

use super::{
    pipeline::{ResolvedSearchRoute, SearchHit},
    storage::SearchStorage,
};
use crate::{
    api::context::stage_trace::{
        CandidateStages, ObservedEntity, RecallAttempt, StagePoolSizes, StageRange, StageRecall,
        StageTrace,
    },
    domain::{EntityId, FileId},
    storage::types::StorageSearchHit,
};

pub(super) fn configured_paths() -> Option<HashSet<PathBuf>> {
    let enabled = env::var_os("ZG_TRACE_STAGES");
    if enabled.as_deref() != Some(OsStr::new("1")) {
        return None;
    }
    parse_paths(env::var_os("ZG_TRACE_PATHS").as_deref())
}

fn parse_paths(paths: Option<&OsStr>) -> Option<HashSet<PathBuf>> {
    paths.map_or_else(
        || Some(HashSet::new()),
        |paths| serde_json::from_str(paths.to_str()?).ok(),
    )
}

pub(super) struct StageTraceCollector {
    paths: HashMap<FileId, String>,
    fragments: BTreeMap<String, HashSet<String>>,
    capture: StageTrace,
}

impl StageTraceCollector {
    pub(super) fn begin(
        routes: &[ResolvedSearchRoute],
        limit: usize,
        watched: &HashSet<PathBuf>,
        storage: &dyn SearchStorage,
    ) -> Self {
        let started = Instant::now();
        let indexed = storage.indexed_sources(watched);
        let index_read_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let mut watched_paths = watched
            .iter()
            .filter_map(|path| path.to_str())
            .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"))
            .collect::<Vec<_>>();
        watched_paths.sort();
        let mut capture = StageTrace {
            version: 1,
            group_id: String::new(),
            limit,
            watched_paths,
            index_observed: indexed.is_ok(),
            index_read_ms,
            routes: routes
                .iter()
                .map(|route| (route.id.clone(), StageRecall::default()))
                .collect(),
            sizes: StagePoolSizes::default(),
            files: BTreeMap::new(),
        };
        let mut paths = HashMap::new();
        for file in indexed.unwrap_or_default() {
            let Some(path) = file.path.to_str() else {
                continue;
            };
            let path = path.replace(std::path::MAIN_SEPARATOR, "/");
            paths.insert(file.id, path.clone());
            capture.files.insert(
                path,
                file.entities
                    .into_iter()
                    .map(|entity| {
                        (
                            entity.id.as_str().to_owned(),
                            ObservedEntity::new(
                                Some(StageRange::from_location(entity.range, entity.source_bytes)),
                                entity.searchable,
                            ),
                        )
                    })
                    .collect(),
            );
        }
        Self {
            paths,
            fragments: BTreeMap::new(),
            capture,
        }
    }

    fn stages(&mut self, file: FileId, id: &EntityId) -> Option<&mut CandidateStages> {
        let path = self.paths.get(&file)?;
        let entities = self.capture.files.get_mut(path)?;
        let observed = entities
            .entry(id.as_str().to_owned())
            .or_insert_with(|| ObservedEntity::new(None, false));
        Some(&mut observed.stages)
    }

    pub(super) fn recall(&mut self, route: &str, depth: usize, hits: &[StorageSearchHit]) {
        let recall = self.capture.routes.entry(route.to_owned()).or_default();
        recall.attempts.push(RecallAttempt {
            depth,
            returned_fragments: hits.len(),
        });
        let fragments = self.fragments.entry(route.to_owned()).or_default();
        fragments.extend(hits.iter().map(|hit| hit.document_id.clone()));
        recall.unique_fragments = fragments.len();
        for (position, hit) in hits.iter().enumerate() {
            if let Some(stages) = self.stages(hit.file_id, &hit.entity_id) {
                let best = stages
                    .recall
                    .entry(route.to_owned())
                    .or_insert(position + 1);
                *best = (*best).min(position + 1);
            }
        }
    }

    pub(super) fn merged<'a>(
        &mut self,
        size: usize,
        candidates: impl Iterator<Item = (FileId, &'a EntityId)>,
    ) {
        self.capture.sizes.merged = size;
        for (file, id) in candidates {
            if let Some(stages) = self.stages(file, id) {
                stages.merged = true;
            }
        }
    }

    pub(super) fn fused<'a>(
        &mut self,
        size: usize,
        candidates: impl Iterator<Item = (FileId, &'a EntityId, usize)>,
    ) {
        self.capture.sizes.fused = size;
        for (file, id, rank) in candidates {
            if let Some(stages) = self.stages(file, id) {
                stages.rrf_rank = Some(rank);
            }
        }
    }

    pub(super) fn loaded(&mut self, hits: &[SearchHit]) {
        self.capture.sizes.loaded = hits.len();
        for (position, hit) in hits.iter().enumerate() {
            if let Some(stages) = self.stages(hit.file.id(), hit.entity.id()) {
                stages.loaded_rrf_rank = Some(position + 1);
                // Static ranking preserves these positions except where code hits are reordered.
                stages.static_rank = Some(position + 1);
                stages.loaded_range = Some(StageRange::from_location(
                    hit.entity.source_range(),
                    hit.entity.source_byte_offsets(),
                ));
            }
        }
    }

    pub(super) fn static_rank(&mut self, hit: &SearchHit, rank: usize) {
        if let Some(stages) = self.stages(hit.file.id(), hit.entity.id()) {
            stages.static_rank = Some(rank);
        }
    }

    pub(super) fn finish(mut self, hits: &[SearchHit]) -> StageTrace {
        self.capture.sizes.selected = hits.len();
        for hit in hits {
            if let Some(stages) = self.stages(hit.file.id(), hit.entity.id()) {
                stages.final_rank = Some(hit.rank);
            }
        }
        self.capture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_paths_are_optional_and_require_a_json_array() {
        assert_eq!(parse_paths(None), Some(HashSet::new()));
        assert_eq!(
            parse_paths(Some(OsStr::new(r#"["src/a.rs","src/b.rs","src/a.rs"]"#))),
            Some(["src/a.rs", "src/b.rs"].map(PathBuf::from).into())
        );
        assert!(parse_paths(Some(OsStr::new("not json"))).is_none());
        assert!(parse_paths(Some(OsStr::new(r#"{"path":"src/a.rs"}"#))).is_none());
    }
}
