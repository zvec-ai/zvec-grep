use super::storage::SearchStorage;

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Instant,
};

use async_trait::async_trait;

use crate::{
    EngineError,
    api::context::{
        options::{ContextRoute as SearchRoute, ContextRouteMode as SearchRouteMode, QueryFilter},
        result::{
            MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace, SearchRecallTrace,
            TimingEntry,
        },
    },
    domain::{
        Content, Entity, EntityFragment, EntityId, FileId, FileRecord,
        model::{EmbeddingModelInfo, EmbeddingPurpose},
    },
    file_selection::GlobMatcher,
    models::{EmbeddingOptions, ModelError, ModelRuntimeLease},
    storage::types::{StoragePathFilter, StorageSearchFilter, StorageSearchHit, StoredSearchData},
};

use super::{
    format_filter::{compile_format_filter, matches_file_name},
    path_filter::{all, compile_path_filter},
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

    let recall_started = Instant::now();
    let mut candidates = HashMap::new();
    if has_searchable_files && limit > 0 {
        collect_adaptive_recall(
            &routes,
            filter.as_ref(),
            plan.prefer_symbol,
            &vectors,
            limit,
            storage,
            &mut candidates,
        )?;
    }
    let recall_duration = recall_started.elapsed();

    let fusion_started = Instant::now();
    let fused = fuse_candidates(candidates);
    let fusion_duration = fusion_started.elapsed();

    let load_started = Instant::now();
    // Every recalled entity must be eligible for structural boosts before cutoff.
    let hits = load_candidates(fused, limit, plan.trace, storage)?;
    let load_duration = load_started.elapsed();
    let ranking_started = Instant::now();
    let maximum_rrf = build_recall_routes(&routes, filter.as_ref(), plan.prefer_symbol)
        .iter()
        .map(|_| 1.0 / (RRF_K + 1.0))
        .sum();
    let hits = super::ranking::rank(hits, &routes, &plan.filter, maximum_rrf, limit);
    let ranking_duration = ranking_started.elapsed();

    Ok(SearchPlanResult {
        routes,
        hits,
        timings: vec![
            timing("search_plan", plan_duration),
            timing("search_filter", filter_duration),
            timing("query_embedding", embedding_duration),
            timing("recall", recall_duration),
            timing("fusion", fusion_duration),
            timing("load_results", load_duration),
            timing("ranking", ranking_duration),
            timing("search_total", total_started.elapsed()),
        ],
    })
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
    model.info().validate()?;
    let vector_routes = routes
        .iter()
        .filter(|route| route.mode == SearchRouteMode::Vector)
        .collect::<Vec<_>>();
    let maximum = model.info().max_batch_size;
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
                    model: model.info().model.reference(),
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
) -> Result<(), EngineError> {
    let recall_routes = build_recall_routes(routes, filter, prefer_symbol);
    let target = (limit * RECALL_TARGET_FACTOR).max(RECALL_MIN_TARGET_CANDIDATES);
    let mut previous_depth = 0;
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
            add_recall_hits(candidates, &hits, &route.route, previous_depth)?;
        }
        if candidates.len() >= target || !saturated || depth >= RECALL_MAX_DEPTH {
            return Ok(());
        }
        previous_depth = depth;
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
    start_index: usize,
) -> Result<(), EngineError> {
    for (index, hit) in hits.iter().enumerate().skip(start_index) {
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
        candidate.evidence.push(InternalEvidence {
            hit: hit.clone(),
            path: route.mode,
            route_id: route.id.clone(),
            rank,
        });
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
    trace: bool,
    storage: &dyn SearchStorage,
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
        if let Some(hit) = candidate_to_hit(candidate, limit, trace, &mut data)? {
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
) -> Result<Option<SearchHit>, EngineError> {
    let Some(stored) = data.entities.remove(&candidate.id) else {
        // A file replacement may remove owners after recall has completed.
        return Ok(None);
    };
    if stored.entity.id != candidate.id
        || stored.entity.file_id != candidate.file_id
        || stored.file.id != candidate.file_id
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
                .fragments
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
        return Ok(None);
    }
    Ok(Some(SearchHit {
        entity: stored.entity,
        file: stored.file,
        evidence: loaded_evidence,
        rank: candidate.rank,
        score: candidate.score,
        matched_by,
        trace: trace.then_some(SearchHitTrace {
            recall: candidate.recall,
            ranking: None,
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
    // Compile even when pushdown is available so invalid rules have one error path.
    let matcher = GlobMatcher::new(workspace_root, &plan.filter.globs)?;
    let has_globs = !plan.filter.globs.is_empty();
    let glob_path = if has_globs {
        compile_path_filter(&plan.filter.globs, storage)?
    } else {
        None
    };
    let format_path = compile_format_filter(&plan.filter);
    let push_formats = format_path.is_some() && !storage.has_non_unicode_file_names()?;
    let residual_format = (!push_formats).then_some(format_path.as_ref()).flatten();
    let needs_attributes = plan.filter.modified_after_epoch_ms.is_some()
        || plan.filter.modified_before_epoch_ms.is_some();
    let file_ids = if needs_attributes {
        let attributes = storage.list_file_attributes()?;
        Some(resolve_filtered_paths(
            plan,
            &matcher,
            residual_format,
            attributes.iter().map(|file| {
                (
                    file.id,
                    file.relative_path.as_path(),
                    file.modified_epoch_ms,
                )
            }),
        ))
    } else if residual_format.is_some() || (has_globs && glob_path.is_none()) {
        let paths = storage.list_file_paths()?;
        Some(resolve_filtered_paths(
            plan,
            &matcher,
            residual_format,
            paths.iter().map(|(id, path)| (*id, path.as_path(), None)),
        ))
    } else {
        None
    };
    let path = match (glob_path, format_path.filter(|_| push_formats)) {
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

fn resolve_filtered_paths<'a>(
    plan: &SearchPlan,
    matcher: &GlobMatcher,
    format: Option<&StoragePathFilter>,
    files: impl IntoIterator<Item = (FileId, &'a Path, Option<u64>)>,
) -> Vec<FileId> {
    files
        .into_iter()
        .filter(|(_, relative, modified)| {
            matcher.matches_path(relative)
                && plan
                    .filter
                    .modified_after_epoch_ms
                    .is_none_or(|after| modified.is_some_and(|modified| modified >= after))
                && plan
                    .filter
                    .modified_before_epoch_ms
                    .is_none_or(|before| modified.is_some_and(|modified| modified <= before))
                && format.is_none_or(|predicate| {
                    relative
                        .file_name()
                        .is_some_and(|name| matches_file_name(predicate, name))
                })
        })
        .map(|(id, _, _)| id)
        .collect()
}

#[cfg(test)]
fn resolve_filtered_file_ids(
    workspace_root: &Path,
    plan: &SearchPlan,
    files: &[FileRecord],
) -> Result<Vec<FileId>, EngineError> {
    let matcher = GlobMatcher::new(workspace_root, &plan.filter.globs)?;
    let format = compile_format_filter(&plan.filter);
    Ok(resolve_filtered_paths(
        plan,
        &matcher,
        format.as_ref(),
        files.iter().map(|file| {
            (
                file.id,
                file.relative_path.as_path(),
                file.snapshot.modified_epoch_ms,
            )
        }),
    ))
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
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;

    use crate::{
        EngineResult,
        domain::{
            ByteRange, Content, Entity, EntityFragment, EntityId, FileCategory, FileFormat, FileId,
            FileIndexStatus, FileRecord, FileSnapshot, FragmentId, GlobRule, Range, TextRange,
            model::{EmbeddingModelInfo, Metric},
        },
        models::ModelError,
        storage::types::{
            StoragePathFilter, StorageSearchFilter, StorageSearchHit, StorageSearchPath,
            StoredEntity, StoredFileAttributes, StoredSearchData,
        },
    };

    use super::{
        GlobMatcher, MatchedBy, QueryFilter, SearchEmbeddingRuntime, SearchPlan, SearchRoute,
        SearchRouteMode, SearchStorage, compile_path_filter, search_workspace_index,
    };

    struct FixtureModel {
        info: EmbeddingModelInfo,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FixtureModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".to_owned(),
                        name: "fixture".to_owned(),
                        endpoint: None,
                    },
                    dimension: 2,
                    metric: Metric::Cosine,
                    max_batch_size: 8,
                    max_input_tokens: None,
                    max_image_bytes: None,
                },
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
                                .find(|file| file.id == hit.hit.file_id)
                                .is_some_and(|file| {
                                    predicate_matches(predicate, file.relative_path.as_path())
                                })
                        })
                })
                .take(limit)
                .map(|hit| hit.hit.clone())
                .collect()
        }
    }

    impl SearchStorage for FixtureStorage {
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
                .map(|file| (file.id, file.relative_path.to_path_buf()))
                .collect())
        }

        fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
            Ok(self.files.iter().any(|file| {
                file.relative_path
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
                    stored.entity.fragments = self
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
            Ok(self.hits_with_filter(&self.vector, limit, filter))
        }
    }

    #[tokio::test]
    async fn promotes_symbol_definition_before_result_cutoff() {
        let reference_file = file(1, "src/plugin.ts", 100);
        let definition_file = file(2, "src/core.ts", 100);
        let mut reference = entity(&reference_file, "vitest: Vitest");
        let mut definition = entity(&definition_file, "class Vitest {}");
        let definition_id = definition.entity.id.clone();
        for (stored, kind, name) in [
            (&mut reference, crate::domain::SymbolType::Value, "vitest"),
            (&mut definition, crate::domain::SymbolType::Class, "Vitest"),
        ] {
            stored.entity.metadata = Some(crate::domain::EntityMetadata::Code(
                crate::domain::CodeMetadata {
                    symbol_type: Some(kind),
                    symbol_name: Some(name.to_owned()),
                    scope: None,
                    signature: None,
                    documentation: None,
                },
            ));
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
            .insert(reference.entity.id.as_str().to_owned(), reference);
        storage
            .entities
            .insert(definition.entity.id.as_str().to_owned(), definition);
        let mut request = plan(vec![SearchRoute {
            mode: SearchRouteMode::Fts,
            query: "Vitest".to_owned(),
        }]);
        request.limit = Some(1);
        let root = tempfile::tempdir().expect("workspace");
        let result = search_workspace_index(root.path(), request, &storage, &[])
            .await
            .expect("search");
        assert_eq!(result.hits[0].entity.id, definition_id);
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
                (entity_a.entity.id.as_str().to_owned(), entity_a.clone()),
                (entity_b.entity.id.as_str().to_owned(), entity_b.clone()),
            ]),
            fts: HashMap::from([("alpha".to_owned(), vec![a_fts, b_fts])]),
            vector: vec![b_vector, a_vector],
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
        )
        .await
        .expect("hybrid search");

        assert_eq!(result.hits.len(), 2);
        let mut expected_ids = [entity_a.entity.id, entity_b.entity.id];
        expected_ids.sort();
        assert_eq!(result.hits[0].entity.id, expected_ids[0]);
        assert_eq!(result.hits[1].entity.id, expected_ids[1]);
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
    async fn loads_all_candidates_once_before_rule_ranking() {
        let source = file(1, "src/lib.rs", 100);
        let selected = entity(&source, "selected source");
        let discarded = entity(&source, "discarded source");
        let discarded_id = discarded.entity.id.clone();
        let mut fts = (0..205)
            .map(|index| hit(&selected, index, StorageSearchPath::Fts, 1.0))
            .collect::<Vec<_>>();
        fts.push(hit(&discarded, 0, StorageSearchPath::Fts, 0.5));
        let mut storage = pushdown_storage();
        storage.files.push(source.clone());
        storage
            .entities
            .insert(selected.entity.id.as_str().to_owned(), selected.clone());
        storage
            .entities
            .insert(discarded.entity.id.as_str().to_owned(), discarded);
        storage.fts.insert("query".to_owned(), fts);
        storage.vector = vec![hit(&selected, 0, StorageSearchPath::Vector, 0.9)];
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
        )
        .await
        .expect("search");

        assert_eq!(result.hits.len(), 1);
        let selected_hit = &result.hits[0];
        assert_eq!(selected_hit.entity.id, selected.entity.id);
        assert_eq!(selected_hit.evidence.len(), 206);
        assert_eq!(
            selected_hit.evidence[0].fragment.id,
            FragmentId::new(&selected.entity.id, 0)
        );
        assert_eq!(
            selected_hit.evidence[1].fragment.id,
            FragmentId::new(&selected.entity.id, 0)
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
                .filter(|hit| hit.entity_id == selected.entity.id)
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
                    .insert(stored.entity.id.as_str().to_owned(), stored.clone());
                recalled.fragment.range =
                    Range::Byte(ByteRange::new(1, 3).expect("ordered byte offsets"));
            }
            storage.fts.insert("query".to_owned(), vec![recalled]);
            let plan = plan(vec![SearchRoute {
                mode: SearchRouteMode::Fts,
                query: "query".to_owned(),
            }]);
            let result = search_workspace_index(Path::new("/workspace"), plan, &storage, &[]).await;
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
        let mut stored = entity(&source, "source");
        let retained = hit(&stored, 0, StorageSearchPath::Fts, 2.0);
        let removed = hit(&stored, 1, StorageSearchPath::Fts, 1.0);
        stored.entity.fragments = vec![retained.fragment.clone()];
        for keep_fragment in [false, true] {
            let mut data = StoredSearchData::default();
            data.entities
                .insert(stored.entity.id.clone(), stored.clone());
            if keep_fragment {
                data.fragments
                    .insert(retained.hit.document_id.clone(), retained.fragment.clone());
            }
            let candidate = super::Candidate {
                id: stored.entity.id.clone(),
                file_id: source.id,
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
                assert!(result.is_none());
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
            entities: HashMap::from([(owner.entity.id.as_str().to_owned(), owner.clone())]),
            fts: HashMap::from([(
                "Service".to_owned(),
                vec![hit(&owner, 0, StorageSearchPath::Fts, 2.0)],
            )]),
            vector: Vec::new(),
            filters: Arc::new(Mutex::new(Vec::new())),
            load_batches: Mutex::default(),
        };
        let first = FixtureModel::new();
        let mut second = FixtureModel::new();
        second.info.model.name = "second".into();
        let mut query = plan(vec![SearchRoute {
            mode: SearchRouteMode::Vector,
            query: "Service".to_owned(),
        }]);
        query.prefer_symbol = true;
        let error =
            search_workspace_index(Path::new("/workspace"), query, &storage, &[&first, &second])
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
                    source_entity.entity.id.as_str().to_owned(),
                    source_entity.clone(),
                ),
                (
                    docs_entity.entity.id.as_str().to_owned(),
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
            filters: Arc::clone(&filters),
            load_batches: Mutex::default(),
        };
        let mut plan = plan(vec![SearchRoute {
            mode: SearchRouteMode::Fts,
            query: "Service".to_owned(),
        }]);
        plan.filter.globs = glob_rules(&["src/**"]);
        plan.filter.formats = vec![FileFormat::Rust];
        plan.prefer_symbol = true;

        let result = search_workspace_index(Path::new("/workspace"), plan, &storage, &[])
            .await
            .expect("filtered search");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].file.id, source.id);
        let filters = filters.lock().expect("captured filters");
        assert!(
            filters
                .iter()
                .flatten()
                .all(|filter| selected_file_ids(&storage, filter) == [source.id])
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
                [source.id]
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
                    .is_none_or(|ids| ids.contains(&file.id))
                    && filter.path.as_ref().is_none_or(|predicate| {
                        predicate_matches(predicate, file.relative_path.as_path())
                    })
            })
            .map(|file| file.id)
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
            filters: Arc::default(),
            load_batches: Mutex::default(),
        }
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
        assert_eq!(filter.file_ids, Some(vec![storage.files[0].id]));
    }

    #[test]
    fn format_filters_push_down_without_enumerating_files() {
        let storage = pushdown_storage();
        for query in [
            QueryFilter {
                formats: vec![FileFormat::Rust],
                ..Default::default()
            },
            QueryFilter {
                formats: vec![FileFormat::Jpeg],
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
                formats: vec![FileFormat::Unknown],
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
        plan.filter.formats = vec![FileFormat::Rust];
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
        plan.filter.formats = vec![
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
            plan.filter.formats = vec![format];
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
        plan.filter.formats = vec![FileFormat::Json];
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
        storage.files[2].snapshot.modified_epoch_ms = None;
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
            let predicate = compile_path_filter(&filter.globs, &storage)
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
                    let predicate = compile_path_filter(&filter.globs, &storage)
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
                compile_path_filter(&plan.filter.globs, &pushdown_storage())
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
            compile_path_filter(&filter.globs, &pushdown_storage())
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

        let mut source = file(1, "placeholder.rs", 1);
        source.relative_path =
            crate::domain::SourcePath::new(OsString::from_vec(b"src/\xff.rs".to_vec()))
                .expect("source path");
        let mut storage = pushdown_storage();
        storage.files = vec![source.clone()];
        let name_patterns = glob_rules(&["*.rs"]);
        assert!(
            compile_path_filter(&name_patterns, &storage)
                .expect("planner")
                .is_none()
        );
        let directory_patterns = glob_rules(&["src/**"]);
        assert_eq!(
            compile_path_filter(&directory_patterns, &storage).expect("planner"),
            Some(StoragePathFilter::Directory(
                crate::domain::SourcePath::new("src").expect("directory")
            ))
        );
        let mut plan = plan(Vec::new());
        plan.filter.globs = glob_rules(&["*.rs"]);
        assert_eq!(
            super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &[source.clone()])
                .expect("valid fixture"),
            vec![source.id]
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
        FileRecord {
            id: FileId::new(id),
            relative_path: crate::domain::SourcePath::new(relative).expect("source path"),
            snapshot: FileSnapshot {
                size_bytes: 100,
                modified_epoch_ms: Some(modified),
                content_hash: Some(crate::utils::sha256_hex(&id.to_le_bytes())),
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: modified,
                entity_count: 1,
            },
        }
    }

    fn entity(file: &FileRecord, content: &str) -> StoredEntity {
        let source_range = Range::Text(
            TextRange::from_coordinates(0, content.len(), 1, 1, 0, content.len())
                .expect("source range"),
        );
        let content = Content::Text(content.to_owned());
        StoredEntity {
            entity: Entity {
                id: EntityId::new(file.id, &content, source_range).expect("entity id"),
                file_id: file.id,
                source_range,
                content,
                metadata: None,
                fragments: Vec::new(),
            },
            file: file.clone(),
        }
    }

    fn hit(stored: &StoredEntity, ordinal: u32, path: StorageSearchPath, score: f64) -> FixtureHit {
        let fragment_id = FragmentId::new(&stored.entity.id, ordinal);
        FixtureHit {
            hit: StorageSearchHit {
                document_id: fragment_id.as_str().to_owned(),
                entity_id: stored.entity.id.clone(),
                file_id: stored.file.id,
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
