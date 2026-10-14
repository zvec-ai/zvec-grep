use super::storage::SearchStorage;

use std::{collections::HashSet, fs, path::Path};

use crate::{
    EngineError,
    api::context::{
        ContextOptions, ContextResult,
        options::{ContextRoute, ContextRouteMode, QueryImage},
        result::{
            ContentPreview, ContentRange, ContextContentRole, ContextCoverage, ContextDiagnostics,
            ContextGroupResult, ContextItem, ContextItemKind, ContextItemStatus,
            ContextQueryGroupMatch, ContextQueryGroupRole, ContextSelectionReason, ContextSource,
            ContextWorkspaceIndex, EmptyReason, IndexDiagnostics, IndexQueryGroupDiagnostics,
            IndexResultGroup, IndexRouteDiagnostics, IndexScoring, IndexTargetDiagnostics,
            IndexTargetStatus, MatchedBy,
        },
    },
    domain::{Content, ContentKind, EntityId, FileRecord, ImageContent, Range, Workspace},
    models::QueryEncoding,
    utils::sha256_hex,
};

use super::pipeline::{
    RecalledSearch, SearchEmbeddingRuntime, SearchHit, SearchPlan, SearchPlanResult,
    recall_workspace_index,
};

const DEFAULT_CONTEXT_TOTAL_LIMIT: usize = 30;
const DEFAULT_CONTEXT_PRIORITY_LIMIT: usize = 6;
const CONTEXT_GROUP_RRF_K: f64 = 60.0;

#[derive(Clone, Debug)]
pub(crate) struct NormalizedContextRequest {
    pub image: Option<ImageContent>,
    pub display_query: String,
    pub routes: Vec<ContextRoute>,
    pub groups: Vec<NormalizedContextGroup>,
}

#[derive(Clone, Debug)]
pub(crate) struct NormalizedContextGroup {
    pub id: String,
    pub query: String,
    pub role: ContextQueryGroupRole,
    pub routes: Vec<ContextRoute>,
}

pub(crate) fn normalize_context_request(
    options: &ContextOptions,
) -> Result<NormalizedContextRequest, EngineError> {
    options.validate_file_selection()?;
    if let Some(image) = &options.query_image {
        return normalize_image_request(image);
    }
    let primary_queries = options
        .query
        .iter()
        .chain(&options.queries)
        .filter_map(|query| {
            let query = query.trim();
            (!query.is_empty()).then(|| query.to_owned())
        })
        .collect::<Vec<_>>();
    let routes = options
        .routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let query = route.query.trim();
            if query.is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "context route {index} requires a non-empty query"
                )));
            }
            Ok(ContextRoute {
                mode: route.mode,
                query: query.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if primary_queries.is_empty() && routes.is_empty() {
        return Err(EngineError::invalid_argument(
            "context requires a non-empty query or route",
        ));
    }

    let mut groups = primary_queries
        .iter()
        .enumerate()
        .map(|(index, query)| NormalizedContextGroup {
            id: format!("Q{}", index + 1),
            query: query.clone(),
            role: ContextQueryGroupRole::Primary,
            routes: vec![
                ContextRoute {
                    mode: ContextRouteMode::Fts,
                    query: query.clone(),
                },
                ContextRoute {
                    mode: ContextRouteMode::Vector,
                    query: query.clone(),
                },
            ],
        })
        .collect::<Vec<_>>();
    groups.extend(
        routes
            .iter()
            .enumerate()
            .map(|(index, route)| NormalizedContextGroup {
                id: format!("Q{}", primary_queries.len() + index + 1),
                query: route.query.clone(),
                role: ContextQueryGroupRole::Supplemental,
                routes: vec![route.clone()],
            }),
    );
    let all_routes = groups
        .iter()
        .flat_map(|group| group.routes.iter().cloned())
        .collect::<Vec<_>>();
    let display_query = if primary_queries.is_empty() {
        routes
            .iter()
            .map(|route| route.query.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    } else {
        primary_queries.join(" | ")
    };

    Ok(NormalizedContextRequest {
        image: None,
        display_query,
        routes: all_routes,
        groups,
    })
}

fn normalize_image_request(image: &QueryImage) -> Result<NormalizedContextRequest, EngineError> {
    let (image, display_query) = match image {
        QueryImage::Path { path } => (
            crate::extraction::read_image(path)?,
            format!("image:{}", path.display()),
        ),
        QueryImage::Bytes { format, data } => (
            crate::extraction::prepare_image(data.clone(), *format)?,
            format!("image:{}", format.as_str()),
        ),
    };
    let routes = vec![ContextRoute {
        mode: ContextRouteMode::Vector,
        query: display_query.clone(),
    }];
    Ok(NormalizedContextRequest {
        image: Some(image),
        display_query: display_query.clone(),
        routes: routes.clone(),
        groups: vec![NormalizedContextGroup {
            id: "Q1".to_owned(),
            query: display_query,
            role: ContextQueryGroupRole::Primary,
            routes,
        }],
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn context_from_index(
    root: &Path,
    workspace: &Workspace,
    workspace_home: &Path,
    storage: &dyn SearchStorage,
    embedding_models: &[&dyn SearchEmbeddingRuntime],
    options: &ContextOptions,
    request: &NormalizedContextRequest,
) -> Result<ContextResult, EngineError> {
    let targets = super::service::query_targets(workspace, options, request)?;
    let groups = query_groups(options, request);
    let limit = options.limit.unwrap_or(DEFAULT_CONTEXT_TOTAL_LIMIT);
    let cached = embedding_models
        .iter()
        .map(|model| super::pipeline::CachedSearchRuntime::new(*model))
        .collect::<Vec<_>>();
    let mut targets_diagnostics = Vec::new();
    let mut recalls = Vec::new();
    let mut result = empty_index_context(root, workspace, workspace_home, request, &groups)?;
    let mut first_error = None;
    for target in targets {
        let model_ref = target.schema.model().reference();
        let mut diagnostic = IndexTargetDiagnostics {
            kind: target.kind,
            model_ref: model_ref.clone(),
            status: IndexTargetStatus::Searched,
            reason: None,
        };
        if let Some(reason) = &target.skip_reason {
            diagnostic.status = IndexTargetStatus::Skipped;
            diagnostic.reason = Some(reason.clone());
            targets_diagnostics.push(diagnostic);
            continue;
        }
        let recall = recall_table(
            &workspace.root,
            &target,
            &groups,
            storage,
            &cached,
            options,
            request,
        )
        .await;
        let recall = match recall {
            Ok(recall) => recall,
            Err(error) => {
                if options.target_kind.is_some() || error.code() == EngineError::CANCELLED {
                    return Err(error);
                }
                diagnostic.status = IndexTargetStatus::Failed;
                diagnostic.reason = Some(error.message().to_owned());
                if first_error.is_none() {
                    first_error = Some(error);
                }
                targets_diagnostics.push(diagnostic);
                continue;
            }
        };
        pool_table_recall(&mut recalls, recall);
        targets_diagnostics.push(diagnostic);
    }
    let mut bundles = Vec::new();
    for recall in recalls {
        let kinds = recall.kinds.clone();
        let finished = finish_table_recall(recall, &workspace.root, request, storage);
        match finished {
            Ok((group, table_result)) => {
                append_table_result(&mut result, &mut bundles, group, table_result);
            }
            Err(error) => {
                if options.target_kind.is_some() || error.code() == EngineError::CANCELLED {
                    return Err(error);
                }
                for diagnostic in &mut targets_diagnostics {
                    if kinds.contains(&diagnostic.kind) {
                        diagnostic.status = IndexTargetStatus::Failed;
                        diagnostic.reason = Some(error.message().to_owned());
                    }
                }
                first_error.get_or_insert(error);
            }
        }
    }
    if bundles.is_empty()
        && let Some(error) = first_error
    {
        return Err(error);
    }
    Ok(finish_context_result(
        result,
        bundles,
        targets_diagnostics,
        limit,
    ))
}

fn empty_index_context(
    root: &Path,
    workspace: &Workspace,
    workspace_home: &Path,
    request: &NormalizedContextRequest,
    groups: &[NormalizedContextGroup],
) -> Result<ContextResult, EngineError> {
    Ok(ContextResult {
        freshness: None,
        background_refresh: None,
        query: request.display_query.clone(),
        root: root.to_path_buf(),
        source: ContextSource::Index,
        coverage: ContextCoverage::RankedSample,
        workspace_index: Some(ContextWorkspaceIndex {
            name: workspace.name.clone(),
            path: workspace_home.to_path_buf(),
        }),
        items: Vec::new(),
        group_results: Vec::new(),
        diagnostics: build_table_result(&workspace.root, request, groups, Vec::new())?.diagnostics,
    })
}

fn query_groups(
    options: &ContextOptions,
    request: &NormalizedContextRequest,
) -> Vec<NormalizedContextGroup> {
    if options.fuse {
        vec![NormalizedContextGroup {
            id: "Q1".to_owned(),
            query: request.display_query.clone(),
            role: ContextQueryGroupRole::Primary,
            routes: request.routes.clone(),
        }]
    } else {
        request.groups.clone()
    }
}

struct TableRecall {
    schema: crate::domain::EmbeddingModelInfo,
    kinds: Vec<ContentKind>,
    model_refs: Vec<String>,
    query_encoding: Option<QueryEncoding>,
    groups: Vec<NormalizedContextGroup>,
    scoring: IndexScoring,
    searches: Vec<RecalledSearch>,
}

#[allow(clippy::too_many_arguments)]
async fn recall_table(
    root: &Path,
    target: &super::service::QueryTarget<'_>,
    groups: &[NormalizedContextGroup],
    storage: &dyn SearchStorage,
    cached: &[super::pipeline::CachedSearchRuntime<'_>],
    options: &ContextOptions,
    request: &NormalizedContextRequest,
) -> Result<TableRecall, EngineError> {
    let groups = target_query_groups(groups, target.kind);
    let prefer_symbol = options.prefer_symbol && target.kind == ContentKind::Code;
    let scoring = table_scoring(&groups, prefer_symbol);
    let model_ref = target.schema.model().reference();
    let uses_vectors = groups
        .iter()
        .flat_map(|group| &group.routes)
        .any(|route| route.mode == ContextRouteMode::Vector);
    let runtime = uses_vectors
        .then(|| {
            cached
                .iter()
                .find(|runtime| runtime.info().model().reference() == model_ref)
        })
        .flatten();
    let searches = recall_target(
        root,
        SearchPlan {
            model_ref,
            kind: target.kind,
            image: request.image.clone(),
            routes: Vec::new(),
            limit: Some(options.limit.unwrap_or(DEFAULT_CONTEXT_TOTAL_LIMIT)),
            trace: options.trace,
            prefer_symbol,
            filter: options.filter.clone(),
        },
        &groups,
        storage,
        runtime,
    )
    .await?;
    let query_encoding = runtime
        .filter(|runtime| runtime.has_pinned_weights())
        .map(|runtime| {
            if let Some(encoding) = searches.first().and_then(|search| search.query_encoding) {
                return Ok(encoding);
            }
            let input = if request.image.is_some() {
                ContentKind::Image
            } else {
                ContentKind::Text
            };
            runtime
                .query_encoding(input, target.kind)
                .map_err(crate::models::ModelError::into_engine_error)
        })
        .transpose()?;
    Ok(TableRecall {
        schema: target.schema.clone(),
        kinds: vec![target.kind],
        model_refs: vec![target.schema.model().reference()],
        query_encoding,
        groups,
        scoring,
        searches,
    })
}

#[derive(Clone)]
struct IndexedContextItem {
    entity_id: EntityId,
    item: ContextItem,
}

struct TableContextResult {
    items: Vec<IndexedContextItem>,
    diagnostics: ContextDiagnostics,
}

struct RankedResultGroup {
    group: IndexResultGroup,
    items: Vec<IndexedContextItem>,
}

fn target_query_groups(
    groups: &[NormalizedContextGroup],
    kind: ContentKind,
) -> Vec<NormalizedContextGroup> {
    groups
        .iter()
        .filter_map(|group| {
            let mut group = group.clone();
            if kind == ContentKind::Image {
                group
                    .routes
                    .retain(|route| route.mode == ContextRouteMode::Vector);
            }
            (!group.routes.is_empty()).then_some(group)
        })
        .collect()
}

fn table_scoring(groups: &[NormalizedContextGroup], prefer_symbol: bool) -> IndexScoring {
    if groups.len() == 1 && groups[0].routes.len() == 1 && !prefer_symbol {
        match groups[0].routes[0].mode {
            ContextRouteMode::Vector => IndexScoring::Vector,
            ContextRouteMode::Fts => IndexScoring::FullText,
        }
    } else if groups.len() == 1 {
        IndexScoring::Hybrid
    } else {
        IndexScoring::MultiQuery
    }
}

async fn recall_target(
    root: &Path,
    plan: SearchPlan,
    groups: &[NormalizedContextGroup],
    storage: &dyn SearchStorage,
    runtime: Option<&super::pipeline::CachedSearchRuntime<'_>>,
) -> Result<Vec<RecalledSearch>, EngineError> {
    if let Some(runtime) = runtime {
        runtime.ensure_available()?;
    }
    let runtimes = runtime
        .map(|runtime| vec![runtime as &dyn SearchEmbeddingRuntime])
        .unwrap_or_default();
    let mut searches = Vec::new();
    for group in groups {
        let mut plan = plan.clone();
        plan.routes.clone_from(&group.routes);
        searches.push(recall_workspace_index(root, plan, storage, &runtimes).await?);
    }
    Ok(searches)
}

fn pool_table_recall(pools: &mut Vec<TableRecall>, mut next: TableRecall) {
    let uses_vectors = next
        .groups
        .iter()
        .flat_map(|group| &group.routes)
        .any(|route| route.mode == ContextRouteMode::Vector);
    let pool = pools.iter_mut().find(|pool| {
        !pool.kinds.contains(&ContentKind::Image)
            && !next.kinds.contains(&ContentKind::Image)
            && (!uses_vectors
                || (next.query_encoding.is_some()
                    && next.query_encoding == pool.query_encoding
                    && next.schema.model().reference() == pool.schema.model().reference()
                    && next.schema.dimension() == pool.schema.dimension()
                    && next.schema.metric() == pool.schema.metric()))
    });
    if let Some(pool) = pool {
        pool.kinds.append(&mut next.kinds);
        for model in next.model_refs {
            if !pool.model_refs.contains(&model) {
                pool.model_refs.push(model);
            }
        }
        if next.scoring == IndexScoring::Hybrid {
            pool.scoring = IndexScoring::Hybrid;
        }
        if pool.searches.is_empty() {
            pool.searches = next.searches;
        } else {
            for (existing, search) in pool.searches.iter_mut().zip(next.searches) {
                existing.merge(search);
            }
        }
    } else {
        pools.push(next);
    }
}

fn finish_table_recall(
    recall: TableRecall,
    root: &Path,
    request: &NormalizedContextRequest,
    storage: &dyn SearchStorage,
) -> Result<(IndexResultGroup, TableContextResult), EngineError> {
    let searches = recall
        .searches
        .into_iter()
        .map(|search| search.finish(storage))
        .collect::<Result<Vec<_>, _>>()?;
    let result = build_table_result(root, request, &recall.groups, searches)?;
    let group = IndexResultGroup {
        id: recall.kinds[0].as_str().to_owned(),
        rule_ranking: !recall.kinds.contains(&ContentKind::Image),
        kinds: recall.kinds,
        model_refs: recall.model_refs,
        scoring: recall.scoring,
        item_start: 0,
        item_count: 0,
    };
    Ok((group, result))
}

fn append_table_result(
    result: &mut ContextResult,
    bundles: &mut Vec<RankedResultGroup>,
    group: IndexResultGroup,
    table_result: TableContextResult,
) {
    result
        .diagnostics
        .timings
        .extend(table_result.diagnostics.timings);
    if let Some(index) = table_result.diagnostics.index {
        result
            .diagnostics
            .index
            .as_mut()
            .expect("index diagnostics")
            .routes
            .extend(index.routes);
    }
    bundles.push(RankedResultGroup {
        group,
        items: table_result.items,
    });
}

fn finish_context_result(
    mut result: ContextResult,
    bundles: Vec<RankedResultGroup>,
    targets: Vec<IndexTargetDiagnostics>,
    limit: usize,
) -> ContextResult {
    // Allocate a total cap fairly without inventing a global relevance rank.
    let mut allocations = vec![0; bundles.len()];
    let mut remaining = limit;
    while remaining > 0 {
        let mut advanced = false;
        for (allocation, bundle) in allocations.iter_mut().zip(&bundles) {
            if remaining > 0 && *allocation < bundle.items.len() {
                *allocation += 1;
                remaining -= 1;
                advanced = true;
            }
        }
        if !advanced {
            break;
        }
    }
    let mut items = Vec::new();
    let mut result_groups = Vec::new();
    for (mut bundle, allocation) in bundles.into_iter().zip(allocations) {
        bundle.items.truncate(allocation);
        for (i, item) in bundle.items.iter_mut().enumerate() {
            item.item.rank = i + 1;
            if let Some(trace) = &mut item.item.trace {
                trace.final_selection.cutoff_rank = allocation;
            }
        }
        bundle.group.item_start = items.len();
        bundle.group.item_count = bundle.items.len();
        items.extend(bundle.items.into_iter().map(|candidate| candidate.item));
        result_groups.push(bundle.group);
    }
    result.diagnostics.empty_reason = if !items.is_empty() {
        None
    } else if targets
        .iter()
        .all(|t| t.status == IndexTargetStatus::Skipped)
    {
        Some(EmptyReason::NoSupportedTargets)
    } else {
        Some(EmptyReason::NoMatches)
    };
    let index = result
        .diagnostics
        .index
        .as_mut()
        .expect("index diagnostics");
    index.incomplete = targets
        .iter()
        .any(|t| t.status == IndexTargetStatus::Failed);
    index.targets = targets;
    index.result_groups = result_groups;
    index.limit = limit;
    index.hits_returned = items.len();
    result.group_results = index
        .query_groups
        .iter()
        .map(|group| ContextGroupResult {
            id: group.id.clone(),
            query: group.query.clone(),
            role: group.role,
            items: items
                .iter()
                .filter(|item| {
                    item.query_groups
                        .iter()
                        .any(|matched| matched.id == group.id)
                })
                .cloned()
                .collect(),
        })
        .collect();
    result.items = items;
    result
}

fn build_table_result(
    workspace_root: &Path,
    request: &NormalizedContextRequest,
    groups: &[NormalizedContextGroup],
    searches: Vec<SearchPlanResult>,
) -> Result<TableContextResult, EngineError> {
    let group_items = searches
        .iter()
        .zip(groups)
        .map(|(search, group)| search_plan_to_context_items(search, workspace_root, group))
        .collect::<Result<Vec<_>, _>>()?;
    let coverage_groups = groups
        .iter()
        .filter(|group| group.role == ContextQueryGroupRole::Primary)
        .map(|group| group.id.clone())
        .collect::<Vec<_>>();
    let items = select_and_rank_context_items(
        group_items.iter().flatten().cloned().collect(),
        &coverage_groups,
    );
    let timings = searches
        .iter()
        .flat_map(|search| search.timings.iter().cloned())
        .collect();
    let hits_returned = items.len();

    Ok(TableContextResult {
        items,
        diagnostics: ContextDiagnostics {
            empty_reason: (hits_returned == 0).then_some(EmptyReason::NoMatches),
            index: Some(IndexDiagnostics {
                input_kind: if request.image.is_some() {
                    ContentKind::Image
                } else {
                    ContentKind::Text
                },
                targets: Vec::new(),
                result_groups: Vec::new(),
                incomplete: false,
                limit: hits_returned,
                hits_returned,
                query_groups: groups
                    .iter()
                    .map(|group| IndexQueryGroupDiagnostics {
                        id: group.id.clone(),
                        query: group.query.clone(),
                        role: group.role,
                    })
                    .collect(),
                routes: searches
                    .into_iter()
                    .flat_map(|search| search.routes)
                    .map(|route| IndexRouteDiagnostics {
                        id: route.id,
                        mode: route.mode,
                        query: route.query,
                    })
                    .collect(),
            }),
            rg: None,
            structure: None,
            timings,
        },
    })
}

fn search_plan_to_context_items(
    result: &SearchPlanResult,
    workspace_root: &Path,
    group: &NormalizedContextGroup,
) -> Result<Vec<IndexedContextItem>, EngineError> {
    result
        .hits
        .iter()
        .map(|hit| {
            let target = context_item_target(hit)?;
            Ok(IndexedContextItem {
                entity_id: hit.entity.id.clone(),
                item: ContextItem {
                    kind: ContextItemKind::IndexedEntity,
                    rank: hit.rank,
                    absolute_path: workspace_root.join(&hit.file.relative_path),
                    relative_path: hit.file.relative_path.to_path_buf(),
                    range: hit.entity.source_range.into(),
                    excerpt_range: target.excerpt_range,
                    content_range: target.content_range,
                    preview: target.preview,
                    outline: None,
                    content_role: Some(target.content_role),
                    status: file_freshness_status(workspace_root, &hit.file),
                    score: Some(hit.score),
                    matched_by: hit.matched_by,
                    metadata: hit.entity.metadata.clone(),
                    container: None,
                    trace: hit.trace.clone(),
                    query_groups: vec![ContextQueryGroupMatch {
                        id: group.id.clone(),
                        query: group.query.clone(),
                        role: group.role,
                        rank: hit.rank,
                        matched_by: hit.matched_by,
                    }],
                    selection_reason: None,
                    coverage_group: None,
                },
            })
        })
        .collect()
}

fn select_and_rank_context_items(
    items: Vec<IndexedContextItem>,
    coverage_group_ids: &[String],
) -> Vec<IndexedContextItem> {
    let mut deduped: Vec<IndexedContextItem> = Vec::new();
    for item in items {
        let key = item.entity_id.clone();
        if let Some(existing) = deduped
            .iter_mut()
            .find(|candidate| candidate.entity_id == key)
        {
            merge_query_group_matches(&mut existing.item.query_groups, &item.item.query_groups);
            existing.item.matched_by = merged_matched_by(&existing.item.query_groups);
        } else {
            deduped.push(item);
        }
    }
    let mut globally_ranked = deduped.clone();
    globally_ranked.sort_by(compare_global_rank);
    let mut selected = HashSet::<EntityId>::new();
    let mut prioritized = Vec::new();

    for group_id in coverage_group_ids {
        if prioritized.len() >= DEFAULT_CONTEXT_PRIORITY_LIMIT {
            break;
        }
        let mut candidates = deduped
            .iter()
            .filter(|item| !selected.contains(&item.entity_id))
            .filter(|item| query_group_match(&item.item, group_id).is_some())
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| compare_group_rank(left, right, group_id));
        if let Some(mut candidate) = candidates.into_iter().next() {
            selected.insert(candidate.entity_id.clone());
            candidate.item.selection_reason = Some(ContextSelectionReason::Coverage);
            candidate.item.coverage_group = Some(group_id.clone());
            prioritized.push(candidate);
        }
    }
    for mut candidate in globally_ranked.iter().cloned() {
        if prioritized.len() >= DEFAULT_CONTEXT_PRIORITY_LIMIT {
            break;
        }
        let key = candidate.entity_id.clone();
        if selected.insert(key) {
            candidate.item.selection_reason = Some(ContextSelectionReason::GlobalFill);
            candidate.item.coverage_group = None;
            prioritized.push(candidate);
        }
    }
    prioritized.extend(
        globally_ranked
            .into_iter()
            .filter(|item| !selected.contains(&item.entity_id)),
    );
    for (index, item) in prioritized.iter_mut().enumerate() {
        item.item.rank = index + 1;
    }
    prioritized
}

fn merge_query_group_matches(
    target: &mut Vec<ContextQueryGroupMatch>,
    additional: &[ContextQueryGroupMatch],
) {
    for item in additional {
        if let Some(existing) = target.iter_mut().find(|candidate| candidate.id == item.id) {
            if item.rank < existing.rank {
                *existing = item.clone();
            }
        } else {
            target.push(item.clone());
        }
    }
    target.sort_by_key(|item| context_group_number(&item.id));
}

fn merged_matched_by(matches: &[ContextQueryGroupMatch]) -> MatchedBy {
    let has_fts = matches
        .iter()
        .any(|item| matches!(item.matched_by, MatchedBy::Fts | MatchedBy::FtsAndVector));
    let has_vector = matches
        .iter()
        .any(|item| matches!(item.matched_by, MatchedBy::Vector | MatchedBy::FtsAndVector));
    match (has_fts, has_vector) {
        (true, true) => MatchedBy::FtsAndVector,
        (false, true) => MatchedBy::Vector,
        (true | false, false) => MatchedBy::Fts,
    }
}

fn compare_global_rank(
    left: &IndexedContextItem,
    right: &IndexedContextItem,
) -> std::cmp::Ordering {
    global_rrf_score(&right.item)
        .total_cmp(&global_rrf_score(&left.item))
        .then_with(|| best_group_rank(&left.item).cmp(&best_group_rank(&right.item)))
        .then_with(|| first_group_number(&left.item).cmp(&first_group_number(&right.item)))
        .then_with(|| left.entity_id.cmp(&right.entity_id))
}

fn global_rrf_score(item: &ContextItem) -> f64 {
    item.query_groups
        .iter()
        .map(|group| 1.0 / (CONTEXT_GROUP_RRF_K + rank_as_f64(group.rank)))
        .sum()
}

fn compare_group_rank(
    left: &IndexedContextItem,
    right: &IndexedContextItem,
    group_id: &str,
) -> std::cmp::Ordering {
    let left_rank = query_group_match(&left.item, group_id).map_or(usize::MAX, |group| group.rank);
    let right_rank =
        query_group_match(&right.item, group_id).map_or(usize::MAX, |group| group.rank);
    left_rank
        .cmp(&right_rank)
        .then_with(|| compare_global_rank(left, right))
}

fn query_group_match<'item>(
    item: &'item ContextItem,
    group_id: &str,
) -> Option<&'item ContextQueryGroupMatch> {
    item.query_groups.iter().find(|group| group.id == group_id)
}

fn best_group_rank(item: &ContextItem) -> usize {
    item.query_groups
        .iter()
        .map(|group| group.rank)
        .min()
        .unwrap_or(usize::MAX)
}

fn first_group_number(item: &ContextItem) -> usize {
    item.query_groups
        .iter()
        .map(|group| context_group_number(&group.id))
        .min()
        .unwrap_or(usize::MAX)
}

fn context_group_number(id: &str) -> usize {
    id.strip_prefix('Q')
        .and_then(|value| value.parse().ok())
        .unwrap_or(usize::MAX)
}

struct ContextItemTarget {
    preview: ContentPreview,
    content_range: ContentRange,
    content_role: ContextContentRole,
    excerpt_range: Option<ContentRange>,
}

fn context_item_target(hit: &SearchHit) -> Result<ContextItemTarget, EngineError> {
    let Some(evidence) = hit.evidence.first() else {
        return Ok(ContextItemTarget {
            preview: content_preview(&hit.entity.content),
            content_range: hit.entity.source_range.into(),
            content_role: ContextContentRole::Source,
            excerpt_range: None,
        });
    };
    let fragment = &evidence.fragment;
    let invalid_fragment = |error| {
        EngineError::storage_failure(format!(
            "invalid search fragment {}: {error}",
            fragment.id.as_str()
        ))
    };
    let (preview, excerpt_range) = match (fragment.range, &hit.entity.content) {
        (Range::Full, content) => (content_preview(content), None),
        (Range::Byte(range), Content::Text(text) | Content::Code(text)) => {
            let start = usize::try_from(range.start_offset()).map_err(|_| {
                invalid_fragment(EngineError::invalid_argument(
                    "fragment start offset exceeds platform limits",
                ))
            })?;
            let end = usize::try_from(range.end_offset()).map_err(|_| {
                invalid_fragment(EngineError::invalid_argument(
                    "fragment end offset exceeds platform limits",
                ))
            })?;
            let content = crate::utils::slice_text(text, start, end).map_err(invalid_fragment)?;
            let lines = crate::utils::line_byte_offsets(text);
            let local = crate::utils::text_range_from_offsets(text, &lines, start, end)
                .map_err(invalid_fragment)?;
            let source = match hit.entity.source_range {
                Range::Full => local,
                Range::Text(origin) => {
                    crate::utils::map_text_range(local, origin).map_err(invalid_fragment)?
                }
                Range::Byte(_) => {
                    return Err(invalid_fragment(EngineError::invalid_argument(
                        "fragment content coordinates cannot be mapped to this entity source",
                    )));
                }
            };
            let preview = if matches!(&hit.entity.content, Content::Code(_)) {
                ContentPreview::Code(content.to_owned())
            } else {
                ContentPreview::Text(content.to_owned())
            };
            (preview, Some(Range::Text(source).into()))
        }
        _ => {
            return Err(invalid_fragment(EngineError::invalid_argument(
                "fragments use Full or entity-relative byte ranges for text and code; images require Full",
            )));
        }
    };
    Ok(ContextItemTarget {
        preview,
        content_range: excerpt_range
            .clone()
            .unwrap_or_else(|| hit.entity.source_range.into()),
        content_role: ContextContentRole::Source,
        excerpt_range,
    })
}

fn file_freshness_status(workspace_root: &Path, file: &FileRecord) -> ContextItemStatus {
    if !file.index_status.is_indexed() {
        return ContextItemStatus::PossiblyStale;
    }
    let absolute_path = workspace_root.join(&file.relative_path);
    let Ok(metadata) = fs::metadata(&absolute_path) else {
        return ContextItemStatus::PossiblyStale;
    };
    if !metadata.is_file() {
        return ContextItemStatus::PossiblyStale;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| duration.as_millis().try_into().ok());
    if metadata.len() == file.snapshot.size_bytes
        && modified.is_some()
        && modified == file.snapshot.modified_epoch_ms
    {
        return ContextItemStatus::Fresh;
    }
    if let Some(expected) = &file.snapshot.content_hash
        && fs::read(&absolute_path).is_ok_and(|bytes| sha256_hex(&bytes) == *expected)
    {
        return ContextItemStatus::Fresh;
    }
    ContextItemStatus::PossiblyStale
}

fn rank_as_f64(rank: usize) -> f64 {
    f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
}

fn content_preview(content: &Content) -> ContentPreview {
    match content {
        Content::Text(text) => ContentPreview::Text(text.clone()),
        Content::Code(code) => ContentPreview::Code(code.clone()),
        Content::Image(image) => ContentPreview::Image {
            format: image.format(),
            size_bytes: u64::try_from(image.data().len()).unwrap_or(u64::MAX),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::api::context::{
        ContextOptions,
        options::{ContextRoute, ContextRouteMode},
        result::{
            ContentPreview, ContentRange, ContextContentRole, ContextItem, ContextItemKind,
            ContextItemStatus, ContextQueryGroupMatch, ContextQueryGroupRole,
            ContextSelectionReason, MatchedBy,
        },
    };

    use super::{normalize_context_request, select_and_rank_context_items};

    #[test]
    fn image_inputs_normalize_to_one_vector_route_and_reject_invalid_bytes() {
        use crate::api::context::options::QueryImage;
        use crate::domain::FileFormat;
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut output, image::ImageFormat::Png)
            .expect("encode PNG");
        let data = output.into_inner();
        let options = ContextOptions {
            query_image: Some(QueryImage::Bytes {
                format: FileFormat::Png,
                data: data.clone(),
            }),
            ..Default::default()
        };
        let request = normalize_context_request(&options).expect("image input");
        assert_eq!(request.routes.len(), 1);
        assert_eq!(request.routes[0].mode, ContextRouteMode::Vector);
        assert_eq!(request.image.expect("image").data(), data);
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("arbitrary.extension");
        std::fs::write(&path, &data).expect("write image");
        let from_path = normalize_context_request(&ContextOptions {
            query_image: Some(QueryImage::Path { path }),
            ..Default::default()
        })
        .expect("detect image by bytes");
        assert_eq!(from_path.image.expect("image").data(), data);
        for (format, data) in [(FileFormat::Jpeg, data), (FileFormat::Png, vec![1, 2, 3])] {
            assert!(
                normalize_context_request(&ContextOptions {
                    query_image: Some(QueryImage::Bytes { format, data }),
                    ..Default::default()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn image_results_return_metadata_without_internal_identity_or_payload() {
        use crate::domain::{
            Content, ContentKind, Entity, EntityId, FileFormat, FileId, FileIndexStatus,
            FileRecord, FileSnapshot, ImageContent, Range, SourcePath,
        };
        use crate::pipelines::indexed_search::pipeline::{SearchHit, SearchPlanResult};
        let content =
            Content::Image(ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image"));
        let file_id = FileId::new(1);
        let id = EntityId::new(file_id, &content, Range::Full).expect("id");
        let search = SearchPlanResult {
            routes: vec![],
            timings: vec![],
            hits: vec![SearchHit {
                entity: Entity {
                    id: id.clone(),
                    file_id,
                    content,
                    source_range: Range::Full,
                    metadata: None,
                    fragments: vec![],
                },
                file: FileRecord {
                    id: file_id,
                    relative_path: SourcePath::new("image.png").expect("path"),
                    snapshot: FileSnapshot {
                        size_bytes: 3,
                        modified_epoch_ms: None,
                        content_hash: None,
                    },
                    index_status: FileIndexStatus::NotIndexed,
                },
                evidence: vec![],
                rank: 1,
                score: 0.9,
                matched_by: MatchedBy::Vector,
                trace: None,
            }],
        };
        let group = super::NormalizedContextGroup {
            id: "Q1".into(),
            query: "image:input.png".into(),
            role: ContextQueryGroupRole::Primary,
            routes: vec![],
        };
        let items = super::search_plan_to_context_items(
            &search,
            std::path::Path::new("/workspace"),
            &group,
        )
        .expect("result");
        assert_eq!(items[0].entity_id, id);
        let item = &items[0].item;
        assert_eq!(item.preview.text(), None);
        assert_eq!(item.range, ContentRange::File);
        assert_eq!(item.preview.kind(), ContentKind::Image);
        assert_eq!(
            item.preview,
            ContentPreview::Image {
                format: FileFormat::Png,
                size_bytes: 3
            }
        );
        assert_eq!(item.absolute_path, PathBuf::from("/workspace/image.png"));
        let json = serde_json::to_value(item).expect("JSON");
        assert_eq!(
            json["preview"],
            serde_json::json!({"kind": "image", "value": {"format": "png", "size_bytes": 3}})
        );
        for removed in [
            "image",
            "entity_id",
            "content_kind",
            "content",
            "content_ref",
            "generation",
        ] {
            assert!(json.get(removed).is_none(), "obsolete field {removed}");
        }
    }

    #[test]
    fn best_fragment_preserves_original_content_and_source_range() {
        assert_fragment_content_and_source_range(crate::domain::Content::Text);
    }

    #[test]
    fn code_fragment_preserves_original_content_and_source_range() {
        assert_fragment_content_and_source_range(crate::domain::Content::Code);
    }

    fn assert_fragment_content_and_source_range(
        content_from_text: fn(String) -> crate::domain::Content,
    ) {
        use crate::{
            domain::{
                ByteRange, Entity, EntityFragment, EntityId, FileId, FileIndexStatus, FileRecord,
                FileSnapshot, FragmentId, Range, TextRange,
            },
            pipelines::indexed_search::pipeline::{SearchEvidence, SearchHit},
        };
        let file_id = FileId::new(1);
        let source = "A中😀\r\nβeta\n尾";
        let source_range = Range::Text(
            TextRange::from_coordinates(9, 9 + source.len(), 2, 4, 2, 3).expect("range"),
        );
        let fragment_range = Range::Byte(ByteRange::new(1, 12).expect("ordered byte offsets"));
        let fragment_source_range = Range::Text(
            TextRange::from_coordinates(10, 21, 2, 3, 3, 2).expect("fragment source range"),
        );
        let content = content_from_text(source.into());
        let entity_id = EntityId::new(file_id, &content, source_range).expect("entity id");
        let fragment = EntityFragment {
            id: FragmentId::new(&entity_id, 0),
            range: fragment_range,
        };
        let full = EntityFragment {
            id: FragmentId::new(&entity_id, 1),
            range: Range::Full,
        };
        let mut hit = SearchHit {
            entity: Entity {
                id: entity_id,
                file_id,
                source_range,
                content,
                metadata: None,
                fragments: vec![fragment.clone(), full.clone()],
            },
            file: FileRecord {
                id: file_id,
                relative_path: crate::domain::SourcePath::new("file.txt").expect("source path"),
                snapshot: FileSnapshot {
                    size_bytes: (9 + source.len()) as u64,
                    modified_epoch_ms: None,
                    content_hash: None,
                },
                index_status: FileIndexStatus::NotIndexed,
            },
            evidence: vec![
                SearchEvidence {
                    fragment: fragment.clone(),
                },
                SearchEvidence {
                    fragment: full.clone(),
                },
            ],
            rank: 1,
            score: 1.0,
            matched_by: MatchedBy::Fts,
            trace: None,
        };
        let target = super::context_item_target(&hit).expect("fragment content");
        assert_eq!(target.content_role, ContextContentRole::Source);
        assert_eq!(target.preview.kind(), hit.entity.content.kind());
        assert_eq!(target.preview.text().expect("text preview"), "中😀\r\nβ");
        assert_eq!(target.excerpt_range, Some(fragment_source_range.into()));
        assert_eq!(target.content_range, fragment_source_range.into());
        assert_eq!(hit.entity.content, content_from_text(source.into()));
        let item = context_item_for_hit(&hit);
        assert_eq!(item.range, source_range.into());
        assert_eq!(item.preview.kind(), hit.entity.content.kind());
        assert_eq!(item.excerpt_range, Some(fragment_source_range.into()));
        assert_eq!(item.content_range, fragment_source_range.into());
        assert_eq!(item.preview.text().expect("text preview"), "中😀\r\nβ");
        hit.evidence.reverse();
        let target = super::context_item_target(&hit).expect("full content");
        assert_eq!(target.preview.text().expect("text preview"), source);
        assert_eq!(target.preview.kind(), hit.entity.content.kind());
        assert_eq!(target.content_range, source_range.into());
        assert_eq!(
            target.excerpt_range, None,
            "full fragments do not create an excerpt range"
        );
        hit.evidence[0].fragment.range =
            Range::Byte(ByteRange::new(0, 999).expect("ordered byte offsets"));
        let error = super::context_item_target(&hit)
            .err()
            .expect("invalid fragment must fail");
        assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
    }

    fn context_item_for_hit(hit: &super::SearchHit) -> ContextItem {
        let search = super::SearchPlanResult {
            routes: Vec::new(),
            hits: vec![hit.clone()],
            timings: Vec::new(),
        };
        let request = normalize_context_request(&ContextOptions {
            query: Some("source".into()),
            ..ContextOptions::default()
        })
        .expect("request");
        super::search_plan_to_context_items(&search, std::path::Path::new("."), &request.groups[0])
            .expect("context items")
            .remove(0)
            .item
    }

    #[test]
    fn markdown_eof_fragments_keep_their_own_source_coordinates() {
        use crate::{
            domain::{
                Content, Entity, EntityFragment, EntityId, FileFormat, FileId, FileIndexStatus,
                FileRecord, FileSnapshot, FragmentId, Range, SourcePath,
            },
            extraction::{ChunkOptions, TextSource, extract_for_indexing},
            pipelines::indexed_search::pipeline::{SearchEvidence, SearchHit},
        };
        for newline in ["", "\n", "\r\n"] {
            let source = format!("# Heading\nprefix {} needle{newline}", "x".repeat(5_000));
            let input = TextSource {
                relative_path: SourcePath::new("sample.md").expect("source path"),
                formats: vec![FileFormat::Markdown],
                text: source.clone(),
            };
            let extracted = extract_for_indexing(&input, ChunkOptions::default())
                .expect("default Markdown extraction")
                .remove(0);
            let file_id = FileId::new(1);
            let id = EntityId::new(file_id, &extracted.content, extracted.source_range)
                .expect("entity ID");
            let fragment = EntityFragment {
                id: FragmentId::new(&id, 0),
                range: extracted.fragments.last().expect("EOF fragment").range,
            };
            let Range::Byte(bytes) = fragment.range else {
                panic!("chunked source")
            };
            let hit = SearchHit {
                entity: Entity {
                    id,
                    file_id,
                    source_range: extracted.source_range,
                    content: extracted.content,
                    metadata: extracted.metadata,
                    fragments: vec![fragment.clone()],
                },
                file: FileRecord {
                    id: file_id,
                    relative_path: input.relative_path,
                    snapshot: FileSnapshot {
                        size_bytes: source.len() as u64,
                        modified_epoch_ms: None,
                        content_hash: None,
                    },
                    index_status: FileIndexStatus::NotIndexed,
                },
                evidence: vec![SearchEvidence { fragment }],
                rank: 1,
                score: 1.0,
                matched_by: MatchedBy::Fts,
                trace: None,
            };
            let target = super::context_item_target(&hit).expect("source fragment");
            let start = usize::try_from(bytes.start_offset()).expect("start");
            let end = usize::try_from(bytes.end_offset()).expect("end");
            assert!(start > "# Heading\nprefix ".len());
            assert_eq!(
                target.preview.text().expect("text preview"),
                &source[start..end]
            );
            assert_eq!(target.content_range.start_line(), Some(2));
            assert_eq!(target.content_range.last_line(), Some(2));
            assert_eq!(target.excerpt_range, Some(target.content_range));
            assert_eq!(
                target.preview.text().expect("text preview").ends_with('\n'),
                !newline.is_empty()
            );
            assert!(matches!(hit.entity.content, Content::Text(_)));
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "The complete fixture keeps relocation, subdirectory lookup, and freshness checks in one scenario"
    )]
    fn resolves_context_paths_and_freshness_after_workspace_relocation() {
        use crate::{
            domain::{
                Content, Entity, EntityId, FileId, FileIndexStatus, FileRecord, FileSnapshot,
                IndexDescriptor, IndexState, Range, TextRange, Workspace,
                model::{EmbeddingMetric, EmbeddingModelInfo},
            },
            pipelines::indexed_search::pipeline::{SearchHit, SearchPlanResult},
            utils::sha256_hex,
        };

        let directory = tempfile::tempdir().expect("temporary directory");
        let original_root = directory.path().join("original");
        let moved_root = directory.path().join("moved");
        std::fs::create_dir_all(original_root.join("src")).expect("source directory");
        let content = "source contents";
        std::fs::write(original_root.join("src/file.txt"), content).expect("source file");
        let source = FileRecord {
            id: FileId::new(1),
            relative_path: crate::domain::SourcePath::new("src/file.txt").expect("source path"),
            snapshot: FileSnapshot {
                size_bytes: content.len() as u64,
                modified_epoch_ms: None,
                content_hash: Some(sha256_hex(content.as_bytes())),
            },
            index_status: FileIndexStatus::NotIndexed,
        };
        let file = FileRecord {
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 0,
                entity_count: 1,
            },
            ..source.clone()
        };
        let source_range = Range::Text(
            TextRange::from_coordinates(0, content.len(), 1, 1, 0, content.len()).expect("range"),
        );
        let content = Content::Text(content.to_owned());
        let search = SearchPlanResult {
            routes: Vec::new(),
            hits: vec![SearchHit {
                entity: Entity {
                    id: EntityId::new(source.id, &content, source_range).expect("entity id"),
                    file_id: source.id,
                    source_range,
                    content,
                    metadata: None,
                    fragments: Vec::new(),
                },
                file: file.clone(),
                evidence: Vec::new(),
                rank: 1,
                score: 1.0,
                matched_by: MatchedBy::Fts,
                trace: None,
            }],
            timings: Vec::new(),
        };
        let request = normalize_context_request(&ContextOptions {
            query: Some("source".to_owned()),
            ..ContextOptions::default()
        })
        .expect("request");
        let mut workspace = Workspace {
            name: "workspace".to_owned(),
            root: original_root.clone(),
            scan: crate::domain::ScanRules::default(),
            index: IndexState::Enabled(Box::new(
                IndexDescriptor::new(
                    EmbeddingModelInfo::new(
                        crate::domain::model::ModelInfo::new(
                            "local",
                            "fixture",
                            [
                                crate::domain::ContentKind::Text,
                                crate::domain::ContentKind::Code,
                            ],
                            32,
                        )
                        .expect("fixture model identity"),
                        2,
                        EmbeddingMetric::Cosine,
                        None,
                        None,
                    )
                    .expect("embedding model"),
                    std::collections::BTreeMap::new(),
                    crate::domain::FTS_CONFIG,
                )
                .expect("index descriptor"),
            )),
            created_epoch_ms: 0,
            updated_epoch_ms: 0,
        };

        for root in [&original_root, &moved_root] {
            if root == &moved_root {
                std::fs::rename(&original_root, &moved_root).expect("move workspace");
                workspace.root = moved_root.clone();
            }
            // The request may start in a subdirectory; source paths retain the workspace base.
            let result = super::build_table_result(
                &workspace.root,
                &request,
                &request.groups,
                vec![search.clone()],
            )
            .expect("context result");
            assert_eq!(
                result.items[0].item.absolute_path,
                root.join("src/file.txt")
            );
            assert_eq!(
                result.items[0].item.relative_path,
                PathBuf::from("src/file.txt")
            );
            assert_eq!(result.items[0].item.status, ContextItemStatus::Fresh);
        }
        for status in [FileIndexStatus::NotIndexed, FileIndexStatus::Deleting] {
            let mut interrupted = search.clone();
            interrupted.hits[0].file.index_status = status;
            let result = super::build_table_result(
                &workspace.root,
                &request,
                &request.groups,
                vec![interrupted],
            )
            .expect("read remaining indexed content");
            assert_eq!(result.items.len(), 1);
            assert_eq!(
                result.items[0].item.status,
                ContextItemStatus::PossiblyStale
            );
        }
        assert_eq!(
            super::file_freshness_status(&original_root, &file),
            ContextItemStatus::PossiblyStale
        );
        std::fs::write(moved_root.join("src/file.txt"), "changed contents")
            .expect("change moved source");
        assert_eq!(
            super::file_freshness_status(&moved_root, &file),
            ContextItemStatus::PossiblyStale
        );
        let mut future_indexed = file;
        future_indexed.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: u64::MAX,
            entity_count: 1,
        };
        assert_eq!(
            super::file_freshness_status(&moved_root, &future_indexed),
            ContextItemStatus::PossiblyStale,
            "an index clock ahead of file mtime does not prove the source is unchanged"
        );
    }

    #[test]
    fn normalizes_primary_and_supplemental_query_groups_like_main() {
        let request = normalize_context_request(&ContextOptions {
            query: Some(" alpha ".to_owned()),
            queries: vec![" beta ".to_owned()],
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: " gamma ".to_owned(),
            }],
            ..ContextOptions::default()
        })
        .expect("normalized context request");

        assert_eq!(request.display_query, "alpha | beta");
        assert_eq!(request.groups.len(), 3);
        assert_eq!(request.groups[0].id, "Q1");
        assert_eq!(request.groups[0].role, ContextQueryGroupRole::Primary);
        assert_eq!(request.groups[0].routes.len(), 2);
        assert_eq!(request.groups[2].id, "Q3");
        assert_eq!(request.groups[2].role, ContextQueryGroupRole::Supplemental);
        assert_eq!(request.routes.len(), 5);
    }

    #[test]
    fn deduplicates_cross_group_hits_and_preserves_primary_coverage() {
        let items = vec![
            item("shared", group("Q1", 1, MatchedBy::Fts)),
            item("q1-only", group("Q1", 2, MatchedBy::Fts)),
            item("shared", group("Q2", 1, MatchedBy::Vector)),
            item("q2-only", group("Q2", 2, MatchedBy::Vector)),
        ];

        let selected = select_and_rank_context_items(items, &["Q1".to_owned(), "Q2".to_owned()]);

        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].entity_id.as_str(), "shared");
        assert_eq!(selected[0].item.query_groups.len(), 2);
        assert_eq!(selected[0].item.matched_by, MatchedBy::FtsAndVector);
        assert_eq!(
            selected[0].item.selection_reason,
            Some(ContextSelectionReason::Coverage)
        );
        assert_eq!(selected[0].item.coverage_group.as_deref(), Some("Q1"));
        assert_eq!(selected[1].entity_id.as_str(), "q2-only");
        assert_eq!(selected[1].item.coverage_group.as_deref(), Some("Q2"));
        assert_eq!(
            selected
                .iter()
                .map(|item| item.item.rank)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    fn group(id: &str, rank: usize, matched_by: MatchedBy) -> ContextQueryGroupMatch {
        ContextQueryGroupMatch {
            id: id.to_owned(),
            query: id.to_owned(),
            role: ContextQueryGroupRole::Primary,
            rank,
            matched_by,
        }
    }

    fn item(id: &str, query_group: ContextQueryGroupMatch) -> super::IndexedContextItem {
        super::IndexedContextItem {
            entity_id: crate::domain::EntityId::from_string(id.to_owned()),
            item: ContextItem {
                kind: ContextItemKind::IndexedEntity,
                rank: query_group.rank,
                absolute_path: PathBuf::from("/workspace/source.rs"),
                relative_path: PathBuf::from("source.rs"),
                range: ContentRange::Text {
                    start_line: 1,
                    end_line: 1,
                    start_byte_offset: 0,
                    end_byte_offset: 1,
                    start_byte_column: 0,
                    end_byte_column: 1,
                },
                content_range: ContentRange::Text {
                    start_line: 1,
                    end_line: 1,
                    start_byte_offset: 0,
                    end_byte_offset: 1,
                    start_byte_column: 0,
                    end_byte_column: 1,
                },
                excerpt_range: None,
                preview: ContentPreview::Text(id.to_owned()),
                outline: None,
                content_role: Some(ContextContentRole::Source),
                status: ContextItemStatus::Fresh,
                score: Some(1.0),
                matched_by: query_group.matched_by,
                metadata: None,
                container: None,
                trace: None,
                query_groups: vec![query_group],
                selection_reason: None,
                coverage_group: None,
            },
        }
    }
}
