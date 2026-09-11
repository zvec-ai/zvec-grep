use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Instant,
};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use ignore::types::{Types, TypesBuilder};

use crate::{
    EngineError,
    api::context::{
        options::{ContextRoute, ContextRouteMode, SymbolType},
        result::{
            MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace, SearchRecallTrace,
            TimingEntry,
        },
    },
    domain::{Entity, EntityFragment, FileId},
    models::{
        EmbeddingInput, EmbeddingModelInfo, EmbeddingOptions, EmbeddingPurpose, ModelError,
        ModelRuntimeLease,
    },
    storage::spi::{
        StorageSearchFilter, StorageSearchHit, StoredEntity, StoredFile, WorkspaceIndexStorage,
    },
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
    pub routes: Vec<ContextRoute>,
    pub limit: Option<usize>,
    pub trace: bool,
    pub prefer_symbol: bool,
    pub symbol_types: Vec<SymbolType>,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub globs: Vec<String>,
    pub insensitive_globs: Vec<String>,
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
    pub modified_after_epoch_ms: Option<u64>,
    pub modified_before_epoch_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSearchRoute {
    pub id: String,
    pub mode: ContextRouteMode,
    pub query: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchEvidence {
    pub fragment: EntityFragment,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchHit {
    pub entity: Entity,
    pub file: StoredFile,
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
                .map(EmbeddingInput::text)
                .collect::<Vec<_>>(),
            EmbeddingOptions {
                purpose: Some(EmbeddingPurpose::Query),
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
    id: String,
    entity: Entity,
    file: StoredFile,
    sources: HashSet<ContextRouteMode>,
    recall: Vec<SearchRecallTrace>,
    evidence: Vec<InternalEvidence>,
    score: f64,
    rank: usize,
}

struct InternalEvidence {
    fragment: EntityFragment,
    path: ContextRouteMode,
    route_id: String,
    rank: usize,
}

pub(crate) async fn search_workspace_index(
    plan: SearchPlan,
    storage: &dyn WorkspaceIndexStorage,
    embedding_model: Option<&dyn SearchEmbeddingRuntime>,
) -> Result<SearchPlanResult, EngineError> {
    let total_started = Instant::now();
    let plan_started = Instant::now();
    let routes = resolve_routes(&plan.routes)?;
    validate_modified_range(&plan)?;
    let limit = plan.limit.unwrap_or(DEFAULT_LIMIT);
    let plan_duration = plan_started.elapsed();

    let filter_started = Instant::now();
    let filter = search_plan_to_storage_filter(&plan, storage)?;
    let filter_duration = filter_started.elapsed();
    let has_searchable_files = !filter_matches_no_files(filter.as_ref());

    let embedding_started = Instant::now();
    let vectors = if has_searchable_files
        && routes
            .iter()
            .any(|route| route.mode == ContextRouteMode::Vector)
    {
        embed_vector_routes(&routes, require_embedding_model(embedding_model)?).await?
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
    let hits = fused
        .into_iter()
        .take(limit)
        .map(|candidate| candidate_to_hit(candidate, limit, plan.trace))
        .collect();
    let fusion_duration = fusion_started.elapsed();

    Ok(SearchPlanResult {
        routes,
        hits,
        timings: vec![
            timing("search_plan", plan_duration),
            timing("search_filter", filter_duration),
            timing("query_embedding", embedding_duration),
            timing("recall", recall_duration),
            timing("fusion", fusion_duration),
            timing("search_total", total_started.elapsed()),
        ],
    })
}

fn resolve_routes(routes: &[ContextRoute]) -> Result<Vec<ResolvedSearchRoute>, EngineError> {
    if routes.is_empty() {
        return Err(EngineError::invalid_argument(
            "search plan requires at least one route",
        ));
    }
    let mut counts = HashMap::<ContextRouteMode, usize>::new();
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
                ContextRouteMode::Fts => "fts",
                ContextRouteMode::Vector => "vector",
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
    if plan.modified_after_epoch_ms.is_some_and(|after| {
        plan.modified_before_epoch_ms
            .is_some_and(|before| after > before)
    }) {
        Err(EngineError::invalid_argument(
            "modified-after must not be later than modified-before",
        ))
    } else {
        Ok(())
    }
}

fn require_embedding_model(
    model: Option<&dyn SearchEmbeddingRuntime>,
) -> Result<&dyn SearchEmbeddingRuntime, EngineError> {
    model.ok_or_else(|| {
        EngineError::unsupported("vector search requires a configured embedding model")
    })
}

async fn embed_vector_routes(
    routes: &[ResolvedSearchRoute],
    model: &dyn SearchEmbeddingRuntime,
) -> Result<HashMap<String, Vec<f32>>, EngineError> {
    let vector_routes = routes
        .iter()
        .filter(|route| route.mode == ContextRouteMode::Vector)
        .collect::<Vec<_>>();
    let maximum = model.info().limits.max_batch_size;
    if maximum == 0 {
        return Err(EngineError::internal(
            "embedding model has a zero query batch limit",
        ));
    }
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
            vectors.insert(route.id.clone(), vector);
        }
    }
    Ok(vectors)
}

#[allow(clippy::too_many_arguments)]
fn collect_adaptive_recall(
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    prefer_symbol: bool,
    vectors: &HashMap<String, Vec<f32>>,
    limit: usize,
    storage: &dyn WorkspaceIndexStorage,
    candidates: &mut HashMap<String, Candidate>,
) -> Result<(), EngineError> {
    let recall_routes = build_recall_routes(routes, filter, prefer_symbol);
    let target = (limit * RECALL_TARGET_FACTOR).max(RECALL_MIN_TARGET_CANDIDATES);
    let mut previous_depth = 0;
    let mut depth = RECALL_INITIAL_DEPTH;
    loop {
        let mut saturated = false;
        for route in &recall_routes {
            let hits = match route.route.mode {
                ContextRouteMode::Fts => {
                    storage.search_fts(&route.route.query, depth, route.filter.as_ref())?
                }
                ContextRouteMode::Vector => vectors
                    .get(route.vector_route_id.as_deref().unwrap_or(&route.route.id))
                    .map_or_else(
                        || Ok(Vec::new()),
                        |vector| storage.search_vector(vector, depth, route.filter.as_ref()),
                    )?,
            };
            saturated |= hits.len() >= depth;
            add_recall_hits(candidates, &hits, &route.route, storage, previous_depth)?;
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
            vector_route_id: (route.mode == ContextRouteMode::Vector).then(|| route.id.clone()),
            route,
            filter: filter.cloned(),
        })
        .collect::<Vec<_>>();
    if prefer_symbol {
        for route in routes {
            let symbol_names = extract_symbol_names(&route.query);
            if symbol_names.is_empty() {
                continue;
            }
            let mut symbol_filter = filter.cloned().unwrap_or_default();
            symbol_filter.symbol_names = Some(symbol_names);
            output.push(RecallRoute {
                route: ResolvedSearchRoute {
                    id: format!("{}.prefer-symbol", route.id),
                    mode: ContextRouteMode::Fts,
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
    candidates: &mut HashMap<String, Candidate>,
    hits: &[StorageSearchHit],
    route: &ResolvedSearchRoute,
    storage: &dyn WorkspaceIndexStorage,
    start_index: usize,
) -> Result<(), EngineError> {
    for (index, hit) in hits.iter().enumerate().skip(start_index) {
        let entity_id = public_entity_id(&hit.fragment).to_owned();
        let resolved = if let Some(candidate) = candidates.get(&entity_id) {
            Some(StoredEntity {
                entity: candidate.entity.clone(),
                file: candidate.file.clone(),
            })
        } else {
            resolve_hit_entity(hit, storage)?
        };
        let Some(resolved) = resolved else {
            continue;
        };
        let rank = index + 1;
        let candidate = candidates
            .entry(entity_id.clone())
            .or_insert_with(|| Candidate {
                id: entity_id,
                entity: resolved.entity,
                file: resolved.file,
                sources: HashSet::new(),
                recall: Vec::new(),
                evidence: Vec::new(),
                score: 0.0,
                rank: usize::MAX,
            });
        candidate.sources.insert(route.mode);
        candidate.evidence.push(InternalEvidence {
            fragment: hit.fragment.clone(),
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

fn resolve_hit_entity(
    hit: &StorageSearchHit,
    storage: &dyn WorkspaceIndexStorage,
) -> Result<Option<StoredEntity>, EngineError> {
    match &hit.fragment {
        EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
            Ok(Some(StoredEntity {
                entity: entity.clone(),
                file: hit.file.clone(),
            }))
        }
        EntityFragment::Window(window) => storage.get_entity(&window.entity_id),
    }
}

fn public_entity_id(fragment: &EntityFragment) -> &str {
    fragment.entity_id().as_str()
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

fn fuse_candidates(candidates: HashMap<String, Candidate>) -> Vec<Candidate> {
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
            .then_with(|| left.id.cmp(&right.id))
    });
    for (index, candidate) in fused.iter_mut().enumerate() {
        candidate.rank = index + 1;
    }
    fused
}

fn candidate_to_hit(candidate: Candidate, limit: usize, trace: bool) -> SearchHit {
    let matched_by = derive_matched_by(&candidate.sources);
    let mut evidence = candidate.evidence;
    evidence.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| route_mode_order(left.path).cmp(&route_mode_order(right.path)))
            .then_with(|| left.route_id.cmp(&right.route_id))
            .then_with(|| {
                left.fragment
                    .document_id()
                    .cmp(right.fragment.document_id())
            })
    });
    SearchHit {
        entity: candidate.entity,
        file: candidate.file,
        evidence: evidence
            .into_iter()
            .map(|evidence| SearchEvidence {
                fragment: evidence.fragment,
            })
            .collect(),
        rank: candidate.rank,
        score: candidate.score,
        matched_by,
        trace: trace.then_some(SearchHitTrace {
            recall: candidate.recall,
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
    }
}

fn derive_matched_by(sources: &HashSet<ContextRouteMode>) -> MatchedBy {
    match (
        sources.contains(&ContextRouteMode::Fts),
        sources.contains(&ContextRouteMode::Vector),
    ) {
        (true, true) => MatchedBy::FtsAndVector,
        (false, true) => MatchedBy::Vector,
        (true | false, false) => MatchedBy::Fts,
    }
}

fn route_mode_order(mode: ContextRouteMode) -> u8 {
    match mode {
        ContextRouteMode::Fts => 0,
        ContextRouteMode::Vector => 1,
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
    plan: &SearchPlan,
    storage: &dyn WorkspaceIndexStorage,
) -> Result<Option<StorageSearchFilter>, EngineError> {
    let needs_file_filter = !plan.include_paths.is_empty()
        || !plan.exclude_paths.is_empty()
        || !plan.globs.is_empty()
        || !plan.insensitive_globs.is_empty()
        || !plan.file_types.is_empty()
        || !plan.excluded_file_types.is_empty()
        || plan.modified_after_epoch_ms.is_some()
        || plan.modified_before_epoch_ms.is_some();
    let file_ids = needs_file_filter
        .then(|| resolve_filtered_file_ids(plan, &storage.list_files()?))
        .transpose()?;
    let symbol_types = (!plan.symbol_types.is_empty())
        .then(|| plan.symbol_types.iter().copied().map(Into::into).collect());
    if file_ids.is_none() && symbol_types.is_none() {
        Ok(None)
    } else {
        Ok(Some(StorageSearchFilter {
            file_ids,
            symbol_types,
            ..StorageSearchFilter::default()
        }))
    }
}

fn resolve_filtered_file_ids(
    plan: &SearchPlan,
    files: &[StoredFile],
) -> Result<Vec<FileId>, EngineError> {
    let include = plan
        .include_paths
        .iter()
        .map(|pattern| PathMatcher::path(pattern))
        .collect::<Result<Vec<_>, _>>()?;
    let exclude = plan
        .exclude_paths
        .iter()
        .map(|pattern| PathMatcher::path(pattern))
        .collect::<Result<Vec<_>, _>>()?;
    let ordered_globs = ordered_globs(&plan.globs, &plan.insensitive_globs)?;
    let types = build_file_types(&plan.file_types, &plan.excluded_file_types)?;
    Ok(files
        .iter()
        .filter(|file| {
            let absolute = normalize_path(&file.source.absolute_path);
            let relative = normalize_path(&file.source.relative_path);
            (include.is_empty()
                || include.iter().any(|matcher| {
                    matcher.is_match(if matcher.absolute {
                        &absolute
                    } else {
                        &relative
                    })
                }))
                && !exclude.iter().any(|matcher| {
                    matcher.is_match(if matcher.absolute {
                        &absolute
                    } else {
                        &relative
                    })
                })
                && matches_ordered_globs(&relative, &ordered_globs)
                && !types.matched(&file.source.relative_path, false).is_ignore()
                && plan.modified_after_epoch_ms.is_none_or(|after| {
                    file.source
                        .snapshot
                        .modified_epoch_ms
                        .is_some_and(|modified| modified >= after)
                })
                && plan.modified_before_epoch_ms.is_none_or(|before| {
                    file.source
                        .snapshot
                        .modified_epoch_ms
                        .is_some_and(|modified| modified <= before)
                })
        })
        .map(|file| file.source.id.clone())
        .collect())
}

struct PathMatcher {
    normalized: String,
    matcher: Option<GlobMatcher>,
    absolute: bool,
}

impl PathMatcher {
    fn path(pattern: &str) -> Result<Self, EngineError> {
        let normalized = normalize_pattern(pattern);
        if normalized.is_empty() {
            return Err(EngineError::invalid_argument(
                "path filter must not be empty",
            ));
        }
        let absolute = is_absolute_pattern(&normalized);
        let matcher = has_glob(&normalized)
            .then(|| compile_glob(&normalized, false))
            .transpose()?;
        Ok(Self {
            normalized,
            matcher,
            absolute,
        })
    }

    fn is_match(&self, path: &str) -> bool {
        if let Some(matcher) = &self.matcher {
            return matcher.is_match(path)
                || self
                    .normalized
                    .strip_suffix("/**")
                    .is_some_and(|directory| {
                        compile_glob(directory, false).is_ok_and(|matcher| matcher.is_match(path))
                    });
        }
        path == self.normalized
            || path.starts_with(&format!("{}/", self.normalized.trim_end_matches('/')))
    }
}

struct OrderedGlob {
    matcher: GlobMatcher,
    negated: bool,
}

fn ordered_globs(
    sensitive: &[String],
    insensitive: &[String],
) -> Result<Vec<OrderedGlob>, EngineError> {
    sensitive
        .iter()
        .map(|pattern| (pattern, false))
        .chain(insensitive.iter().map(|pattern| (pattern, true)))
        .filter_map(|(pattern, insensitive)| {
            let pattern = pattern.trim();
            (!pattern.is_empty()).then_some((pattern, insensitive))
        })
        .map(|(pattern, insensitive)| {
            let (negated, pattern) = pattern
                .strip_prefix('!')
                .map_or((false, pattern), |pattern| (true, pattern.trim()));
            if pattern.is_empty() {
                return Err(EngineError::invalid_argument("glob must not be empty"));
            }
            Ok(OrderedGlob {
                matcher: compile_glob(pattern, insensitive)?,
                negated,
            })
        })
        .collect()
}

fn matches_ordered_globs(path: &str, globs: &[OrderedGlob]) -> bool {
    let mut included = !globs.iter().any(|rule| !rule.negated);
    for rule in globs {
        if rule.matcher.is_match(path) {
            included = !rule.negated;
        }
    }
    included
}

fn compile_glob(pattern: &str, case_insensitive: bool) -> Result<GlobMatcher, EngineError> {
    let normalized = normalize_pattern(pattern);
    let pattern = if !normalized.contains('/') && normalized != "**" {
        format!("**/{normalized}")
    } else {
        normalized
    };
    let mut builder = GlobBuilder::new(&pattern);
    builder
        .case_insensitive(case_insensitive)
        .literal_separator(true)
        .backslash_escape(false)
        .allow_unclosed_class(true);
    builder
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| {
            EngineError::invalid_argument(format!("invalid glob {pattern:?}: {error}"))
        })
}

fn build_file_types(included: &[String], excluded: &[String]) -> Result<Types, EngineError> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    for name in included {
        builder.select(&file_type_name(name));
    }
    for name in excluded {
        builder.negate(&file_type_name(name));
    }
    builder.build().map_err(|error| {
        EngineError::invalid_argument(format!("invalid ripgrep file type selection: {error}"))
    })
}

fn file_type_name(name: &str) -> String {
    let normalized = name.trim().trim_start_matches('.').to_ascii_lowercase();
    match normalized.as_str() {
        "bash" | "zsh" => "sh",
        "cjs" | "jsx" | "mjs" => "js",
        "cp" | "cc" | "cxx" => "cpp",
        "hpp" | "hxx" | "hh" => "h",
        "markdown" | "mdx" => "md",
        "pyi" => "py",
        "rb" => "ruby",
        "rs" => "rust",
        "tsx" => "ts",
        "yml" => "yaml",
        _ => &normalized,
    }
    .to_owned()
}

fn filter_matches_no_files(filter: Option<&StorageSearchFilter>) -> bool {
    filter
        .and_then(|filter| filter.file_ids.as_ref())
        .is_some_and(Vec::is_empty)
}

fn normalize_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    if !is_absolute_pattern(&normalized) {
        while let Some(stripped) = normalized.strip_prefix("./") {
            normalized = stripped.to_owned();
        }
    }
    normalized
}

fn normalize_path(path: &Path) -> String {
    normalize_pattern(&path.to_string_lossy())
}

fn is_absolute_pattern(pattern: &str) -> bool {
    pattern.starts_with('/')
        || pattern.as_bytes().get(1) == Some(&b':') && pattern.as_bytes().get(2) == Some(&b'/')
}

fn has_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
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
        api::context::{
            options::{ContextRoute, ContextRouteMode},
            result::MatchedBy,
        },
        domain::{
            Content, Entity, EntityContent, EntityFragment, EntityId, FileFormat, FileId,
            FileSnapshot, FragmentId, SourceFile, SourceRange, TextRange, WindowFragment,
        },
        models::{
            EmbeddingInputKind, EmbeddingMetric, EmbeddingModelInfo, EmbeddingModelLimits,
            ModelError,
        },
        storage::spi::{
            FileIndexDiagnostics, FileIndexStatus, IndexedFragment, StorageResult,
            StorageSearchFilter, StorageSearchHit, StorageSearchPath, StoredEntity, StoredFile,
            WorkspaceIndexStorage,
        },
    };

    use super::{
        SearchEmbeddingRuntime, SearchPlan, file_type_name, normalize_path, search_workspace_index,
    };

    #[test]
    fn normalizes_absolute_paths_and_ripgrep_file_type_aliases() {
        assert_eq!(
            normalize_path(Path::new("/workspace/src/lib.rs")),
            "/workspace/src/lib.rs"
        );
        assert_eq!(file_type_name(".RS"), "rust");
        assert_eq!(file_type_name("ALL"), "all");
    }

    struct FixtureModel {
        info: EmbeddingModelInfo,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FixtureModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    reference: "local/fixture".to_owned(),
                    provider: "local".to_owned(),
                    name: "fixture".to_owned(),
                    dimension: 2,
                    metric: EmbeddingMetric::Cosine,
                    endpoint: None,
                    default_concurrency: Some(1),
                    input_kinds: vec![EmbeddingInputKind::Text],
                    limits: EmbeddingModelLimits {
                        max_batch_size: 8,
                        max_input_tokens: None,
                        max_image_bytes: None,
                    },
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
        files: Vec<StoredFile>,
        entities: HashMap<String, StoredEntity>,
        fts: HashMap<String, Vec<StorageSearchHit>>,
        vector: Vec<StorageSearchHit>,
        filters: Arc<Mutex<Vec<Option<StorageSearchFilter>>>>,
    }

    impl FixtureStorage {
        fn hits_with_filter(
            &self,
            hits: &[StorageSearchHit],
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
                        .is_none_or(|ids| ids.contains(&hit.file.source.id))
                })
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.entity_ids.as_ref())
                        .is_none_or(|ids| ids.contains(hit.fragment.entity_id()))
                })
                .take(limit)
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl WorkspaceIndexStorage for FixtureStorage {
        fn is_read_only(&self) -> bool {
            true
        }

        fn list_files(&self) -> StorageResult<Vec<StoredFile>> {
            Ok(self.files.clone())
        }

        fn get_entity(&self, entity_id: &EntityId) -> StorageResult<Option<StoredEntity>> {
            Ok(self.entities.get(entity_id.as_str()).cloned())
        }

        fn search_fts(
            &self,
            query: &str,
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(self.hits_with_filter(
                self.fts.get(query).map_or(&[], Vec::as_slice),
                limit,
                filter,
            ))
        }

        fn search_vector(
            &self,
            _vector: &[f32],
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(self.hits_with_filter(&self.vector, limit, filter))
        }

        fn replace_file(
            &self,
            _file: &StoredFile,
            _entries: &[IndexedFragment],
            _diagnostics: Option<&FileIndexDiagnostics>,
        ) -> StorageResult<()> {
            Ok(())
        }

        fn mark_file_failed(&self, _file: &StoredFile, _error: &str) -> StorageResult<()> {
            Ok(())
        }

        fn delete_file(&self, _file_id: &FileId) -> StorageResult<()> {
            Ok(())
        }

        async fn finalize_writes(&self) -> StorageResult<()> {
            Ok(())
        }

        fn close(&self) -> StorageResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn fuses_fts_and_vector_routes_with_main_compatible_rrf() {
        let file = file("file", "src/lib.rs", "rust", 100);
        let entity_a = entity("a", &file, "alpha entity");
        let entity_b = entity("b", &file, "beta entity");
        let a_fts = hit(&entity_a, "a-fts", StorageSearchPath::Fts, 9.0);
        let b_fts = hit(&entity_b, "b-fts", StorageSearchPath::Fts, 8.0);
        let b_vector = hit(&entity_b, "b-vector", StorageSearchPath::Vector, 0.9);
        let a_vector = hit(&entity_a, "a-vector", StorageSearchPath::Vector, 0.8);
        let storage = FixtureStorage {
            files: vec![file],
            entities: HashMap::from([("a".to_owned(), entity_a), ("b".to_owned(), entity_b)]),
            fts: HashMap::from([("alpha".to_owned(), vec![a_fts, b_fts])]),
            vector: vec![b_vector, a_vector],
            filters: Arc::new(Mutex::new(Vec::new())),
        };
        let model = FixtureModel::new();

        let result = search_workspace_index(
            plan(vec![
                ContextRoute {
                    mode: ContextRouteMode::Fts,
                    query: "alpha".to_owned(),
                },
                ContextRoute {
                    mode: ContextRouteMode::Vector,
                    query: "alpha".to_owned(),
                },
            ]),
            &storage,
            Some(&model),
        )
        .await
        .expect("hybrid search");

        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].entity.id.as_str(), "a");
        assert_eq!(result.hits[1].entity.id.as_str(), "b");
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
    async fn pushes_file_and_symbol_filters_into_storage() {
        let source = file("source", "src/service.rs", "rust", 200);
        let docs = file("docs", "docs/service.md", "markdown", 200);
        let source_entity = entity("source-entity", &source, "Service implementation");
        let docs_entity = entity("docs-entity", &docs, "Service documentation");
        let filters = Arc::new(Mutex::new(Vec::new()));
        let storage = FixtureStorage {
            files: vec![source.clone(), docs.clone()],
            entities: HashMap::from([
                ("source-entity".to_owned(), source_entity.clone()),
                ("docs-entity".to_owned(), docs_entity.clone()),
            ]),
            fts: HashMap::from([(
                "Service".to_owned(),
                vec![
                    hit(&source_entity, "source-hit", StorageSearchPath::Fts, 2.0),
                    hit(&docs_entity, "docs-hit", StorageSearchPath::Fts, 1.0),
                ],
            )]),
            vector: Vec::new(),
            filters: Arc::clone(&filters),
        };
        let mut plan = plan(vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "Service".to_owned(),
        }]);
        plan.include_paths = vec!["src".to_owned()];
        plan.file_types = vec!["rs".to_owned()];
        plan.prefer_symbol = true;

        let result = search_workspace_index(plan, &storage, None)
            .await
            .expect("filtered search");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].file.source.id.as_str(), "source");
        let filters = filters.lock().expect("captured filters");
        assert!(filters.iter().flatten().all(|filter| {
            filter.file_ids.as_deref() == Some(&[FileId::new("source").expect("file id")])
        }));
        assert!(filters.iter().flatten().any(|filter| {
            filter
                .symbol_names
                .as_ref()
                .is_some_and(|names| names.contains(&"Service".to_owned()))
        }));
    }

    fn plan(routes: Vec<ContextRoute>) -> SearchPlan {
        SearchPlan {
            routes,
            limit: Some(10),
            trace: true,
            prefer_symbol: false,
            symbol_types: Vec::new(),
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            globs: Vec::new(),
            insensitive_globs: Vec::new(),
            file_types: Vec::new(),
            excluded_file_types: Vec::new(),
            modified_after_epoch_ms: None,
            modified_before_epoch_ms: None,
        }
    }

    fn file(id: &str, relative: &str, format: &str, modified: u64) -> StoredFile {
        StoredFile {
            source: SourceFile {
                id: FileId::new(id).expect("file id"),
                absolute_path: PathBuf::from("/workspace").join(relative),
                relative_path: PathBuf::from(relative),
                root_path: PathBuf::from("/workspace"),
                formats: vec![match format {
                    "rust" => FileFormat::Rust,
                    "markdown" => FileFormat::Markdown,
                    _ => panic!("unexpected fixture format"),
                }],
                snapshot: FileSnapshot {
                    size_bytes: 100,
                    modified_epoch_ms: Some(modified),
                    content_hash: None,
                },
            },
            index_status: Some(FileIndexStatus {
                indexed_epoch_ms: Some(modified),
                entity_count: 1,
                token_count: None,
                truncated_fragment_count: None,
                error: None,
            }),
        }
    }

    fn entity(id: &str, file: &StoredFile, content: &str) -> StoredEntity {
        StoredEntity {
            entity: Entity {
                id: EntityId::new(id).expect("entity id"),
                file_id: file.source.id.clone(),
                range: text_range(1, 8),
                content: EntityContent::Source(vec![Content::Text(content.to_owned())]),
                metadata: None,
            },
            file: file.clone(),
        }
    }

    fn hit(
        stored: &StoredEntity,
        fragment_id: &str,
        path: StorageSearchPath,
        score: f64,
    ) -> StorageSearchHit {
        StorageSearchHit {
            fragment: EntityFragment::Window(WindowFragment {
                id: FragmentId::new(fragment_id).expect("fragment id"),
                entity_id: stored.entity.id.clone(),
                file_id: stored.file.source.id.clone(),
                range: text_range(2, 3),
                contents: vec![Content::Text(format!(
                    "{} source",
                    stored.entity.id.as_str()
                ))],
                metadata: None,
            }),
            file: stored.file.clone(),
            path,
            score,
        }
    }

    fn text_range(start_line: usize, end_line: usize) -> SourceRange {
        SourceRange::Text(TextRange {
            start_line,
            end_line,
            start_utf16_offset: 0,
            end_utf16_offset: 10,
        })
    }
}
