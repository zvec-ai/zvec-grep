use super::storage::SearchStorage;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

use async_trait::async_trait;

use crate::{
    EngineError,
    api::context::{
        options::{ContextRoute as SearchRoute, ContextRouteMode as SearchRouteMode, QueryFilter},
        result::{
            CandidateLoadFailure, FusedCandidateTrace, GroupSearchTrace, MatchedBy,
            RecallAttemptTrace, RecallFragmentTrace, SearchFinalTrace, SearchFusionTrace,
            SearchHitTrace, SearchRecallTrace, SelectedEntityTrace, TimingEntry,
        },
        stage_trace::StageTrace,
    },
    domain::{
        Content, Entity, EntityFragment, EntityId, FileId, FileRecord,
        model::{EmbeddingModelInfo, EmbeddingPurpose},
    },
    models::{EmbeddingOptions, ModelError, ModelRuntimeLease},
    storage::types::{StoragePathFilter, StorageSearchFilter, StorageSearchHit, StoredSearchData},
};

use super::{
    format_filter::{compile_format_filter, matches_file_name},
    graph_expand::{self, GraphSearch, recall_entity},
    path_filter::{all, resolve_glob_filter},
    stage_trace::{StageTraceCollector, configured_paths},
};

const DEFAULT_LIMIT: usize = 7;
const RRF_K: f64 = 60.0;
const RECALL_INITIAL_DEPTH: usize = 200;
const RECALL_MAX_DEPTH: usize = 2_000;
const RECALL_GROWTH_FACTOR: usize = 2;
const RECALL_TARGET_FACTOR: usize = 5;
const RECALL_MIN_TARGET_CANDIDATES: usize = 50;

#[derive(Clone, Debug)]
pub(crate) struct SearchPlan {
    pub routes: Vec<SearchRoute>,
    pub limit: Option<usize>,
    pub trace: bool,
    pub prefer_symbol: bool,
    pub filter: QueryFilter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSearchRoute {
    pub id: String,
    pub mode: SearchRouteMode,
    pub query: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchEvidence {
    pub fragment: EntityFragment,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchHit {
    pub entity: Entity,
    pub file: FileRecord,
    pub evidence: Vec<SearchEvidence>,
    pub rank: usize,
    pub score: f64,
    pub matched_by: MatchedBy,
    pub trace: Option<SearchHitTrace>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchPlanResult {
    pub routes: Vec<ResolvedSearchRoute>,
    pub hits: Vec<SearchHit>,
    pub timings: Vec<TimingEntry>,
    pub trace: Option<GroupSearchTrace>,
    pub graph_error: Option<String>,
    pub stage_trace: Option<StageTrace>,
}

#[async_trait]
pub(crate) trait SearchEmbeddingRuntime: Send + Sync {
    fn info(&self) -> &EmbeddingModelInfo;

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError>;
}

#[async_trait]
impl SearchEmbeddingRuntime for ModelRuntimeLease {
    fn info(&self) -> &EmbeddingModelInfo {
        self.info()
    }

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        self.embed(
            &queries
                .iter()
                .cloned()
                .map(|text| vec![Content::Text(text)])
                .collect::<Vec<_>>(),
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                ..EmbeddingOptions::default()
            },
            None,
        )
        .await
        .map(|result| result.vectors)
    }
}

pub(crate) struct RequestEmbeddingRuntime<'a> {
    pub model: &'a ModelRuntimeLease,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

#[async_trait]
impl SearchEmbeddingRuntime for RequestEmbeddingRuntime<'_> {
    fn info(&self) -> &EmbeddingModelInfo {
        self.model.info()
    }

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        self.model
            .embed(
                &queries
                    .iter()
                    .cloned()
                    .map(|text| vec![Content::Text(text)])
                    .collect::<Vec<_>>(),
                EmbeddingOptions {
                    purpose: EmbeddingPurpose::Query,
                    signal: self.signal.clone(),
                    ..EmbeddingOptions::default()
                },
                None,
            )
            .await
            .map(|result| result.vectors)
    }
}

#[derive(Clone)]
struct RecallRoute {
    route: ResolvedSearchRoute,
    filter: Option<StorageSearchFilter>,
    vector_route_id: Option<String>,
}

struct Candidate {
    id: EntityId,
    file_id: FileId,
    sources: HashSet<SearchRouteMode>,
    recall: Vec<SearchRecallTrace>,
    evidence: Vec<InternalEvidence>,
    score: f64,
    rank: usize,
}

struct InternalEvidence {
    hit: StorageSearchHit,
    path: SearchRouteMode,
    route_id: String,
    rank: usize,
}

pub(crate) async fn search_workspace_index(
    workspace_root: &Path,
    plan: SearchPlan,
    storage: &dyn SearchStorage,
    embedding_models: &[&dyn SearchEmbeddingRuntime],
    graph: Option<GraphSearch<'_>>,
) -> Result<SearchPlanResult, EngineError> {
    search_with_stage_trace(
        workspace_root,
        plan,
        storage,
        embedding_models,
        graph,
        configured_paths().as_ref(),
    )
    .await
}

async fn search_with_stage_trace(
    workspace_root: &Path,
    plan: SearchPlan,
    storage: &dyn SearchStorage,
    embedding_models: &[&dyn SearchEmbeddingRuntime],
    graph: Option<GraphSearch<'_>>,
    watched: Option<&HashSet<PathBuf>>,
) -> Result<SearchPlanResult, EngineError> {
    if embedding_models.len() > 1 {
        return Err(EngineError::unsupported(
            "this version supports only one embedding model per workspace",
        ));
    }
    let total_started = Instant::now();
    let plan_started = Instant::now();
    let routes = resolve_routes(&plan.routes)?;
    validate_modified_range(&plan)?;
    let limit = plan.limit.unwrap_or(DEFAULT_LIMIT);
    let plan_duration = plan_started.elapsed();

    let filter_started = Instant::now();
    let filter = search_plan_to_storage_filter(workspace_root, &plan, storage)?;
    let filter_duration = filter_started.elapsed();
    let has_searchable_files = !filter_matches_no_files(filter.as_ref());

    let embedding_started = Instant::now();
    let vectors = if has_searchable_files
        && routes
            .iter()
            .any(|route| route.mode == SearchRouteMode::Vector)
    {
        let model = embedding_models.first().ok_or_else(|| {
            EngineError::unsupported("vector search requires a configured embedding model")
        })?;
        embed_vector_routes(&routes, *model).await?
    } else {
        HashMap::new()
    };
    let embedding_duration = embedding_started.elapsed();

    let mut observation =
        watched.map(|paths| StageTraceCollector::begin(&routes, limit, paths, storage));
    let recall_started = Instant::now();
    let mut candidates = HashMap::new();
    let mut trace = plan.trace.then(GroupSearchTrace::default);
    if has_searchable_files && limit > 0 {
        collect_adaptive_recall(
            &routes,
            filter.as_ref(),
            plan.prefer_symbol,
            &vectors,
            limit,
            storage,
            &mut candidates,
            trace.as_mut().map(|trace| &mut trace.recall),
            observation.as_mut(),
        )?;
    }
    let recall_duration = recall_started.elapsed();
    if let Some(observation) = observation.as_mut() {
        observation.merged(
            candidates.len(),
            candidates.values().map(|c| (c.file_id, &c.id)),
        );
    }

    let fusion_started = Instant::now();
    let fused = fuse_candidates(candidates);
    let fusion_duration = fusion_started.elapsed();
    if let Some(observation) = observation.as_mut() {
        observation.fused(
            fused.len(),
            fused.iter().map(|c| (c.file_id, &c.id, c.rank)),
        );
    }

    let load_started = Instant::now();
    // Every recalled entity must be eligible for structural boosts before cutoff.
    let hits = load_candidates(fused, limit, storage, trace.as_mut())?;
    let load_duration = load_started.elapsed();
    if let Some(observation) = observation.as_mut() {
        observation.loaded(&hits);
    }
    let mut result = finish_search(
        hits,
        &plan,
        routes,
        filter.as_ref(),
        graph,
        trace,
        observation,
    )?;
    let mut timings = vec![
        timing("search_plan", plan_duration),
        timing("search_filter", filter_duration),
        timing("query_embedding", embedding_duration),
        timing("recall", recall_duration),
        timing("fusion", fusion_duration),
        timing("load_results", load_duration),
    ];
    timings.append(&mut result.timings);
    timings.push(timing("search_total", total_started.elapsed()));
    result.timings = timings;
    Ok(result)
}

fn finish_search(
    hits: Vec<SearchHit>,
    plan: &SearchPlan,
    routes: Vec<ResolvedSearchRoute>,
    filter: Option<&StorageSearchFilter>,
    graph: Option<GraphSearch<'_>>,
    mut trace: Option<GroupSearchTrace>,
    mut observation: Option<StageTraceCollector>,
) -> Result<SearchPlanResult, EngineError> {
    if let Some(trace) = &mut trace {
        trace.ordinary_pool = hits
            .iter()
            .map(|hit| recall_entity(&hit.entity, &hit.file))
            .collect();
    }
    let mut expansion = graph
        .map(|graph| graph_expand::expand(graph, &hits, filter, plan.trace))
        .transpose()?;
    let graph_timing = expansion.as_ref().map(|result| TimingEntry {
        name: "graph_expand".into(),
        duration_micros: result.trace.duration_micros,
        count: result.trace.error.is_none().then_some(result.added as u64),
    });
    let graph_error = expansion
        .as_ref()
        .and_then(|result| result.trace.error.clone());
    let relevance = expansion
        .as_mut()
        .and_then(|result| result.relevance.take());
    let (hits, ranking_timing, graph_ranking_timing) = rank_candidates(
        hits,
        plan,
        &routes,
        filter,
        graph,
        relevance,
        observation.as_mut(),
    )?;
    if let Some(trace) = &mut trace {
        trace.graph = expansion.map(|result| result.trace);
        trace.selected = selected_trace(&hits);
    }

    let stage_trace = observation.map(|observation| observation.finish(&hits));
    Ok(SearchPlanResult {
        routes,
        hits,
        trace,
        graph_error,
        stage_trace,
        timings: [ranking_timing]
            .into_iter()
            .chain(graph_timing)
            .chain(graph_ranking_timing)
            .collect(),
    })
}

fn rank_candidates(
    hits: Vec<SearchHit>,
    plan: &SearchPlan,
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    graph: Option<GraphSearch<'_>>,
    relevance: Option<graph_expand::Relevance>,
    observation: Option<&mut StageTraceCollector>,
) -> Result<(Vec<SearchHit>, TimingEntry, Option<TimingEntry>), EngineError> {
    let started = Instant::now();
    let maximum_rrf = build_recall_routes(routes, filter, plan.prefer_symbol)
        .iter()
        .map(|_| 1.0 / (RRF_K + 1.0))
        .sum();
    let limit = plan.limit.unwrap_or(DEFAULT_LIMIT);
    let ordinary_limit = if relevance.is_some() {
        hits.len()
    } else {
        limit
    };
    let mut hits = super::ranking::rank(
        hits,
        routes,
        &plan.filter,
        maximum_rrf,
        ordinary_limit,
        observation,
    );
    let ranking_timing = timing("ranking", started.elapsed());
    let graph_timing = if let Some(relevance) = relevance {
        let started = Instant::now();
        relevance.rerank(&mut hits, limit, graph.and_then(|graph| graph.signal))?;
        Some(timing("graph_ranking", started.elapsed()))
    } else {
        None
    };
    Ok((hits, ranking_timing, graph_timing))
}

fn selected_trace(hits: &[SearchHit]) -> Vec<SelectedEntityTrace> {
    hits.iter()
        .map(|hit| SelectedEntityTrace {
            entity_id: hit.entity.id().as_str().to_owned(),
            rank: hit.rank,
            trace: hit.trace.clone().expect("ranking trace requested"),
        })
        .collect()
}

fn resolve_routes(routes: &[SearchRoute]) -> Result<Vec<ResolvedSearchRoute>, EngineError> {
    if routes.is_empty() {
        return Err(EngineError::invalid_argument(
            "search plan requires at least one route",
        ));
    }
    let mut counts = HashMap::<SearchRouteMode, usize>::new();
    routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let query = route.query.trim();
            if query.is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "search route {index} requires a non-empty query"
                )));
            }
            let count = counts.entry(route.mode).or_default();
            *count += 1;
            let base = match route.mode {
                SearchRouteMode::Fts => "fts",
                SearchRouteMode::Vector => "vector",
            };
            Ok(ResolvedSearchRoute {
                id: if *count == 1 {
                    base.to_owned()
                } else {
                    format!("{base}-{count}")
                },
                mode: route.mode,
                query: query.to_owned(),
            })
        })
        .collect()
}

fn validate_modified_range(plan: &SearchPlan) -> Result<(), EngineError> {
    if plan.filter.modified_after_epoch_ms.is_some_and(|after| {
        plan.filter
            .modified_before_epoch_ms
            .is_some_and(|before| after > before)
    }) {
        Err(EngineError::invalid_argument(
            "modified-after must not be later than modified-before",
        ))
    } else {
        Ok(())
    }
}

struct ModelQueryVector {
    model: String,
    values: Vec<f32>,
}

async fn embed_vector_routes(
    routes: &[ResolvedSearchRoute],
    model: &dyn SearchEmbeddingRuntime,
) -> Result<HashMap<String, ModelQueryVector>, EngineError> {
    let vector_routes = routes
        .iter()
        .filter(|route| route.mode == SearchRouteMode::Vector)
        .collect::<Vec<_>>();
    let maximum = model.info().model().max_batch_size();
    let mut vectors = HashMap::new();
    for batch in vector_routes.chunks(maximum) {
        let queries = batch
            .iter()
            .map(|route| route.query.clone())
            .collect::<Vec<_>>();
        let embedded = model
            .embed_queries(&queries)
            .await
            .map_err(ModelError::into_engine_error)?;
        if embedded.len() != batch.len() {
            return Err(EngineError::internal(
                "embedding model returned the wrong number of query vectors",
            ));
        }
        for (route, vector) in batch.iter().zip(embedded) {
            vectors.insert(
                route.id.clone(),
                ModelQueryVector {
                    model: model.info().model().reference(),
                    values: vector,
                },
            );
        }
    }
    Ok(vectors)
}

#[allow(clippy::too_many_arguments)]
fn collect_adaptive_recall(
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    prefer_symbol: bool,
    vectors: &HashMap<String, ModelQueryVector>,
    limit: usize,
    storage: &dyn SearchStorage,
    candidates: &mut HashMap<EntityId, Candidate>,
    mut trace: Option<&mut Vec<RecallAttemptTrace>>,
    mut observation: Option<&mut StageTraceCollector>,
) -> Result<(), EngineError> {
    let recall_routes = build_recall_routes(routes, filter, prefer_symbol);
    let target = (limit * RECALL_TARGET_FACTOR).max(RECALL_MIN_TARGET_CANDIDATES);
    let mut depth = RECALL_INITIAL_DEPTH;
    loop {
        let mut saturated = false;
        for route in &recall_routes {
            let hits = match route.route.mode {
                SearchRouteMode::Fts => {
                    storage.search_fts(&route.route.query, depth, route.filter.as_ref())?
                }
                SearchRouteMode::Vector => vectors
                    .get(route.vector_route_id.as_deref().unwrap_or(&route.route.id))
                    .map_or_else(
                        || Ok(Vec::new()),
                        |vector| {
                            storage.search_vector(
                                &vector.model,
                                &vector.values,
                                depth,
                                route.filter.as_ref(),
                            )
                        },
                    )?,
            };
            saturated |= hits.len() >= depth;
            if let Some(trace) = trace.as_deref_mut() {
                trace.push(RecallAttemptTrace {
                    route_id: route.route.id.clone(),
                    mode: route.route.mode,
                    query: route.route.query.clone(),
                    depth,
                    fragments: hits
                        .iter()
                        .enumerate()
                        .map(|(index, hit)| RecallFragmentTrace {
                            fragment_id: hit.document_id.clone(),
                            entity_id: hit.entity_id.as_str().to_owned(),
                            rank: index + 1,
                            score: hit.score,
                        })
                        .collect(),
                });
            }
            if let Some(observation) = observation.as_deref_mut() {
                observation.recall(&route.route.id, depth, &hits);
            }
            add_recall_hits(candidates, &hits, &route.route)?;
        }
        if candidates.len() >= target || !saturated || depth >= RECALL_MAX_DEPTH {
            return Ok(());
        }
        depth = (depth * RECALL_GROWTH_FACTOR).min(RECALL_MAX_DEPTH);
    }
}

fn build_recall_routes(
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    prefer_symbol: bool,
) -> Vec<RecallRoute> {
    let mut output = routes
        .iter()
        .cloned()
        .map(|route| RecallRoute {
            vector_route_id: (route.mode == SearchRouteMode::Vector).then(|| route.id.clone()),
            route,
            filter: filter.cloned(),
        })
        .collect::<Vec<_>>();
    if prefer_symbol {
        let mut symbol_routes = HashSet::new();
        for route in routes {
            // Model expansion creates independent vector searches, while symbol
            // recall still searches all FTS partitions for the original route.
            let logical_id = route
                .id
                .split_once('@')
                .map_or(route.id.as_str(), |(id, _)| id);
            if !symbol_routes.insert(logical_id) {
                continue;
            }
            let symbol_names = extract_symbol_names(&route.query);
            if symbol_names.is_empty() {
                continue;
            }
            let mut symbol_filter = filter.cloned().unwrap_or_default();
            symbol_filter.symbol_names = Some(symbol_names);
            output.push(RecallRoute {
                route: ResolvedSearchRoute {
                    id: format!("{logical_id}.prefer-symbol"),
                    mode: SearchRouteMode::Fts,
                    query: route.query.clone(),
                },
                filter: Some(symbol_filter),
                vector_route_id: None,
            });
        }
    }
    output
}

fn add_recall_hits(
    candidates: &mut HashMap<EntityId, Candidate>,
    hits: &[StorageSearchHit],
    route: &ResolvedSearchRoute,
) -> Result<(), EngineError> {
    // Increasing top-k can change its prefix, including which fragments appear there.
    for (index, hit) in hits.iter().enumerate() {
        let rank = index + 1;
        let candidate = candidates
            .entry(hit.entity_id.clone())
            .or_insert_with(|| Candidate {
                id: hit.entity_id.clone(),
                file_id: hit.file_id,
                sources: HashSet::new(),
                recall: Vec::new(),
                evidence: Vec::new(),
                score: 0.0,
                rank: usize::MAX,
            });
        if candidate.file_id != hit.file_id {
            return Err(EngineError::storage_failure(
                "search hits disagree on entity file ownership",
            ));
        }
        candidate.sources.insert(route.mode);
        if let Some(existing) = candidate.evidence.iter_mut().find(|evidence| {
            evidence.path == route.mode
                && evidence.route_id == route.id
                && evidence.hit.document_id == hit.document_id
        }) {
            if rank < existing.rank {
                existing.hit = hit.clone();
                existing.rank = rank;
            }
        } else {
            candidate.evidence.push(InternalEvidence {
                hit: hit.clone(),
                path: route.mode,
                route_id: route.id.clone(),
                rank,
            });
        }
        add_or_update_recall(
            &mut candidate.recall,
            SearchRecallTrace {
                path: route.mode,
                route_id: route.id.clone(),
                query: route.query.clone(),
                found: true,
                rank: Some(rank),
                score: Some(hit.score),
                forced: false,
                reason: None,
            },
        );
    }
    Ok(())
}

fn add_or_update_recall(recall: &mut Vec<SearchRecallTrace>, next: SearchRecallTrace) {
    if let Some(existing) = recall
        .iter_mut()
        .find(|item| item.path == next.path && item.route_id == next.route_id)
    {
        if existing
            .rank
            .is_none_or(|rank| next.rank.is_some_and(|next_rank| next_rank < rank))
        {
            *existing = next;
        }
    } else {
        recall.push(next);
    }
}

fn fuse_candidates(candidates: HashMap<EntityId, Candidate>) -> Vec<Candidate> {
    let mut fused = candidates
        .into_values()
        .map(|mut candidate| {
            candidate.score = candidate
                .recall
                .iter()
                .filter_map(|trace| trace.rank)
                .map(|rank| 1.0 / (RRF_K + rank_as_f64(rank)))
                .sum();
            candidate
        })
        .collect::<Vec<_>>();
    fused.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    for (index, candidate) in fused.iter_mut().enumerate() {
        candidate.rank = index + 1;
    }
    fused
}

fn load_candidates(
    candidates: Vec<Candidate>,
    limit: usize,
    storage: &dyn SearchStorage,
    mut trace: Option<&mut GroupSearchTrace>,
) -> Result<Vec<SearchHit>, EngineError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let hits = candidates
        .iter()
        .flat_map(|candidate| &candidate.evidence)
        .filter(|evidence| seen.insert(evidence.hit.document_id.clone()))
        .map(|evidence| evidence.hit.clone())
        .collect::<Vec<_>>();
    let mut data = storage.load_search_hits(&hits)?;
    let mut loaded = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let record = trace.is_some().then(|| FusedCandidateTrace {
            entity_id: candidate.id.as_str().to_owned(),
            rank: candidate.rank,
            score: candidate.score,
            load_failure: None,
        });
        let result = candidate_to_hit(candidate, limit, trace.is_some(), &mut data)?;
        if let (Some(group), Some(mut record)) = (trace.as_deref_mut(), record) {
            record.load_failure = result.as_ref().err().copied();
            group.fused.push(record);
        }
        if let Ok(hit) = result {
            loaded.push(hit);
        }
    }
    Ok(loaded)
}

fn candidate_to_hit(
    candidate: Candidate,
    limit: usize,
    trace: bool,
    data: &mut StoredSearchData,
) -> Result<Result<SearchHit, CandidateLoadFailure>, EngineError> {
    let Some(stored) = data.entities.remove(&candidate.id) else {
        // A file replacement may remove owners after recall has completed.
        return Ok(Err(CandidateLoadFailure::MissingEntityOrFile));
    };
    if stored.entity.id() != &candidate.id
        || stored.entity.file_id() != candidate.file_id
        || stored.file.id() != candidate.file_id
    {
        return Err(EngineError::storage_failure(
            "loaded search entity has inconsistent ownership",
        ));
    }
    let matched_by = derive_matched_by(&candidate.sources);
    let mut evidence = candidate.evidence;
    evidence.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| route_mode_order(left.path).cmp(&route_mode_order(right.path)))
            .then_with(|| left.route_id.cmp(&right.route_id))
            .then_with(|| left.hit.document_id.cmp(&right.hit.document_id))
    });
    let mut loaded_evidence = Vec::with_capacity(evidence.len());
    for evidence in evidence {
        let Some(fragment) = data.fragments.get(&evidence.hit.document_id) else {
            continue;
        };
        if fragment.id.as_str() != evidence.hit.document_id
            || !stored
                .entity
                .fragments()
                .iter()
                .any(|owned| owned == fragment)
        {
            return Err(EngineError::storage_failure(
                "loaded search fragment has inconsistent ownership",
            ));
        }
        loaded_evidence.push(SearchEvidence {
            fragment: fragment.clone(),
        });
    }
    if loaded_evidence.is_empty() {
        return Ok(Err(CandidateLoadFailure::MissingFragments));
    }
    Ok(Ok(SearchHit {
        entity: stored.entity,
        file: stored.file,
        evidence: loaded_evidence,
        rank: candidate.rank,
        score: candidate.score,
        matched_by,
        trace: trace.then_some(SearchHitTrace {
            recall: candidate.recall,
            ranking: None,
            graph_ranking: None,
            fusion: SearchFusionTrace {
                rank: candidate.rank,
                score: candidate.score,
                forced: false,
            },
            final_selection: SearchFinalTrace {
                returned_by_limit: candidate.rank <= limit,
                cutoff_rank: limit,
            },
        }),
    }))
}

fn derive_matched_by(sources: &HashSet<SearchRouteMode>) -> MatchedBy {
    match (
        sources.contains(&SearchRouteMode::Fts),
        sources.contains(&SearchRouteMode::Vector),
    ) {
        (true, true) => MatchedBy::FtsAndVector,
        (false, true) => MatchedBy::Vector,
        (true | false, false) => MatchedBy::Fts,
    }
}

fn route_mode_order(mode: SearchRouteMode) -> u8 {
    match mode {
        SearchRouteMode::Fts => 0,
        SearchRouteMode::Vector => 1,
    }
}

fn extract_symbol_names(query: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "class",
        "struct",
        "enum",
        "interface",
        "function",
        "method",
        "type",
        "const",
        "let",
        "var",
        "namespace",
        "where",
        "find",
        "explain",
    ];
    let mut names = HashSet::new();
    for token in query.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '~'))
    }) {
        let token = token.trim_matches(':');
        if token.is_empty()
            || KEYWORDS.contains(&token.to_ascii_lowercase().as_str())
            || !token.chars().next().is_some_and(|character| {
                character.is_ascii_alphabetic() || matches!(character, '_' | '~')
            })
        {
            continue;
        }
        let parts = token
            .split("::")
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let Some(name) = parts.last() else {
            continue;
        };
        let resolved = if parts.len() >= 2
            && parts[parts.len() - 2]
                .chars()
                .next()
                .is_some_and(|character| {
                    character.is_ascii_uppercase() || matches!(character, '_' | '~')
                }) {
            format!("{}::{name}", parts[parts.len() - 2])
        } else {
            (*name).to_owned()
        };
        names.insert(resolved);
    }
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort();
    names
}

fn search_plan_to_storage_filter(
    workspace_root: &Path,
    plan: &SearchPlan,
    storage: &dyn SearchStorage,
) -> Result<Option<StorageSearchFilter>, EngineError> {
    let types = crate::file_selection::file_types(
        &plan.filter.file_types,
        &plan.filter.excluded_file_types,
    )?;
    let has_types =
        !plan.filter.file_types.is_empty() || !plan.filter.excluded_file_types.is_empty();
    let format_path = compile_format_filter(&plan.filter);
    let push_formats = format_path.is_some() && !storage.has_non_unicode_file_names()?;
    let residual_format = (!push_formats).then_some(format_path.as_ref()).flatten();
    let needs_attributes = plan.filter.modified_after_epoch_ms.is_some()
        || plan.filter.modified_before_epoch_ms.is_some();
    let glob_filter = resolve_glob_filter(
        workspace_root,
        &plan.filter.globs,
        || storage.has_non_unicode_file_names(),
        needs_attributes || has_types || residual_format.is_some(),
        || {
            if needs_attributes {
                Ok(storage
                    .list_file_attributes()?
                    .into_iter()
                    .filter(|file| {
                        matches_file_filters(
                            &plan.filter,
                            &types,
                            residual_format,
                            &file.relative_path,
                            file.modified_epoch_ms,
                        )
                    })
                    .map(|file| (file.id, file.relative_path))
                    .collect())
            } else {
                Ok(storage
                    .list_file_paths()?
                    .into_iter()
                    .filter(|(_, path)| {
                        matches_file_filters(&plan.filter, &types, residual_format, path, None)
                    })
                    .collect())
            }
        },
    )?;
    let file_ids = glob_filter.file_ids;
    let path = match (glob_filter.path, format_path.filter(|_| push_formats)) {
        (Some(glob), Some(formats)) => Some(all(vec![glob, formats])),
        (glob, formats) => glob.or(formats),
    };
    let symbol_types =
        (!plan.filter.symbol_types.is_empty()).then(|| plan.filter.symbol_types.clone());
    if file_ids.is_none() && symbol_types.is_none() && path.is_none() {
        Ok(None)
    } else {
        Ok(Some(StorageSearchFilter {
            path,
            file_ids,
            symbol_types,
            ..StorageSearchFilter::default()
        }))
    }
}

fn matches_file_filters(
    filter: &QueryFilter,
    types: &ignore::types::Types,
    format: Option<&StoragePathFilter>,
    relative: &Path,
    modified: Option<u64>,
) -> bool {
    !types.matched(relative, false).is_ignore()
        && filter
            .modified_after_epoch_ms
            .is_none_or(|after| modified.is_some_and(|modified| modified >= after))
        && filter
            .modified_before_epoch_ms
            .is_none_or(|before| modified.is_some_and(|modified| modified <= before))
        && format.is_none_or(|predicate| {
            relative
                .file_name()
                .is_some_and(|name| matches_file_name(predicate, name))
        })
}

#[cfg(test)]
fn resolve_filtered_file_ids(
    workspace_root: &Path,
    plan: &SearchPlan,
    files: &[FileRecord],
) -> Result<Vec<FileId>, EngineError> {
    let matcher = crate::file_selection::GlobMatcher::new(workspace_root, &plan.filter.globs)?;
    let format = compile_format_filter(&plan.filter);
    let types = crate::file_selection::file_types(
        &plan.filter.file_types,
        &plan.filter.excluded_file_types,
    )?;
    Ok(files
        .iter()
        .filter(|file| {
            matcher.matches_path(file.relative_path().as_path())
                && matches_file_filters(
                    &plan.filter,
                    &types,
                    format.as_ref(),
                    file.relative_path().as_path(),
                    file.snapshot().modified_epoch_ms,
                )
        })
        .map(FileRecord::id)
        .collect())
}

fn filter_matches_no_files(filter: Option<&StorageSearchFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter.file_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.path == Some(StoragePathFilter::None)
    })
}

fn rank_as_f64(rank: usize) -> f64 {
    f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
}

fn timing(name: &str, duration: std::time::Duration) -> TimingEntry {
    TimingEntry {
        name: name.to_owned(),
        duration_micros: duration.as_micros().try_into().unwrap_or(u64::MAX),
        count: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap, HashSet},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;

    use crate::{
        EngineResult,
        domain::{
            ByteRange, Content, Entity, EntityFragment, EntityId, FileCategory, FileFormat, FileId,
            FileIndexStatus, FileRecord, FileSnapshot, FragmentId, GlobRule, Range, TextRange,
            model::{EmbeddingMetric, EmbeddingModelInfo},
        },
        file_selection::GlobMatcher,
        models::ModelError,
        pipelines::indexed_search::path_filter::compile_path_filter,
        storage::types::{
            StoragePathFilter, StorageSearchFilter, StorageSearchHit, StorageSearchPath,
            StoredEntity, StoredFileAttributes, StoredSearchData,
        },
    };

    use super::{
        MatchedBy, QueryFilter, SearchEmbeddingRuntime, SearchPlan, SearchRoute, SearchRouteMode,
        SearchStorage, graph_expand, search_workspace_index,
    };

    use crate::{
        EngineError,
        domain::{
            CodeMetadata, EdgeProvenance, EntityMetadata, FileEdge, FileGraphNode, FileGraphResult,
            GraphEdgeKind, SymbolType,
        },
        storage::{
            IndexStore,
            types::{IndexedFragment, WorkspaceIndexStorageOptions},
        },
    };
    use tokio_util::sync::CancellationToken;

    struct GraphFixture {
        root: PathBuf,
        model: FixtureModel,
        graph: IndexStore,
        storage: FixtureStorage,
        entities: Vec<Entity>,
        // The native store must close before its directory is removed.
        _temp: tempfile::TempDir,
    }

    fn graph_entities(file: &FileRecord) -> Vec<Entity> {
        (0..216)
            .map(|index| {
                let stored = entity(file, &format!("fn node_{index}() {{}}"));
                let value = stored.entity;
                Entity::new(
                    value.id().clone(),
                    file.id(),
                    value.source_range(),
                    value.content().clone(),
                    Some(EntityMetadata::Code(CodeMetadata {
                        symbol_type: Some(if index == 1 {
                            SymbolType::Class
                        } else {
                            SymbolType::Function
                        }),
                        symbol_name: Some(if index == 0 {
                            "fixture".into()
                        } else {
                            format!("node_{index}")
                        }),
                        ..CodeMetadata::default()
                    })),
                    value.fragments().to_vec(),
                )
                .expect("code entity")
            })
            .collect()
    }

    fn graph_edges(entities: &[Entity], file_id: FileId) -> Vec<FileEdge> {
        let mut edges = Vec::new();
        let id = |index: usize| entities[index].id().as_str().to_owned();
        let bridge = format!("f{}", file_id.get());
        let mut edge = |kind, source, target, line| {
            edges.push(FileEdge {
                kind,
                source,
                target,
                line: Some(line),
                column: Some(0),
                provenance: EdgeProvenance::FileLocal,
                metadata: BTreeMap::new(),
            });
        };
        edge(GraphEdgeKind::Calls, bridge.clone(), id(0), 1);
        edge(GraphEdgeKind::Calls, bridge, id(1), 2);
        edge(GraphEdgeKind::Calls, id(1), id(2), 3);
        edge(GraphEdgeKind::Calls, id(2), id(3), 4);
        edge(GraphEdgeKind::Calls, id(0), id(0), 5);
        edge(GraphEdgeKind::Calls, id(4), id(5), 6);
        edge(GraphEdgeKind::Calls, id(0), id(4), 8);
        let contained = 211;
        for index in 6..contained {
            for line in 10..18 {
                edge(GraphEdgeKind::Calls, id(0), id(index), line);
            }
        }
        edge(GraphEdgeKind::Calls, id(5), id(212), 30);
        edge(GraphEdgeKind::Calls, id(212), id(213), 31);
        edge(GraphEdgeKind::Calls, id(213), id(214), 32);
        edge(GraphEdgeKind::Extends, id(3), id(0), 20);
        edge(GraphEdgeKind::Contains, id(4), id(contained), 21);
        edge(GraphEdgeKind::Contains, id(1), id(6), 33);
        edges
    }

    impl GraphFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("fixture workspace");
            let root = temp
                .path()
                .canonicalize()
                .expect("canonical fixture workspace");
            let model = FixtureModel::new();
            let graph = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
                graph_enabled: true,
                storage_path: root.join("index"),
                embeddings: vec![model.info().clone()],
            })
            .expect("graph-backed index");
            let file_id = graph
                .resolve_file_ids(&[PathBuf::from("src/lib.rs")])
                .expect("file id")[0];
            let file = file(file_id.get(), "src/lib.rs", 100);
            let entities = graph_entities(&file);
            let fragments = entities
                .iter()
                .map(|entity| IndexedFragment {
                    entity_id: entity.id().clone(),
                    fragment_id: entity.fragments()[0].id.clone(),
                    model: model.info().model().reference(),
                    vector: vec![0.25, 0.75],
                    fts_text: "fixture".into(),
                })
                .collect::<Vec<_>>();
            let nodes = entities
                .iter()
                .map(|entity| FileGraphNode {
                    id: entity.id().as_str().to_owned(),
                    kind: SymbolType::Function,
                    name: None,
                    qualified_name: entity.id().as_str().to_owned(),
                    language: "rust".into(),
                    range: None,
                    signature: None,
                    doc: None,
                    arity: None,
                    visibility: None,
                    is_exported: false,
                    is_declaration: false,
                })
                .collect();
            graph
                .replace_file_with_graph(
                    &file,
                    &entities,
                    &fragments,
                    &FileGraphResult {
                        nodes,
                        edges: graph_edges(&entities, file_id),
                        ..FileGraphResult::default()
                    },
                )
                .expect("persist fixture calls");
            let file = graph
                .read_snapshot(|view| view.fetch_files(&[file_id]))
                .expect("indexed file")
                .remove(&file_id)
                .expect("published file");
            let seeds = [0, 4].map(|index| StoredEntity {
                entity: entities[index].clone(),
                file: file.clone(),
            });
            let storage = FixtureStorage {
                paths_only: false,
                forbid_enumeration: false,
                files: vec![file],
                entities: seeds
                    .iter()
                    .map(|seed| (seed.entity.id().as_str().to_owned(), seed.clone()))
                    .collect(),
                fts: HashMap::from([(
                    "fixture".into(),
                    vec![hit(&seeds[0], 0, StorageSearchPath::Fts, 1.0)],
                )]),
                vector: vec![hit(&seeds[1], 0, StorageSearchPath::Vector, 1.0)],
                reorder_deeper_vector_results: false,
                filters: Arc::default(),
                load_batches: Mutex::default(),
            };
            Self {
                _temp: temp,
                root,
                model,
                graph,
                storage,
                entities,
            }
        }

        fn request() -> SearchPlan {
            let mut request = plan(vec![
                SearchRoute {
                    mode: SearchRouteMode::Fts,
                    query: "fixture".into(),
                },
                SearchRoute {
                    mode: SearchRouteMode::Vector,
                    query: "fixture".into(),
                },
            ]);
            request.limit = Some(1);
            request
        }

        fn id(&self, index: usize) -> &str {
            self.entities[index].id().as_str()
        }

        async fn search(&self, request: SearchPlan, enabled: bool) -> super::SearchPlanResult {
            search_workspace_index(
                &self.root,
                request,
                &self.storage,
                &[&self.model],
                enabled.then_some(super::GraphSearch {
                    storage: &self.graph,
                    signal: None,
                    query: "fixture",
                }),
            )
            .await
            .expect("fixture search")
        }
    }

    #[tokio::test]
    async fn budgeted_graph_reranking_preserves_recall_and_trace_independence() {
        let fixture = GraphFixture::new();
        let request = GraphFixture::request();
        let baseline = fixture
            .search(request.clone(), false)
            .await
            .trace
            .expect("ordinary trace");
        let enabled = fixture.search(request.clone(), true).await;
        assert_eq!(enabled.hits.len(), 1);
        let trace = enabled.trace.as_ref().expect("graph trace");
        assert_eq!(trace.recall, baseline.recall);
        assert_eq!(trace.fused, baseline.fused);
        assert_eq!(trace.ordinary_pool, baseline.ordinary_pool);
        assert!(
            baseline
                .ordinary_pool
                .iter()
                .any(|entity| entity.entity_id == trace.selected[0].entity_id)
        );
        let score = trace.selected[0]
            .trace
            .graph_ranking
            .as_ref()
            .expect("graph score");
        assert_eq!(score.rank, 1);
        assert!(score.graph_normalizer > 0.0);
        assert_eq!(trace.selected[0].trace.final_selection.cutoff_rank, 1);
        assert!(
            enabled
                .timings
                .iter()
                .any(|timing| timing.name == "graph_ranking")
        );
        let mut request = request;
        request.trace = false;
        let untraced = fixture.search(request, true).await;
        assert!(untraced.trace.is_none());
        assert_eq!(untraced.hits[0].entity.id(), enabled.hits[0].entity.id());
        assert!((untraced.hits[0].score - enabled.hits[0].score).abs() < 1e-15);
    }

    #[tokio::test]
    async fn stage_capture_preserves_graph_reranking_and_reports_actual_selection() {
        let fixture = GraphFixture::new();
        let watched = fixture
            .storage
            .files
            .iter()
            .map(|file| file.relative_path().to_path_buf())
            .collect();
        for enabled in [false, true] {
            let request = GraphFixture::request();
            let plain = fixture.search(request.clone(), enabled).await;
            let observed = super::search_with_stage_trace(
                &fixture.root,
                request,
                &fixture.storage,
                &[&fixture.model],
                enabled.then_some(super::GraphSearch {
                    storage: &fixture.graph,
                    signal: None,
                    query: "fixture",
                }),
                Some(&watched),
            )
            .await
            .expect("observed graph search");
            let evidence = |result: &super::SearchPlanResult| {
                let trace = result.trace.as_ref().expect("query trace");
                (
                    trace.recall.clone(),
                    trace.fused.clone(),
                    trace.ordinary_pool.clone(),
                    trace.selected.clone(),
                )
            };
            assert_eq!(evidence(&plain), evidence(&observed));
            let results = |hits: &[super::SearchHit]| {
                hits.iter()
                    .map(|hit| (hit.entity.id().clone(), hit.rank, hit.score))
                    .collect::<Vec<_>>()
            };
            assert_eq!(results(&plain.hits), results(&observed.hits));
            let capture = observed.stage_trace.expect("stage trace");
            for hit in &observed.hits {
                let path = hit
                    .file
                    .relative_path()
                    .to_str()
                    .expect("path")
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let entry = &capture.files[&path][hit.entity.id().as_str()];
                assert_eq!(entry.stages.final_rank, Some(hit.rank));
                assert!(entry.stages.loaded_rrf_rank.is_some());
                assert!(entry.stages.static_rank.is_some());
            }
        }
    }

    #[tokio::test]
    async fn budgeted_graph_keeps_paths_and_stops_at_three_hops() {
        let fixture = GraphFixture::new();
        let result = fixture.search(GraphFixture::request(), true).await;
        let trace = result
            .trace
            .expect("search trace")
            .graph
            .expect("graph trace");
        assert_eq!(trace.error, None);
        assert_eq!(trace.edge_kinds, graph_expand::ALL_EDGES);
        assert_eq!(trace.seeds.len(), 2);
        assert_eq!(trace.layers.len(), 3);
        assert_eq!(
            trace.edges.len(),
            trace
                .layers
                .iter()
                .map(|layer| layer.edges_read - layer.filtered - layer.invisible)
                .sum::<usize>()
        );
        assert!(trace.added.len() <= 200);
        let work = trace.work.expect("work");
        assert!(work.edge_rows_read <= 1500);
        assert_eq!(
            work.edge_rows_read,
            trace
                .layers
                .iter()
                .map(|layer| layer.edges_read)
                .sum::<usize>()
        );
        assert!(work.sqlite_edge_rows_considered.expect("SQL work") > work.edge_rows_read);
        assert!(
            trace.layers[2]
                .reached
                .iter()
                .any(|id| id == fixture.id(213))
        );
        assert!(
            !trace
                .layers
                .iter()
                .flat_map(|layer| &layer.reached)
                .any(|id| id == fixture.id(214))
        );
        assert!(trace.layers.iter().any(|layer| layer.already_visited > 0));
        let inherited = trace
            .added
            .iter()
            .find(|entry| entry.entity.entity_id == fixture.id(3))
            .expect("reverse extends");
        assert_eq!(inherited.path.len(), 1);
        assert_eq!(inherited.path[0].kind, Some(GraphEdgeKind::Extends));
        assert_eq!(inherited.path[0].caller_id, fixture.id(3));
        let budget = trace.budget.expect("budget");
        assert!(budget.reached_pool.len() >= trace.added.len());
        let path = budget
            .retained_paths
            .iter()
            .find(|path| path.path.len() == 2)
            .expect("calls and contains retention");
        assert_eq!(path.path[0].kind, Some(GraphEdgeKind::Calls));
        assert_eq!(path.path[1].kind, Some(GraphEdgeKind::Contains));
        assert!(path.candidate_ids.iter().all(|id| {
            trace
                .added
                .iter()
                .any(|entry| &entry.entity.entity_id == id)
        }));
    }

    #[tokio::test]
    async fn graph_reranking_respects_symbol_filters() {
        let fixture = GraphFixture::new();
        let mut request = GraphFixture::request();
        request.filter.symbol_types = vec![SymbolType::Function];
        let result = fixture.search(request, true).await;
        let trace = result
            .trace
            .expect("search trace")
            .graph
            .expect("graph trace");
        let filtered: usize = trace.layers.iter().map(|layer| layer.filtered).sum();
        assert!(filtered >= 2);
        assert_eq!(trace.rejected.len(), 1);
        assert_eq!(trace.rejected[0].entity.entity_id, fixture.id(1));
        assert!(
            !trace
                .added
                .iter()
                .any(|entry| entry.entity.entity_id == fixture.id(1))
        );
        assert!(
            trace
                .edges
                .iter()
                .all(|edge| { edge.caller_id != fixture.id(1) && edge.callee_id != fixture.id(1) })
        );
    }

    fn walk_trace_paths(
        node: &str,
        mass: f64,
        depth: usize,
        neighbors: &BTreeMap<&str, BTreeSet<&str>>,
        scores: &mut BTreeMap<String, f64>,
    ) {
        if depth == 3 {
            return;
        }
        let Some(targets) = neighbors.get(node) else {
            return;
        };
        let mass = mass / f64::from(u32::try_from(targets.len()).expect("fixture degree"));
        for target in targets {
            *scores.entry(target.to_string()).or_default() += mass * [1.0, 0.5, 0.25][depth];
            walk_trace_paths(target, mass, depth + 1, neighbors, scores);
        }
    }

    fn replay_graph_scores(
        trace: &crate::api::context::result::GroupSearchTrace,
    ) -> BTreeMap<String, f64> {
        let graph = trace.graph.as_ref().expect("graph trace");
        let mut neighbors = BTreeMap::<&str, BTreeSet<&str>>::new();
        for edge in &graph.edges {
            if edge.caller_id != edge.callee_id {
                neighbors
                    .entry(&edge.caller_id)
                    .or_default()
                    .insert(&edge.callee_id);
                neighbors
                    .entry(&edge.callee_id)
                    .or_default()
                    .insert(&edge.caller_id);
            }
        }
        let seed_scores: HashMap<_, _> = trace
            .fused
            .iter()
            .map(|candidate| (candidate.entity_id.as_str(), candidate.score))
            .collect();
        let mut scores = BTreeMap::new();
        // Enumerate each seed's paths independently of the production aggregate walk.
        for seed in &graph.seeds {
            walk_trace_paths(seed, seed_scores[seed.as_str()], 0, &neighbors, &mut scores);
        }
        scores
    }

    #[tokio::test]
    async fn graph_scores_can_be_replayed_from_the_serialized_response() {
        let fixture = GraphFixture::new();
        let result = fixture.search(GraphFixture::request(), true).await;
        let json =
            serde_json::to_vec(&result.trace.expect("search trace")).expect("serialize trace");
        let trace: crate::api::context::result::GroupSearchTrace =
            serde_json::from_slice(&json).expect("response trace");
        let graph = trace.graph.as_ref().expect("graph trace");
        assert_eq!(
            graph
                .edges
                .iter()
                .filter(|edge| {
                    edge.caller_id == fixture.id(0) && edge.callee_id == fixture.id(4)
                })
                .count(),
            2
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.caller_id == edge.callee_id)
        );
        let scores = replay_graph_scores(&trace);
        let candidates: BTreeSet<_> = trace
            .ordinary_pool
            .iter()
            .map(|entity| entity.entity_id.as_str())
            .chain(
                graph
                    .added
                    .iter()
                    .map(|candidate| candidate.entity.entity_id.as_str()),
            )
            .collect();
        let normalizer: f64 = candidates
            .iter()
            .map(|id| scores.get(*id).copied().unwrap_or(0.0))
            .sum();
        let ordinary_normalizer: f64 = (1..=trace.ordinary_pool.len())
            .map(|rank| 1.0 / (60.0 + f64::from(u32::try_from(rank).expect("fixture rank"))))
            .sum();
        assert!(normalizer > 0.0);
        for hit in &trace.selected {
            let ranking = hit.trace.graph_ranking.as_ref().expect("graph ranking");
            let raw = scores.get(&hit.entity_id).copied().unwrap_or(0.0);
            assert!((raw - ranking.graph_raw_score).abs() < 1e-12);
            assert!((normalizer - ranking.graph_normalizer).abs() < 1e-12);
            let graph_score = raw / normalizer;
            let ordinary_score = 1.0
                / (60.0 + f64::from(u32::try_from(ranking.ordinary_rank).expect("rank")))
                / ordinary_normalizer;
            assert!((graph_score - ranking.graph_score).abs() < 1e-12);
            assert!((ordinary_score - ranking.ordinary_score).abs() < 1e-12);
            assert!((ordinary_score.midpoint(graph_score) - ranking.score).abs() < 1e-12);
        }
        let mut missing_seed_edges = trace.clone();
        missing_seed_edges
            .graph
            .as_mut()
            .expect("graph")
            .edges
            .retain(|edge| {
                !((edge.caller_id == fixture.id(0) && edge.callee_id == fixture.id(4))
                    || (edge.caller_id == fixture.id(4) && edge.callee_id == fixture.id(0)))
            });
        let incomplete = replay_graph_scores(&missing_seed_edges);
        assert!(trace.selected.iter().any(|hit| {
            (incomplete.get(&hit.entity_id).copied().unwrap_or(0.0) - scores[&hit.entity_id]).abs()
                > 1e-6
        }));
    }

    #[test]
    fn cancellation_inside_sql_keeps_the_engine_cancelled_error_code() {
        let fixture = GraphFixture::new();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let error = fixture
            .graph
            .read_snapshot(|view| {
                view.with_edge_priority(
                    move |_, _, _| {
                        cancel.cancel();
                        None
                    },
                    || view.ranked_hop(&[fixture.id(0)], 600, Some(&signal)),
                )
            })
            .expect_err("cancel from a running SQLite priority callback");
        assert!(signal.is_cancelled());
        assert_eq!(error.code(), EngineError::CANCELLED);
    }

    #[tokio::test]
    async fn empty_graph_seeds_skip_symbol_projection() {
        let mut fixture = GraphFixture::new();
        fixture.storage.fts.clear();
        fixture.storage.vector.clear();
        let result = fixture.search(GraphFixture::request(), true).await;
        assert!(result.hits.is_empty());
        assert!(
            !result
                .timings
                .iter()
                .any(|timing| timing.name == "graph_ranking")
        );
        let trace = result
            .trace
            .expect("search trace")
            .graph
            .expect("graph attempt");
        assert!(trace.seeds.is_empty());
        let work = trace.work.expect("empty work");
        assert_eq!(work.symbol_rows_read, 0);
        assert_eq!(work.edge_rows_read, 0);
    }

    #[tokio::test]
    async fn unavailable_graph_preserves_ordinary_results_and_cancellation_propagates() {
        let fixture = GraphFixture::new();
        let baseline = fixture
            .search(GraphFixture::request(), false)
            .await
            .trace
            .expect("ordinary trace");
        let graph = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            graph_enabled: false,
            storage_path: fixture.root.join("missing-graph"),
            embeddings: vec![fixture.model.info().clone()],
        })
        .expect("text-only store");
        let result = search_workspace_index(
            &fixture.root,
            GraphFixture::request(),
            &fixture.storage,
            &[&fixture.model],
            Some(super::GraphSearch {
                storage: &graph,
                signal: None,
                query: "fixture",
            }),
        )
        .await
        .expect("ordinary results survive graph failure");
        assert!(result.graph_error.is_some());
        assert_eq!(
            result.trace.expect("search trace").selected,
            baseline.selected
        );
        let signal = CancellationToken::new();
        signal.cancel();
        let error = search_workspace_index(
            &fixture.root,
            GraphFixture::request(),
            &fixture.storage,
            &[&fixture.model],
            Some(super::GraphSearch {
                storage: &fixture.graph,
                signal: Some(&signal),
                query: "fixture",
            }),
        )
        .await
        .expect_err("cancelled graph request");
        assert_eq!(error.code(), EngineError::CANCELLED);
    }

    struct FixtureModel {
        info: EmbeddingModelInfo,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FixtureModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo::new(
                    crate::domain::model::ModelInfo::new(
                        "local".to_owned(),
                        "fixture".to_owned(),
                        [
                            crate::domain::ContentKind::Text,
                            crate::domain::ContentKind::Code,
                            crate::domain::ContentKind::Image,
                        ],
                        8,
                    )
                    .expect("valid model metadata"),
                    2,
                    EmbeddingMetric::Cosine,
                    None,
                    None,
                )
                .expect("valid embedding metadata"),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl SearchEmbeddingRuntime for FixtureModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            self.calls
                .lock()
                .expect("query call mutex")
                .push(queries.to_vec());
            Ok(queries.iter().map(|_| vec![0.25, 0.75]).collect())
        }
    }

    struct FixtureStorage {
        paths_only: bool,
        forbid_enumeration: bool,
        files: Vec<FileRecord>,
        entities: HashMap<String, StoredEntity>,
        fts: HashMap<String, Vec<FixtureHit>>,
        vector: Vec<FixtureHit>,
        reorder_deeper_vector_results: bool,
        filters: Arc<Mutex<Vec<Option<StorageSearchFilter>>>>,
        load_batches: Mutex<Vec<Vec<StorageSearchHit>>>,
    }

    struct FixtureHit {
        hit: StorageSearchHit,
        fragment: EntityFragment,
    }

    impl FixtureStorage {
        fn hits_with_filter(
            &self,
            hits: &[FixtureHit],
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> Vec<StorageSearchHit> {
            self.filters
                .lock()
                .expect("filter mutex")
                .push(filter.cloned());
            hits.iter()
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.file_ids.as_ref())
                        .is_none_or(|ids| ids.contains(&hit.hit.file_id))
                })
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.entity_ids.as_ref())
                        .is_none_or(|ids| ids.contains(&hit.hit.entity_id))
                })
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.path.as_ref())
                        .is_none_or(|predicate| {
                            self.files
                                .iter()
                                .find(|file| file.id() == hit.hit.file_id)
                                .is_some_and(|file| {
                                    predicate_matches(predicate, file.relative_path().as_path())
                                })
                        })
                })
                .take(limit)
                .map(|hit| hit.hit.clone())
                .collect()
        }
    }

    impl SearchStorage for FixtureStorage {
        fn indexed_sources(
            &self,
            paths: &HashSet<PathBuf>,
        ) -> EngineResult<Vec<crate::storage::IndexedSourceFile>> {
            Ok(self
                .files
                .iter()
                .filter(|file| paths.contains(file.relative_path().as_path()))
                .map(|file| crate::storage::IndexedSourceFile {
                    id: file.id(),
                    path: file.relative_path().to_path_buf(),
                    entities: self
                        .entities
                        .values()
                        .filter(|stored| stored.file.id() == file.id())
                        .map(|stored| {
                            (
                                stored.entity.id().clone(),
                                stored.entity.source_range(),
                                true,
                            )
                        })
                        .collect(),
                })
                .collect())
        }

        fn list_file_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>> {
            assert!(!self.paths_only, "name filters must only read paths");
            assert!(
                !self.forbid_enumeration,
                "pushdown must not enumerate attributes"
            );
            Ok(self.files.iter().map(StoredFileAttributes::from).collect())
        }

        fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
            assert!(!self.forbid_enumeration, "pushdown must not scan paths");
            Ok(self
                .files
                .iter()
                .map(|file| (file.id(), file.relative_path().to_path_buf()))
                .collect())
        }

        fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
            Ok(self.files.iter().any(|file| {
                file.relative_path()
                    .file_name()
                    .is_some_and(|name| name.to_str().is_none())
            }))
        }

        fn load_search_hits(&self, hits: &[StorageSearchHit]) -> EngineResult<StoredSearchData> {
            self.load_batches
                .lock()
                .expect("load batches")
                .push(hits.to_vec());
            let mut data = StoredSearchData::default();
            for hit in hits {
                if let Some(entity) = self.entities.get(hit.entity_id.as_str()) {
                    let mut stored = entity.clone();
                    let mut seen = std::collections::HashSet::new();
                    let fragments = self
                        .fts
                        .values()
                        .flatten()
                        .chain(&self.vector)
                        .filter(|found| found.hit.entity_id == hit.entity_id)
                        .filter(|found| seen.insert(found.hit.document_id.clone()))
                        .map(|found| EntityFragment {
                            id: FragmentId::from_string(found.hit.document_id.clone()),
                            range: Range::Full,
                        })
                        .collect();
                    stored.entity = Entity::new(
                        stored.entity.id().clone(),
                        stored.entity.file_id(),
                        stored.entity.source_range(),
                        stored.entity.content().clone(),
                        stored.entity.metadata().cloned(),
                        fragments,
                    )?;
                    data.entities.insert(hit.entity_id.clone(), stored);
                }
                if let Some(found) = self
                    .fts
                    .values()
                    .flatten()
                    .chain(&self.vector)
                    .find(|found| found.hit.document_id == hit.document_id)
                {
                    data.fragments
                        .insert(hit.document_id.clone(), found.fragment.clone());
                }
            }
            Ok(data)
        }

        fn search_fts(
            &self,
            query: &str,
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> EngineResult<Vec<StorageSearchHit>> {
            Ok(self.hits_with_filter(
                self.fts.get(query).map_or(&[], Vec::as_slice),
                limit,
                filter,
            ))
        }

        fn search_vector(
            &self,
            _model: &str,
            _vector: &[f32],
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> EngineResult<Vec<StorageSearchHit>> {
            let mut hits = self.hits_with_filter(&self.vector, limit, filter);
            if self.reorder_deeper_vector_results && limit > super::RECALL_INITIAL_DEPTH {
                hits.reverse();
            }
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn promotes_symbol_definition_before_result_cutoff() {
        let reference_file = file(1, "src/plugin.ts", 100);
        let definition_file = file(2, "src/core.ts", 100);
        let mut reference = entity(&reference_file, "vitest: Vitest");
        let mut definition = entity(&definition_file, "class Vitest {}");
        let definition_id = definition.entity.id().clone();
        for (stored, kind, name) in [
            (&mut reference, crate::domain::SymbolType::Value, "vitest"),
            (&mut definition, crate::domain::SymbolType::Class, "Vitest"),
        ] {
            stored.entity = Entity::new(
                stored.entity.id().clone(),
                stored.entity.file_id(),
                stored.entity.source_range(),
                stored.entity.content().clone(),
                Some(crate::domain::EntityMetadata::Code(
                    crate::domain::CodeMetadata {
                        symbol_type: Some(kind),
                        symbol_name: Some(name.to_owned()),
                        scope: None,
                        signature: None,
                        documentation: None,
                        visibility: None,
                        language: None,
                        arity: None,
                        is_exported: false,
                        is_declaration: false,
                        package: None,
                        has_internal_linkage: false,
                        declares_type: false,
                        alias_of: None,
                        object_members: Vec::new(),
                        overwritten_by: None,
                        exported_as: Vec::new(),
                    },
                )),
                stored.entity.fragments().to_vec(),
            )
            .expect("entity");
        }
        let mut storage = pushdown_storage();
        storage.files = vec![reference_file, definition_file];
        storage.fts.insert(
            "Vitest".to_owned(),
            vec![
                hit(&reference, 0, StorageSearchPath::Fts, 1.0),
                hit(&definition, 0, StorageSearchPath::Fts, 0.9),
            ],
        );
        storage
            .entities
            .insert(reference.entity.id().as_str().to_owned(), reference);
        storage
            .entities
            .insert(definition.entity.id().as_str().to_owned(), definition);
        let mut request = plan(vec![SearchRoute {
            mode: SearchRouteMode::Fts,
            query: "Vitest".to_owned(),
        }]);
        request.limit = Some(1);
        let root = tempfile::tempdir().expect("workspace");
        let result = search_workspace_index(root.path(), request, &storage, &[], None)
            .await
            .expect("search");
        assert_eq!(result.hits[0].entity.id(), &definition_id);
    }

    #[tokio::test]
    async fn fuses_fts_and_vector_routes_with_main_compatible_rrf() {
        let file = file(1, "src/lib.rs", 100);
        let entity_a = entity(&file, "alpha entity");
        let entity_b = entity(&file, "beta entity");
        let a_fts = hit(&entity_a, 0, StorageSearchPath::Fts, 9.0);
        let b_fts = hit(&entity_b, 0, StorageSearchPath::Fts, 8.0);
        let b_vector = hit(&entity_b, 1, StorageSearchPath::Vector, 0.9);
        let a_vector = hit(&entity_a, 1, StorageSearchPath::Vector, 0.8);
        let storage = FixtureStorage {
            paths_only: false,
            forbid_enumeration: false,
            files: vec![file],
            entities: HashMap::from([
                (entity_a.entity.id().as_str().to_owned(), entity_a.clone()),
                (entity_b.entity.id().as_str().to_owned(), entity_b.clone()),
            ]),
            fts: HashMap::from([("alpha".to_owned(), vec![a_fts, b_fts])]),
            vector: vec![b_vector, a_vector],
            reorder_deeper_vector_results: false,
            filters: Arc::new(Mutex::new(Vec::new())),
            load_batches: Mutex::default(),
        };
        let model = FixtureModel::new();

        let result = search_workspace_index(
            Path::new("/workspace"),
            plan(vec![
                SearchRoute {
                    mode: SearchRouteMode::Fts,
                    query: "alpha".to_owned(),
                },
                SearchRoute {
                    mode: SearchRouteMode::Vector,
                    query: "alpha".to_owned(),
                },
            ]),
            &storage,
            &[&model],
            None,
        )
        .await
        .expect("hybrid search");

        assert_eq!(result.hits.len(), 2);
        let mut expected_ids = [entity_a.entity.id(), entity_b.entity.id()];
        expected_ids.sort();
        assert_eq!(result.hits[0].entity.id(), expected_ids[0]);
        assert_eq!(result.hits[1].entity.id(), expected_ids[1]);
        assert!(
            result
                .hits
                .iter()
                .all(|hit| hit.matched_by == MatchedBy::FtsAndVector)
        );
        assert_eq!(result.hits[0].evidence.len(), 2);
        assert_eq!(
            result.hits[0]
                .trace
                .as_ref()
                .map(|trace| trace.recall.len()),
            Some(2)
        );
        assert_eq!(
            model.calls.lock().expect("query calls").as_slice(),
            &[vec!["alpha".to_owned()]]
        );
    }

    #[tokio::test]
    async fn stage_observation_preserves_results_with_arbitrary_limits_and_routes() {
        let first = file(1, "src/a.rs", 100);
        let second = file(2, "src/b.rs", 100);
        let entries = [
            entity(&first, "first"),
            entity(&first, "second"),
            entity(&second, "third"),
        ];
        let mut storage = pushdown_storage();
        storage.files = vec![first, second];
        storage.fts.insert(
            "Needle".to_owned(),
            entries
                .iter()
                .map(|entry| hit(entry, 0, StorageSearchPath::Fts, 1.0))
                .collect(),
        );
        storage.vector = entries
            .iter()
            .rev()
            .map(|entry| hit(entry, 0, StorageSearchPath::Vector, 1.0))
            .collect();
        storage.entities = entries
            .into_iter()
            .map(|entry| (entry.entity.id().as_str().to_owned(), entry))
            .collect();
        let watched = [PathBuf::from("src/a.rs"), PathBuf::from("missing.rs")].into();
        let model = FixtureModel::new();
        for modes in [
            vec![SearchRouteMode::Fts],
            vec![SearchRouteMode::Vector],
            vec![SearchRouteMode::Fts, SearchRouteMode::Vector],
            vec![SearchRouteMode::Fts, SearchRouteMode::Fts],
        ] {
            for prefer_symbol in [false, true] {
                let mut request = plan(
                    modes
                        .iter()
                        .map(|mode| SearchRoute {
                            mode: *mode,
                            query: "Needle".to_owned(),
                        })
                        .collect(),
                );
                request.limit = Some(2);
                request.prefer_symbol = prefer_symbol;
                let off = super::search_with_stage_trace(
                    Path::new("/workspace"),
                    request.clone(),
                    &storage,
                    &[&model],
                    None,
                    None,
                )
                .await
                .expect("ordinary search");
                let on = super::search_with_stage_trace(
                    Path::new("/workspace"),
                    request,
                    &storage,
                    &[&model],
                    None,
                    Some(&watched),
                )
                .await
                .expect("observed search");
                let results = |result: &super::SearchPlanResult| {
                    result.hits.iter().map(|hit| serde_json::json!({
                    "id": hit.entity.id().as_str(), "rank": hit.rank, "score": hit.score,
                    "matched_by": hit.matched_by, "trace": hit.trace,
                })).collect::<Vec<_>>()
                };
                assert_eq!(results(&off), results(&on));
                assert!(off.stage_trace.is_none());
                let trace = on.stage_trace.expect("capture");
                assert_eq!(trace.limit, 2);
                assert_eq!(trace.watched_paths, ["missing.rs", "src/a.rs"]);
                assert!(trace.index_observed);
                assert_eq!(trace.sizes.selected, 2);
                assert_eq!(trace.sizes.loaded, 3);
                assert_eq!(trace.files.len(), 1);
                assert!(
                    trace.files["src/a.rs"]
                        .values()
                        .all(|entry| entry.stages.static_rank.is_some())
                );
                assert!(
                    trace
                        .routes
                        .values()
                        .all(|route| !route.attempts.is_empty())
                );
                let encoded = serde_json::to_value(&trace).expect("encode trace");
                assert!(encoded.get("id").is_none());
                assert_eq!(
                    serde_json::from_value::<crate::api::context::stage_trace::StageTrace>(encoded)
                        .expect("roundtrip"),
                    trace
                );
            }
        }
    }

    #[test]
    fn keeps_best_recall_evidence_separate_for_each_route() {
        let owner = entity(&file(1, "src/lib.rs", 100), "source");
        let first = hit(&owner, 0, StorageSearchPath::Fts, 3.0).hit;
        let later = hit(&owner, 1, StorageSearchPath::Fts, 2.0).hit;
        let improved = hit(&owner, 1, StorageSearchPath::Fts, 4.0).hit;
        let route = super::ResolvedSearchRoute {
            id: "first".into(),
            mode: SearchRouteMode::Fts,
            query: "first query".into(),
        };
        let mut candidates = HashMap::new();
        for hits in [
            [first.clone(), later.clone()],
            [improved.clone(), first.clone()],
            [first, later],
        ] {
            super::add_recall_hits(&mut candidates, &hits, &route).expect("recall round");
        }
        let other_route = super::ResolvedSearchRoute {
            id: "second".into(),
            query: "second query".into(),
            ..route
        };
        super::add_recall_hits(
            &mut candidates,
            std::slice::from_ref(&improved),
            &other_route,
        )
        .expect("independent route");

        let candidate = &candidates[owner.entity.id()];
        assert_eq!(candidate.evidence.len(), 3);
        let evidence = candidate
            .evidence
            .iter()
            .filter(|evidence| evidence.hit.document_id == improved.document_id)
            .collect::<Vec<_>>();
        assert_eq!(evidence.len(), 2);
        assert_ne!(evidence[0].route_id, evidence[1].route_id);
        for evidence in evidence {
            assert_eq!(evidence.rank, 1);
            assert!((evidence.hit.score - improved.score).abs() < f64::EPSILON);
        }
        assert_eq!(candidate.recall.len(), 2);
    }

    #[tokio::test]
    async fn loads_reordered_recall_candidates_once_before_rule_ranking() {
        let source = file(1, "src/lib.rs", 100);
        let selected = entity(&source, "selected source");
        let discarded = entity(&source, "discarded source");
        let discarded_id = discarded.entity.id().clone();
        let mut vector = (0..205)
            .map(|index| hit(&selected, index, StorageSearchPath::Vector, 1.0))
            .collect::<Vec<_>>();
        vector.push(hit(&discarded, 0, StorageSearchPath::Vector, 1.0));
        let mut storage = pushdown_storage();
        storage.files.push(source.clone());
        storage
            .entities
            .insert(selected.entity.id().as_str().to_owned(), selected.clone());
        storage
            .entities
            .insert(discarded.entity.id().as_str().to_owned(), discarded);
        storage.fts.insert(
            "query".to_owned(),
            vec![hit(&selected, 0, StorageSearchPath::Fts, 1.0)],
        );
        storage.vector = vector;
        storage.reorder_deeper_vector_results = true;
        let mut plan = plan(vec![
            SearchRoute {
                mode: SearchRouteMode::Fts,
                query: "query".to_owned(),
            },
            SearchRoute {
                mode: SearchRouteMode::Vector,
                query: "query".to_owned(),
            },
        ]);
        plan.limit = Some(1);
        plan.filter.excluded_formats = vec![FileFormat::Jpeg];

        let result = search_workspace_index(
            Path::new("/workspace"),
            plan,
            &storage,
            &[&FixtureModel::new()],
            None,
        )
        .await
        .expect("search");

        assert_eq!(result.hits.len(), 1);
        let selected_hit = &result.hits[0];
        assert_eq!(selected_hit.entity.id(), selected.entity.id());
        assert_eq!(
            selected_hit
                .evidence
                .iter()
                .map(|evidence| &evidence.fragment.id)
                .collect::<HashSet<_>>()
                .len(),
            205,
            "deeper top-k must retain fragments that enter the old prefix",
        );
        assert_eq!(selected_hit.evidence.len(), 206);
        assert_eq!(
            selected_hit.evidence[0].fragment.id,
            FragmentId::new(selected.entity.id(), 0)
        );
        assert_eq!(
            selected_hit
                .evidence
                .iter()
                .filter(|evidence| evidence.fragment.id == FragmentId::new(selected.entity.id(), 0))
                .count(),
            2,
            "the same fragment remains evidence for both routes",
        );
        let trace = selected_hit.trace.as_ref().expect("trace");
        assert_eq!(trace.recall.len(), 2);
        assert!(trace.recall.iter().all(|recall| recall.rank == Some(1)));
        let batches = storage.load_batches.lock().expect("load batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 206);
        assert_eq!(
            batches[0]
                .iter()
                .filter(|hit| &hit.entity_id == selected.entity.id())
                .count(),
            205
        );
        assert!(batches[0].iter().any(|hit| hit.entity_id == discarded_id));
        assert_eq!(storage.filters.lock().expect("filters").len(), 4);
    }

    #[tokio::test]
    async fn skips_missing_entities_but_rejects_mismatched_fragment_ownership() {
        let source = file(1, "src/lib.rs", 100);
        let stored = entity(&source, "source");
        for missing in [true, false] {
            let mut storage = pushdown_storage();
            let mut recalled = hit(&stored, 0, StorageSearchPath::Fts, 1.0);
            if !missing {
                storage
                    .entities
                    .insert(stored.entity.id().as_str().to_owned(), stored.clone());
                recalled.fragment.range =
                    Range::Byte(ByteRange::new(1, 3).expect("ordered byte offsets"));
            }
            storage.fts.insert("query".to_owned(), vec![recalled]);
            let plan = plan(vec![SearchRoute {
                mode: SearchRouteMode::Fts,
                query: "query".to_owned(),
            }]);
            let result =
                search_workspace_index(Path::new("/workspace"), plan, &storage, &[], None).await;
            if missing {
                assert!(result.expect("interrupted replacement").hits.is_empty());
            } else {
                assert!(
                    result
                        .expect_err("invalid ownership")
                        .to_string()
                        .contains("inconsistent ownership")
                );
            }
        }
    }

    #[test]
    fn skips_missing_fragments_without_discarding_other_evidence() {
        let source = file(1, "src/lib.rs", 100);
        let stored = entity(&source, "source");
        let retained = hit(&stored, 0, StorageSearchPath::Fts, 2.0);
        let removed = hit(&stored, 1, StorageSearchPath::Fts, 1.0);
        for keep_fragment in [false, true] {
            let mut data = StoredSearchData::default();
            data.entities
                .insert(stored.entity.id().clone(), stored.clone());
            if keep_fragment {
                data.fragments
                    .insert(retained.hit.document_id.clone(), retained.fragment.clone());
            }
            let candidate = super::Candidate {
                id: stored.entity.id().clone(),
                file_id: source.id(),
                sources: std::collections::HashSet::from([SearchRouteMode::Fts]),
                recall: Vec::new(),
                evidence: [&retained, &removed]
                    .into_iter()
                    .enumerate()
                    .map(|(rank, hit)| super::InternalEvidence {
                        hit: hit.hit.clone(),
                        path: SearchRouteMode::Fts,
                        route_id: "fts".to_owned(),
                        rank: rank + 1,
                    })
                    .collect(),
                score: 1.0,
                rank: 1,
            };
            let result =
                super::candidate_to_hit(candidate, 1, false, &mut data).expect("partial load");
            if keep_fragment {
                let result = result.expect("surviving evidence");
                assert_eq!(result.evidence.len(), 1);
                assert_eq!(result.evidence[0].fragment.id, retained.fragment.id);
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[tokio::test]
    async fn multiple_models_are_rejected_before_query_embedding() {
        let source = file(1, "src/service.rs", 200);
        let owner = entity(&source, "Service implementation");
        let storage = FixtureStorage {
            paths_only: false,
            forbid_enumeration: false,
            files: vec![source],
            entities: HashMap::from([(owner.entity.id().as_str().to_owned(), owner.clone())]),
            fts: HashMap::from([(
                "Service".to_owned(),
                vec![hit(&owner, 0, StorageSearchPath::Fts, 2.0)],
            )]),
            vector: Vec::new(),
            reorder_deeper_vector_results: false,
            filters: Arc::new(Mutex::new(Vec::new())),
            load_batches: Mutex::default(),
        };
        let first = FixtureModel::new();
        let mut second = FixtureModel::new();
        {
            let mut metadata = serde_json::to_value(&second.info).expect("model metadata");
            metadata["model"]["name"] = serde_json::json!("second");
            second.info = serde_json::from_value(metadata).expect("valid model metadata");
        }
        let mut query = plan(vec![SearchRoute {
            mode: SearchRouteMode::Vector,
            query: "Service".to_owned(),
        }]);
        query.prefer_symbol = true;
        let error = search_workspace_index(
            Path::new("/workspace"),
            query,
            &storage,
            &[&first, &second],
            None,
        )
        .await
        .expect_err("multiple models are unsupported");
        assert!(error.message().contains("only one embedding model"));
        assert!(first.calls.lock().expect("first model calls").is_empty());
        assert!(second.calls.lock().expect("second model calls").is_empty());
    }

    #[tokio::test]
    async fn pushes_file_and_symbol_filters_into_storage() {
        let source = file(1, "src/service.rs", 200);
        let docs = file(2, "docs/service.md", 200);
        let source_entity = entity(&source, "Service implementation");
        let docs_entity = entity(&docs, "Service documentation");
        let filters = Arc::new(Mutex::new(Vec::new()));
        let storage = FixtureStorage {
            paths_only: false,
            forbid_enumeration: false,
            files: vec![source.clone(), docs.clone()],
            entities: HashMap::from([
                (
                    source_entity.entity.id().as_str().to_owned(),
                    source_entity.clone(),
                ),
                (
                    docs_entity.entity.id().as_str().to_owned(),
                    docs_entity.clone(),
                ),
            ]),
            fts: HashMap::from([(
                "Service".to_owned(),
                vec![
                    hit(&source_entity, 0, StorageSearchPath::Fts, 2.0),
                    hit(&docs_entity, 0, StorageSearchPath::Fts, 1.0),
                ],
            )]),
            vector: Vec::new(),
            reorder_deeper_vector_results: false,
            filters: Arc::clone(&filters),
            load_batches: Mutex::default(),
        };
        let mut plan = plan(vec![SearchRoute {
            mode: SearchRouteMode::Fts,
            query: "Service".to_owned(),
        }]);
        plan.filter.globs = glob_rules(&["src/**"]);
        plan.filter.included_formats = vec![FileFormat::Rust];
        plan.prefer_symbol = true;

        let result = search_workspace_index(Path::new("/workspace"), plan, &storage, &[], None)
            .await
            .expect("filtered search");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].file.id(), source.id());
        let filters = filters.lock().expect("captured filters");
        assert!(
            filters
                .iter()
                .flatten()
                .all(|filter| selected_file_ids(&storage, filter) == [source.id()])
        );
        assert!(filters.iter().flatten().any(|filter| {
            filter
                .symbol_names
                .as_ref()
                .is_some_and(|names| names.contains(&"Service".to_owned()))
        }));
    }

    #[test]
    fn resolves_globs_relative_to_the_current_workspace_root() {
        let source = file(1, "src/service.rs", 200);
        let files = [source.clone()];
        let mut plan = plan(Vec::new());
        plan.filter.globs = glob_rules(&["/src/**/*.rs"]);
        for root in ["/original/workspace", "/moved/workspace"] {
            assert_eq!(
                super::resolve_filtered_file_ids(Path::new(root), &plan, &files)
                    .expect("relative glob"),
                [source.id()]
            );
        }
        plan.filter.globs.push(GlobRule {
            pattern: "!src/**".into(),
            case_insensitive: false,
        });
        assert!(
            super::resolve_filtered_file_ids(Path::new("/moved/workspace"), &plan, &files)
                .expect("excluded directory")
                .is_empty()
        );
    }

    fn selected_file_ids(storage: &FixtureStorage, filter: &StorageSearchFilter) -> Vec<FileId> {
        storage
            .files
            .iter()
            .filter(|file| {
                filter
                    .file_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&file.id()))
                    && filter.path.as_ref().is_none_or(|predicate| {
                        predicate_matches(predicate, file.relative_path().as_path())
                    })
            })
            .map(FileRecord::id)
            .collect()
    }

    fn pushdown_storage() -> FixtureStorage {
        FixtureStorage {
            paths_only: false,
            forbid_enumeration: true,
            files: Vec::new(),
            entities: HashMap::new(),
            fts: HashMap::new(),
            vector: Vec::new(),
            reorder_deeper_vector_results: false,
            filters: Arc::default(),
            load_batches: Mutex::default(),
        }
    }

    #[test]
    fn ripgrep_types_intersect_globs_and_use_the_path_projection() {
        let mut storage = pushdown_storage();
        storage.forbid_enumeration = false;
        storage.paths_only = true;
        storage.files = vec![
            file(1, "src/a.h", 1),
            file(2, "src/b.hpp", 1),
            file(3, "src/c.cpp", 1),
            file(4, "other.h", 1),
        ];
        let mut plan = plan(Vec::new());
        plan.filter.file_types = vec!["h".into()];
        plan.filter.globs = glob_rules(&["src/**"]);
        let filter = super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
            .expect("types")
            .expect("filter");
        assert_eq!(
            selected_file_ids(&storage, &filter),
            vec![storage.files[0].id(), storage.files[1].id()]
        );
        plan.filter.excluded_file_types = vec!["h".into()];
        let filter = super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
            .expect("types")
            .expect("filter");
        assert!(selected_file_ids(&storage, &filter).is_empty());
        plan.filter.file_types = vec!["nonexistent-type".into()];
        assert!(
            super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage).is_err()
        );
    }

    #[test]
    fn complex_globs_only_read_the_path_projection() {
        let mut storage = pushdown_storage();
        storage.forbid_enumeration = false;
        storage.paths_only = true;
        storage.files = vec![
            file(1, "src/engine/tests/a.rs", 10),
            file(2, "src/engine/internal/tests/b.rs", 10),
            file(3, "src/other/tests/a.md", 10),
        ];
        let mut plan = plan(Vec::new());
        plan.filter.globs = glob_rules(&["src/*/tests/*.rs"]);
        let filter = super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
            .expect("path filter")
            .expect("filter present");
        assert_eq!(filter.file_ids, Some(vec![storage.files[0].id()]));
    }

    #[test]
    fn format_filters_push_down_without_enumerating_files() {
        let storage = pushdown_storage();
        for query in [
            QueryFilter {
                included_formats: vec![FileFormat::Rust],
                ..Default::default()
            },
            QueryFilter {
                included_formats: vec![FileFormat::Jpeg],
                ..Default::default()
            },
            QueryFilter {
                excluded_formats: vec![FileFormat::Rust],
                ..Default::default()
            },
            QueryFilter {
                categories: vec![FileCategory::Code],
                excluded_categories: vec![FileCategory::Document],
                ..Default::default()
            },
            QueryFilter {
                included_formats: vec![FileFormat::Unknown],
                ..Default::default()
            },
        ] {
            let mut plan = plan(Vec::new());
            plan.filter = query;
            let filter =
                super::search_plan_to_storage_filter(Path::new("/missing"), &plan, &storage)
                    .expect("pushdown")
                    .expect("filter");
            assert!(filter.path.is_some());
            assert!(filter.file_ids.is_none());
        }
    }

    #[test]
    fn excluded_formats_are_pushed_with_directory_globs() {
        let storage = pushdown_storage();
        let mut plan = plan(Vec::new());
        plan.filter.globs = glob_rules(&["src/**"]);
        plan.filter.excluded_formats = vec![FileFormat::Rust];
        let filter = super::search_plan_to_storage_filter(Path::new("/missing"), &plan, &storage)
            .expect("format exclusion is passed to storage")
            .expect("filter");
        assert!(filter.file_ids.is_none());
        let predicate = filter.path.expect("combined path predicate");
        for (path, expected) in [
            ("src/main.rs", false),
            ("src/main.ts", true),
            ("docs/readme.md", false),
            ("src/.rs", true),
        ] {
            assert_eq!(
                predicate_matches(&predicate, Path::new(path)),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn format_pushdown_composes_with_glob_and_time_fallbacks() {
        let mut storage = pushdown_storage();
        let mut plan = plan(Vec::new());
        plan.filter.included_formats = vec![FileFormat::Rust];
        plan.filter.globs = glob_rules(&["src/**"]);
        let filter = super::search_plan_to_storage_filter(Path::new("/missing"), &plan, &storage)
            .expect("pushdown")
            .expect("filter");
        assert!(filter.file_ids.is_none());
        let predicate = filter.path.expect("combined path predicate");
        assert!(predicate_matches(&predicate, Path::new("src/main.rs")));
        assert!(!predicate_matches(&predicate, Path::new("src/main.RS")));
        assert!(!predicate_matches(&predicate, Path::new("tests/main.rs")));

        // Time filtering enumerates attributes while format filtering remains pushed down.
        storage.forbid_enumeration = false;
        storage.files = vec![
            file(1, "src/main.rs", 10),
            file(2, "src/other.rs", 20),
            file(3, "src/main.RS", 20),
        ];
        plan.filter.modified_after_epoch_ms = Some(20);
        let filter = super::search_plan_to_storage_filter(Path::new("/missing"), &plan, &storage)
            .expect("fallback")
            .expect("filter");
        assert_eq!(filter.file_ids, Some(vec![FileId::new(2), FileId::new(3)]));
        assert_eq!(selected_file_ids(&storage, &filter), vec![FileId::new(2)]);
    }

    #[test]
    fn formats_and_categories_use_stored_names_with_any_exclusion_taking_precedence() {
        let mut storage = pushdown_storage();
        storage.forbid_enumeration = false;
        storage.paths_only = true;
        storage.files = vec![
            file(1, "src/header.h", 10),
            file(2, "src/page.html", 20),
            file(3, "src/data.json", 30),
            file(4, "src/readme.md", 40),
            file(5, "src/source.RS", 50),
        ];
        let mut plan = plan(Vec::new());
        plan.filter.included_formats = vec![
            FileFormat::Cpp,
            FileFormat::Html,
            FileFormat::Json,
            FileFormat::Rust,
        ];
        plan.filter.categories = vec![FileCategory::Code, FileCategory::Data];
        plan.filter.excluded_categories = vec![FileCategory::Document];
        plan.filter.excluded_formats = vec![FileFormat::C];
        // Only catalog-registered spellings match, without source I/O.
        let filter = super::search_plan_to_storage_filter(
            Path::new("/nonexistent/workspace"),
            &plan,
            &storage,
        )
        .expect("stored names")
        .expect("filter");
        assert_eq!(selected_file_ids(&storage, &filter), vec![FileId::new(3)]);

        let categories = QueryFilter {
            categories: vec![FileCategory::Code],
            excluded_categories: vec![FileCategory::Document],
            ..Default::default()
        };
        let predicate = super::compile_format_filter(&categories).expect("category filter");
        assert!(predicate_matches(&predicate, Path::new("main.rs")));
        assert!(!predicate_matches(&predicate, Path::new("page.html")));
        assert!(!predicate_matches(&predicate, Path::new("readme.md")));
    }

    #[test]
    fn name_filters_preserve_special_names_and_multiple_formats_without_content_detection() {
        let root = tempfile::tempdir().expect("source root");
        std::fs::write(
            root.path().join("script"),
            "#!/usr/bin/env python3\nprint(1)\n",
        )
        .expect("extensionless Python source");
        let mut storage = pushdown_storage();
        storage.forbid_enumeration = false;
        storage.paths_only = true;
        storage.files = vec![
            file(1, "CMakeLists.txt", 0),
            file(2, "tsconfig.json", 0),
            file(3, "header.h", 0),
            file(4, "plain.txt", 0),
            file(5, "script", 0),
            file(6, "module.d.ts", 0),
        ];
        // Apart from script, these files need not exist. Format aliases, exact names,
        // longest extensions and exclusions are all evaluated against the stored name.
        for (format, expected) in [
            (FileFormat::Cmake, vec![1]),
            (FileFormat::Json, vec![2]),
            (FileFormat::TypeScript, vec![2, 6]),
            (FileFormat::C, vec![3]),
            (FileFormat::Cpp, vec![3]),
            (FileFormat::Text, vec![4]),
            (FileFormat::Unknown, vec![5]),
            (FileFormat::Python, vec![]),
        ] {
            let mut plan = plan(Vec::new());
            plan.filter.included_formats = vec![format];
            let selected = super::search_plan_to_storage_filter(root.path(), &plan, &storage)
                .expect("name filter")
                .expect("filter");
            assert_eq!(
                selected_file_ids(&storage, &selected),
                expected.into_iter().map(FileId::new).collect::<Vec<_>>(),
                "{format:?}"
            );
        }
        let mut plan = plan(Vec::new());
        plan.filter.included_formats = vec![FileFormat::Json];
        plan.filter.categories = vec![FileCategory::Code];
        let selected = super::search_plan_to_storage_filter(root.path(), &plan, &storage)
            .expect("overlapping formats")
            .expect("filter");
        assert_eq!(selected_file_ids(&storage, &selected), vec![FileId::new(2)]);
        plan.filter.excluded_formats = vec![FileFormat::TypeScript];
        let excluded = super::search_plan_to_storage_filter(root.path(), &plan, &storage)
            .expect("format exclusion")
            .expect("filter");
        assert!(selected_file_ids(&storage, &excluded).is_empty());
    }

    #[test]
    fn modification_time_filters_use_light_attributes_and_exclude_unknown_times() {
        let mut storage = pushdown_storage();
        storage.forbid_enumeration = false;
        storage.files = vec![
            file(1, "old.rs", 0),
            file(2, "new.rs", 20),
            file(3, "unknown.rs", 30),
        ];
        let snapshot = FileSnapshot {
            modified_epoch_ms: None,
            ..storage.files[2].snapshot().clone()
        };
        storage.files[2]
            .set_snapshot(snapshot)
            .expect("unknown modification time");
        let mut plan = plan(Vec::new());
        plan.filter.modified_after_epoch_ms = Some(0);
        plan.filter.modified_before_epoch_ms = Some(20);
        let filter = super::search_plan_to_storage_filter(
            Path::new("/nonexistent/workspace"),
            &plan,
            &storage,
        )
        .expect("light time projection")
        .expect("filter");
        assert_eq!(filter.file_ids, Some(vec![FileId::new(1), FileId::new(2)]));
        plan.filter.modified_after_epoch_ms = Some(20);
        let filter = super::search_plan_to_storage_filter(
            Path::new("/nonexistent/workspace"),
            &plan,
            &storage,
        )
        .expect("inclusive time range")
        .expect("filter");
        assert_eq!(filter.file_ids, Some(vec![FileId::new(2)]));
        plan.filter.modified_after_epoch_ms = None;
        plan.filter.modified_before_epoch_ms = None;
        plan.filter.categories = vec![FileCategory::Code];
        let filter = super::search_plan_to_storage_filter(
            Path::new("/nonexistent/workspace"),
            &plan,
            &storage,
        )
        .expect("unknown times need no exclusion without a time filter")
        .expect("filter");
        assert_eq!(
            selected_file_ids(&storage, &filter),
            vec![FileId::new(1), FileId::new(2), FileId::new(3)]
        );
    }

    #[test]
    fn common_globs_do_not_enumerate_file_ids() {
        let storage = pushdown_storage();
        let directory =
            StoragePathFilter::Directory(crate::domain::SourcePath::new("src").expect("directory"));
        let cases = [
            (vec!["src/**"], directory.clone()),
            (
                vec!["*.rs"],
                StoragePathFilter::FileNameSuffix(".rs".into()),
            ),
            (
                vec!["Cargo.toml"],
                StoragePathFilter::FileNameExact("Cargo.toml".into()),
            ),
            (
                vec!["test*"],
                StoragePathFilter::FileNamePrefix("test".into()),
            ),
            (
                vec!["under_score*"],
                StoragePathFilter::FileNamePrefix("under_score".into()),
            ),
            (
                vec!["*_name.rs"],
                StoragePathFilter::FileNameSuffix("_name.rs".into()),
            ),
            (
                vec!["src/**/*.rs"],
                StoragePathFilter::And(vec![
                    directory.clone(),
                    StoragePathFilter::FileNameSuffix(".rs".into()),
                ]),
            ),
            (
                vec!["src/**", "docs/**"],
                StoragePathFilter::Or(vec![
                    directory.clone(),
                    StoragePathFilter::Directory(
                        crate::domain::SourcePath::new("docs").expect("directory"),
                    ),
                ]),
            ),
            (
                vec!["src/**", "!src/generated/**"],
                StoragePathFilter::And(vec![
                    directory,
                    StoragePathFilter::Not(Box::new(StoragePathFilter::Directory(
                        crate::domain::SourcePath::new("src/generated").expect("directory"),
                    ))),
                ]),
            ),
            (
                vec!["missing/**"],
                StoragePathFilter::Directory(
                    crate::domain::SourcePath::new("missing").expect("directory"),
                ),
            ),
        ];
        for (patterns, expected) in cases {
            let mut plan = plan(Vec::new());
            plan.filter.globs = glob_rules(&patterns);
            let result =
                super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
                    .expect("planned filter")
                    .expect("filter");
            assert_eq!(result.path, Some(expected), "{patterns:?}");
            assert!(result.file_ids.is_none(), "{patterns:?}");
        }
    }

    #[test]
    fn pushed_globs_match_ripgrep_overrides_for_files_and_parent_directories() {
        let storage = pushdown_storage();
        let paths = [
            "lib.rs",
            "Cargo.toml",
            "test.txt",
            "src/main.rs",
            "src/main.ts",
            "src/deep/test.rs",
            "src/generated/a.rs",
            "src/generated/deep/a.rs",
            "docs/a.rs",
            "docs/readme.md",
            "nested/src/main.rs",
            "folder.rs/readme.md",
            "under_score.rs",
            "underXscore.rs",
            "src/under_score_test.rs",
            "src/underXscore_test.rs",
            "src/display_name.rs",
            "src/displayXname.rs",
            "src/folder_name.rs/README",
            "under_score_dir/README",
        ];
        let cases = [
            vec!["*"],
            vec!["**"],
            vec!["**/*"],
            vec!["*.rs"],
            vec!["**/*.rs"],
            vec!["Cargo.toml"],
            vec!["test*"],
            vec!["under_score*"],
            vec!["*_name.rs"],
            vec!["src/**"],
            vec!["/src/**"],
            vec!["src/**/*.rs"],
            vec!["*.rs", "*.ts"],
            vec!["src/**", "docs/**"],
            vec!["!src/generated/**"],
            vec!["*.rs", "!src/generated/**"],
            vec!["src/**", "!src/generated/**", "!docs/**"],
        ];
        for patterns in cases {
            let filter = QueryFilter {
                globs: glob_rules(&patterns),
                ..QueryFilter::default()
            };
            let matcher =
                GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("valid fixture");
            let predicate =
                compile_path_filter(&filter.globs, || storage.has_non_unicode_file_names())
                    .expect("valid fixture")
                    .expect("pushdown");
            for path in paths {
                assert_eq!(
                    predicate_matches(&predicate, Path::new(path)),
                    matcher.matches_path(Path::new(path)),
                    "patterns={patterns:?}, path={path}",
                );
            }
        }
    }

    #[test]
    fn optimized_globs_match_ripgrep_walk_across_rule_combinations() {
        use std::collections::BTreeSet;

        let root = tempfile::tempdir().expect("workspace fixture");
        let storage = pushdown_storage();
        let paths = [
            "lib.rs",
            "Cargo.toml",
            ".hidden.rs",
            "test.txt",
            "src/main.rs",
            "src/main.ts",
            "src/deep/test.rs",
            "src/generated/a.rs",
            "src/generated/deep/a.rs",
            "docs/a.rs",
            "docs/readme.md",
            "nested/src/main.rs",
            "folder.rs/readme.md",
            "docs/Cargo.toml",
            "src/folder.rs/README",
            "src/中文.rs",
        ];
        for path in paths {
            let absolute = root.path().join(path);
            std::fs::create_dir_all(absolute.parent().expect("parent")).expect("directory");
            std::fs::write(absolute, "fixture").expect("file");
        }
        let positives = [
            "*.rs",
            "*.ts",
            "Cargo.toml",
            "test*",
            "src/**",
            "docs/**",
            "src/**/*.rs",
        ];
        for first in positives {
            for second in positives {
                for exclusion in [None, Some("!src/generated/**"), Some("!docs/**")] {
                    let mut patterns = vec![first, second];
                    patterns.extend(exclusion);
                    let filter = QueryFilter {
                        globs: glob_rules(&patterns),
                        ..QueryFilter::default()
                    };
                    let matcher = GlobMatcher::new(root.path(), &filter.globs).expect("globs");
                    let predicate =
                        compile_path_filter(&filter.globs, || storage.has_non_unicode_file_names())
                            .expect("planner")
                            .expect("pushdown");
                    let mut overrides = ignore::overrides::OverrideBuilder::new(root.path());
                    for pattern in &patterns {
                        overrides.add(pattern).expect("override");
                    }
                    let mut walker = ignore::WalkBuilder::new(root.path());
                    walker
                        .standard_filters(false)
                        .overrides(overrides.build().expect("overrides"));
                    let expected = walker
                        .build()
                        .map(|entry| entry.expect("walk entry"))
                        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                        .map(|entry| {
                            entry
                                .path()
                                .strip_prefix(root.path())
                                .expect("relative path")
                                .to_path_buf()
                        })
                        .collect::<BTreeSet<_>>();
                    let selected = paths
                        .iter()
                        .filter(|path| predicate_matches(&predicate, Path::new(path)))
                        .map(PathBuf::from)
                        .collect::<BTreeSet<_>>();
                    assert_eq!(selected, expected, "{patterns:?}");
                    for path in paths {
                        assert_eq!(
                            matcher.matches_path(Path::new(path)),
                            expected.contains(Path::new(path)),
                            "{patterns:?}, {path}"
                        );
                    }
                }
            }
        }
    }

    fn predicate_matches(predicate: &StoragePathFilter, path: &Path) -> bool {
        let name = path
            .file_name()
            .expect("valid fixture")
            .to_str()
            .expect("valid fixture");
        match predicate {
            StoragePathFilter::All => true,
            StoragePathFilter::None => false,
            StoragePathFilter::Directory(directory) => path
                .parent()
                .is_some_and(|parent| parent.starts_with(directory.as_path())),
            StoragePathFilter::FileNameExact(value) => name == value,
            StoragePathFilter::FileNamePrefix(value) => name.starts_with(value),
            StoragePathFilter::FileNameSuffix(value) => name.ends_with(value),
            StoragePathFilter::And(predicates) => predicates
                .iter()
                .all(|predicate| predicate_matches(predicate, path)),
            StoragePathFilter::Or(predicates) => predicates
                .iter()
                .any(|predicate| predicate_matches(predicate, path)),
            StoragePathFilter::Not(predicate) => !predicate_matches(predicate, path),
        }
    }

    #[test]
    fn complex_globs_fall_back_with_ordered_reinclusion_and_directory_pruning() {
        let files = [
            file(1, "src/main.rs", 1),
            file(2, "src/generated/keep.rs", 1),
            file(3, "src/generated/nested/keep.rs", 1),
            file(4, "folder.rs/readme.md", 1),
            file(5, "notes/keep.md", 1),
        ];
        let cases = [
            (
                vec!["src/**/*.rs", "!src/generated/**", "src/generated/keep.rs"],
                vec![1, 2],
            ),
            (vec!["!src/generated", "src/generated/keep.rs"], vec![]),
            (
                vec!["!src/generated/**", "src/generated/nested/keep.rs"],
                vec![],
            ),
            (vec!["!*.rs"], vec![5]),
            (vec!["src/**/k?ep.[r]s"], vec![2, 3]),
        ];
        for (patterns, expected) in cases {
            let mut plan = plan(Vec::new());
            plan.filter.globs = glob_rules(&patterns);
            assert!(
                compile_path_filter(&plan.filter.globs, || pushdown_storage()
                    .has_non_unicode_file_names())
                .expect("valid fixture")
                .is_none(),
                "{patterns:?}"
            );
            let actual = super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &files)
                .expect("valid fixture");
            assert_eq!(
                actual.iter().map(|id| id.get()).collect::<Vec<_>>(),
                expected,
                "{patterns:?}"
            );
        }
    }

    #[test]
    fn fallback_preserves_glob_escaping_case_order_and_invalid_pattern_errors() {
        let mut filter = QueryFilter {
            globs: glob_rules(&[r"\!literal.rs"]),
            ..QueryFilter::default()
        };
        let matcher =
            GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("valid fixture");
        assert!(matcher.matches_path(Path::new("nested/!literal.rs")));
        assert!(!matcher.matches_path(Path::new("literal.rs")));

        filter.globs = glob_rules(&["*.RS"]);
        let sensitive =
            GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("valid fixture");
        assert!(!sensitive.matches_path(Path::new("src/main.rs")));
        filter.globs[0].case_insensitive = true;
        let insensitive =
            GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("valid fixture");
        assert!(insensitive.matches_path(Path::new("src/main.rs")));
        assert!(
            compile_path_filter(&filter.globs, || pushdown_storage()
                .has_non_unicode_file_names())
            .expect("planner")
            .is_none()
        );

        filter.globs.extend(glob_rules(&["!main.rs"]));
        let excluded =
            GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("mixed rules");
        assert!(!excluded.matches_path(Path::new("src/main.rs")));
        filter.globs.reverse();
        let included =
            GlobMatcher::new(Path::new("/workspace"), &filter.globs).expect("reversed rules");
        assert!(included.matches_path(Path::new("src/main.rs")));

        filter.globs = glob_rules(&["["]);
        assert!(GlobMatcher::new(Path::new("/workspace"), &filter.globs).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_matches_non_unicode_paths_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let source = file(1, "placeholder.rs", 1);
        let source = FileRecord::new(
            source.id(),
            crate::domain::SourcePath::new(OsString::from_vec(b"src/\xff.rs".to_vec()))
                .expect("source path"),
            source.snapshot().clone(),
            source.index_status().clone(),
        )
        .expect("file");
        let mut storage = pushdown_storage();
        storage.files = vec![source.clone()];
        let name_patterns = glob_rules(&["*.rs"]);
        assert!(
            compile_path_filter(&name_patterns, || storage.has_non_unicode_file_names())
                .expect("planner")
                .is_none()
        );
        let directory_patterns = glob_rules(&["src/**"]);
        assert_eq!(
            compile_path_filter(&directory_patterns, || storage.has_non_unicode_file_names())
                .expect("planner"),
            Some(StoragePathFilter::Directory(
                crate::domain::SourcePath::new("src").expect("directory")
            ))
        );
        let mut plan = plan(Vec::new());
        plan.filter.globs = glob_rules(&["*.rs"]);
        assert_eq!(
            super::resolve_filtered_file_ids(
                Path::new("/workspace"),
                &plan,
                std::slice::from_ref(&source),
            )
            .expect("valid fixture"),
            vec![source.id()]
        );
        plan.filter.globs = glob_rules(&["*\u{fffd}.rs"]);
        assert!(
            super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &[source])
                .expect("valid fixture")
                .is_empty()
        );
    }

    fn glob_rules(patterns: &[&str]) -> Vec<GlobRule> {
        patterns
            .iter()
            .map(|pattern| GlobRule {
                pattern: (*pattern).to_owned(),
                case_insensitive: false,
            })
            .collect()
    }

    fn plan(routes: Vec<SearchRoute>) -> SearchPlan {
        SearchPlan {
            routes,
            limit: Some(10),
            trace: true,
            prefer_symbol: false,
            filter: QueryFilter::default(),
        }
    }

    fn file(id: u32, relative: &str, modified: u64) -> FileRecord {
        FileRecord::new(
            FileId::new(id),
            crate::domain::SourcePath::new(relative).expect("source path"),
            FileSnapshot {
                size_bytes: 100,
                modified_epoch_ms: Some(modified),
                content_hash: Some(crate::utils::sha256_hex(&id.to_le_bytes())),
            },
            FileIndexStatus::Indexed {
                indexed_epoch_ms: modified,
                entity_count: 1,
            },
        )
        .expect("file")
    }

    fn entity(file: &FileRecord, content: &str) -> StoredEntity {
        let source_range = Range::Text(
            TextRange::from_coordinates(0, content.len(), 1, 1, 0, content.len())
                .expect("source range"),
        );
        let content = Content::Text(content.to_owned());
        let id = EntityId::new(file.id(), &content, source_range).expect("entity id");
        let fragment = EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        };
        StoredEntity {
            entity: Entity::new(id, file.id(), source_range, content, None, vec![fragment])
                .expect("entity"),
            file: file.clone(),
        }
    }

    fn hit(stored: &StoredEntity, ordinal: u32, path: StorageSearchPath, score: f64) -> FixtureHit {
        let fragment_id = FragmentId::new(stored.entity.id(), ordinal);
        FixtureHit {
            hit: StorageSearchHit {
                document_id: fragment_id.as_str().to_owned(),
                entity_id: stored.entity.id().clone(),
                file_id: stored.file.id(),
                path,
                score,
            },
            fragment: EntityFragment {
                id: fragment_id,
                range: Range::Full,
            },
        }
    }
}
