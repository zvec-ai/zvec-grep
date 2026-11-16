//! Private sidecar for the Retrieval-only benchmark's isolated daemon.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::PathBuf,
    time::Instant,
};

use serde::{Deserialize, Serialize};

use super::{
    pipeline::{ResolvedSearchRoute, SearchHit},
    storage::SearchStorage,
};
use crate::{
    api::context::options::ContextRouteMode,
    domain::{EntityId, FileId, Range},
    storage::types::StorageSearchHit,
    utils::atomic_write,
};

#[derive(Deserialize)]
struct Request {
    id: u32,
    query: String,
    mode: String,
    paths: HashSet<PathBuf>,
}

#[derive(Default, Serialize)]
struct CandidateStages {
    recall: BTreeMap<String, usize>,
    merged: bool,
    rrf_rank: Option<usize>,
    loaded_rrf_rank: Option<usize>,
    loaded_range: Option<Range>,
    static_rank: Option<usize>,
    final_rank: Option<usize>,
}

#[derive(Serialize)]
struct ObservedEntity {
    range: Option<Range>,
    searchable: bool,
    #[serde(flatten)]
    stages: CandidateStages,
}

impl ObservedEntity {
    fn new(range: Option<Range>, searchable: bool) -> Self {
        Self {
            range,
            searchable,
            stages: CandidateStages::default(),
        }
    }
}

#[derive(Serialize)]
struct RecallAttempt {
    depth: usize,
    returned_fragments: usize,
}

#[derive(Default, Serialize)]
struct Recall {
    attempts: Vec<RecallAttempt>,
    unique_fragments: usize,
    #[serde(skip)]
    fragments: HashSet<String>,
}

#[derive(Default, Serialize)]
struct PoolSizes {
    merged: usize,
    fused: usize,
    loaded: usize,
    selected: usize,
}

#[derive(Serialize)]
struct Capture {
    version: u32,
    id: u32,
    limit: usize,
    index_observed: bool,
    index_read_ms: f64,
    routes: BTreeMap<String, Recall>,
    sizes: PoolSizes,
    files: BTreeMap<String, BTreeMap<String, ObservedEntity>>,
}

pub(super) struct GoldStages {
    directory: PathBuf,
    paths: HashMap<FileId, String>,
    capture: Capture,
}

impl GoldStages {
    pub(super) fn begin(
        routes: &[ResolvedSearchRoute],
        limit: usize,
        prefer_symbol: bool,
        storage: &dyn SearchStorage,
    ) -> Option<Self> {
        let directory = PathBuf::from(env::var_os("ZG_RETRIEVAL_GOLD_STAGE_DIR")?);
        let request: Request =
            serde_json::from_slice(&fs::read(directory.join("request.json")).ok()?).ok()?;
        let fts = routes
            .iter()
            .filter(|route| route.mode == ContextRouteMode::Fts)
            .count();
        let vector = routes.len() - fts;
        let mode = match (fts, vector) {
            (1, 0) => "fts",
            (0, 1) => "vector",
            (1, 1) => "hybrid",
            _ => return None,
        };
        if limit != 10
            || prefer_symbol
            || request.mode != mode
            || routes
                .iter()
                .any(|route| route.query != request.query.trim())
        {
            return None;
        }
        let started = Instant::now();
        let indexed = storage.indexed_sources(&request.paths);
        let mut capture = Capture {
            version: 1,
            id: request.id,
            limit,
            index_observed: indexed.is_ok(),
            index_read_ms: started.elapsed().as_secs_f64() * 1_000.0,
            routes: routes
                .iter()
                .map(|route| (route.id.clone(), Recall::default()))
                .collect(),
            sizes: PoolSizes::default(),
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
                    .map(|(id, range, searchable)| {
                        (
                            id.as_str().to_owned(),
                            ObservedEntity::new(Some(range), searchable),
                        )
                    })
                    .collect(),
            );
        }
        Some(Self {
            directory,
            paths,
            capture,
        })
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
        recall
            .fragments
            .extend(hits.iter().map(|hit| hit.document_id.clone()));
        recall.unique_fragments = recall.fragments.len();
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
                stages.loaded_range = Some(hit.entity.source_range());
            }
        }
    }

    pub(super) fn static_rank(&mut self, hit: &SearchHit, rank: usize) {
        if let Some(stages) = self.stages(hit.file.id(), hit.entity.id()) {
            stages.static_rank = Some(rank);
        }
    }

    pub(super) fn finish(mut self, hits: &[SearchHit]) {
        self.capture.sizes.selected = hits.len();
        for hit in hits {
            if let Some(stages) = self.stages(hit.file.id(), hit.entity.id()) {
                stages.final_rank = Some(hit.rank);
            }
        }
        if let Ok(bytes) = serde_json::to_vec(&self.capture) {
            let _ = atomic_write(
                &self
                    .directory
                    .join(format!("capture-{}.json", self.capture.id)),
                &bytes,
            );
        }
    }
}
