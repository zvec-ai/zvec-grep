use super::storage::SearchStorage;
use crate::pipelines::freshness::file_freshness_status;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};

use crate::{
    EngineError,
    api::context::{
        ContextOptions, ContextResult,
        options::{ContextRoute, ContextRouteMode},
        result::{
            ContentRange, ContextContentRole, ContextCoverage, ContextDiagnostics,
            ContextGroupResult, ContextItem, ContextItemKind, ContextQueryGroupMatch,
            ContextQueryGroupRole, ContextSelectionReason, ContextSource, ContextWorkspaceIndex,
            EmptyReason, IndexDiagnostics, IndexQueryGroupDiagnostics, IndexResultGroup,
            IndexRouteDiagnostics, MatchedBy,
        },
    },
    domain::{Content, ContentIndex, ContentKind, Range, Workspace},
};

use super::pipeline::{
    SearchEmbeddingRuntime, SearchHit, SearchPlan, SearchPlanResult, search_workspace_index,
};

const DEFAULT_CONTEXT_LIMIT: usize = 10;
const DEFAULT_CONTEXT_TOTAL_LIMIT: usize = 30;
const DEFAULT_CONTEXT_PRIORITY_LIMIT: usize = 6;
const CONTEXT_GROUP_RRF_K: f64 = 60.0;

#[derive(Debug)]
pub(crate) struct NormalizedContextRequest {
    display_query: String,
    groups: Vec<NormalizedContextGroup>,
}

impl NormalizedContextRequest {
    pub(crate) fn uses_vectors(&self) -> bool {
        self.groups.iter().any(NormalizedContextGroup::uses_vectors)
    }
}

#[derive(Debug)]
struct NormalizedContextGroup {
    id: String,
    query: String,
    role: ContextQueryGroupRole,
    routes: Vec<ContextRoute>,
}

impl NormalizedContextGroup {
    fn uses_vectors(&self) -> bool {
        self.routes
            .iter()
            .any(|route| route.mode == ContextRouteMode::Vector)
    }
}

pub(crate) fn normalize_context_request(
    options: &ContextOptions,
) -> Result<NormalizedContextRequest, EngineError> {
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
            .into_iter()
            .enumerate()
            .map(|(index, route)| NormalizedContextGroup {
                id: format!("Q{}", primary_queries.len() + index + 1),
                query: route.query.clone(),
                role: ContextQueryGroupRole::Supplemental,
                routes: vec![route],
            }),
    );
    let display_query = if primary_queries.is_empty() {
        groups
            .iter()
            .map(|group| group.query.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    } else {
        primary_queries.join(" | ")
    };

    if options.fuse {
        groups = vec![NormalizedContextGroup {
            id: "Q1".to_owned(),
            query: display_query.clone(),
            role: if primary_queries.is_empty() {
                ContextQueryGroupRole::Supplemental
            } else {
                ContextQueryGroupRole::Primary
            },
            routes: groups.into_iter().flat_map(|group| group.routes).collect(),
        }];
    }

    Ok(NormalizedContextRequest {
        display_query,
        groups,
    })
}

pub(crate) async fn context_from_index(
    workspace: &Workspace,
    storage: &dyn SearchStorage,
    targets: &[ContentIndex],
    populated_kinds: &BTreeSet<ContentKind>,
    embedding_models: &[&dyn SearchEmbeddingRuntime],
    options: &ContextOptions,
    request: &NormalizedContextRequest,
) -> Result<ContextResult, EngineError> {
    let groups = &request.groups;
    let per_query = context_group_limit(options.limit, groups.len());
    let mut bundles = Vec::with_capacity(groups.len());
    for query in groups {
        let vector = query.uses_vectors();
        let mut searches = Vec::new();
        for pool in ranking_groups(targets, vector) {
            let kinds: Vec<_> = pool
                .kinds
                .iter()
                .copied()
                .filter(|kind| populated_kinds.contains(kind))
                .collect();
            let runtime = if vector && !kinds.is_empty() {
                Some(
                    embedding_models
                        .iter()
                        .copied()
                        .find(|model| model.info().model().reference() == pool.models[0])
                        .ok_or_else(|| {
                            EngineError::unsupported(format!(
                                "vector search requires model {}",
                                pool.models[0]
                            ))
                        })?,
                )
            } else {
                None
            };
            let search = search_workspace_index(
                workspace.source_root(),
                SearchPlan {
                    kinds,
                    routes: query.routes.clone(),
                    limit: Some(per_query),
                    trace: options.trace,
                    prefer_symbol: options.prefer_symbol,
                    filter: options.filter.clone(),
                },
                storage,
                runtime,
            )
            .await?;
            searches.push((pool, search));
        }
        bundles.push(searches);
    }
    let summary_groups = ranking_groups(targets, request.uses_vectors());
    build_context_result(
        workspace.source_root(),
        workspace,
        request,
        bundles,
        summary_groups,
        per_query,
    )
}

/// Every collection embeds text queries with `EmbeddingPurpose::Query`. The workspace
/// stores one runtime configuration per model reference and rejects conflicting
/// schemas for that reference, so shared references also share a score space.
fn ranking_groups(targets: &[ContentIndex], vector: bool) -> Vec<IndexResultGroup> {
    let mut groups: Vec<IndexResultGroup> = Vec::new();
    for target in targets {
        let model = target.embedding.model().reference();
        if let Some(group) = groups
            .iter_mut()
            .find(|g| !vector || g.models.contains(&model))
        {
            group.kinds.push(target.kind);
            if !group.models.contains(&model) {
                group.models.push(model);
            }
        } else {
            groups.push(IndexResultGroup {
                kinds: vec![target.kind],
                models: vec![model],
                item_start: 0,
                item_count: 0,
            });
        }
    }
    groups
}

fn build_context_result(
    root: &Path,
    workspace: &Workspace,
    request: &NormalizedContextRequest,
    bundles: Vec<Vec<(IndexResultGroup, SearchPlanResult)>>,
    mut result_groups: Vec<IndexResultGroup>,
    per_query_limit: usize,
) -> Result<ContextResult, EngineError> {
    let groups = &request.groups;
    let coverage_groups = groups
        .iter()
        .filter(|group| group.role == ContextQueryGroupRole::Primary)
        .map(|group| group.id.clone())
        .collect::<Vec<_>>();
    let mut group_results = Vec::with_capacity(groups.len());
    let mut timings = Vec::new();
    let mut routes = Vec::new();
    let mut stage_traces = Vec::new();
    let mut retained = HashMap::<_, Vec<_>>::new();
    for (query, searches) in groups.iter().zip(bundles) {
        let mut result = ContextGroupResult {
            id: query.id.clone(),
            query: query.query.clone(),
            role: query.role,
            result_groups: Vec::with_capacity(searches.len()),
            items: Vec::new(),
        };
        let counts: Vec<_> = searches
            .iter()
            .map(|(_, search)| search.hits.len())
            .collect();
        let allocations = fair_allocations(&counts, per_query_limit);
        for ((mut metadata, mut search), allocation) in searches.into_iter().zip(allocations) {
            // Query routes describe the input and are identical across score spaces.
            if result.result_groups.is_empty() {
                routes.extend(search.routes.drain(..).map(|route| IndexRouteDiagnostics {
                    id: route.id,
                    mode: route.mode,
                    query: route.query,
                }));
            }
            search.truncate(allocation);
            let local = search_plan_to_context_items(&search, workspace.source_root(), query)?;
            for (hit, item) in search.hits.iter().zip(&local) {
                retained
                    .entry(hit.entity.content().kind())
                    .or_default()
                    .push(item.clone());
            }
            timings.append(&mut search.timings);
            if let Some(mut trace) = search.stage_trace {
                trace.group_id.clone_from(&query.id);
                trace.kinds.clone_from(&metadata.kinds);
                stage_traces.push(trace);
            }
            metadata.item_start = result.items.len();
            metadata.item_count = local.len();
            result.result_groups.push(metadata);
            result.items.extend(local);
        }
        group_results.push(result);
    }
    let items = rank_combined_results(&mut result_groups, retained, &coverage_groups);
    let hits_returned = items.len();
    Ok(ContextResult {
        freshness: None,
        background_refresh: None,
        indexing: None,
        query: request.display_query.clone(),
        root: root.to_path_buf(),
        source: ContextSource::Index,
        coverage: ContextCoverage::RankedSample,
        workspace_index: Some(ContextWorkspaceIndex {
            root: workspace.source_root().to_path_buf(),
            path: workspace.layout().storage_dir().to_path_buf(),
        }),
        items,
        stage_traces,
        group_results,
        diagnostics: ContextDiagnostics {
            empty_reason: (hits_returned == 0).then_some(EmptyReason::NoMatches),
            index: Some(IndexDiagnostics {
                hits_returned,
                result_groups,
                query_groups: groups
                    .iter()
                    .map(|group| IndexQueryGroupDiagnostics {
                        id: group.id.clone(),
                        query: group.query.clone(),
                        role: group.role,
                    })
                    .collect(),
                routes,
            }),
            rg: None,
            structure: None,
            timings,
        },
    })
}

/// Deduplicate across queries and rank only within each compatible score space.
fn rank_combined_results(
    groups: &mut [IndexResultGroup],
    mut retained: HashMap<crate::domain::ContentKind, Vec<ContextItem>>,
    coverage_groups: &[String],
) -> Vec<ContextItem> {
    let mut items = Vec::new();
    for group in groups {
        let local = group
            .kinds
            .iter()
            .filter_map(|kind| retained.remove(kind))
            .flatten()
            .collect();
        let ranked = select_and_rank_context_items(local, coverage_groups);
        group.item_start = items.len();
        group.item_count = ranked.len();
        items.extend(ranked);
    }
    items
}

/// Distribute one total limit in stable table order, lending unused slots to
/// groups with more candidates. Scores from different models never meet.
fn fair_allocations(counts: &[usize], mut remaining: usize) -> Vec<usize> {
    let mut allocations = vec![0; counts.len()];
    while remaining > 0 {
        let before = remaining;
        for (&available, allocated) in counts.iter().zip(&mut allocations) {
            if remaining > 0 && *allocated < available {
                *allocated += 1;
                remaining -= 1;
            }
        }
        if remaining == before {
            break;
        }
    }
    allocations
}

fn context_group_limit(limit: Option<usize>, group_count: usize) -> usize {
    limit.unwrap_or_else(|| {
        let group_count = group_count.max(1);
        if group_count <= 3 {
            DEFAULT_CONTEXT_LIMIT
        } else {
            DEFAULT_CONTEXT_TOTAL_LIMIT.div_ceil(group_count).max(1)
        }
    })
}

fn search_plan_to_context_items(
    result: &SearchPlanResult,
    workspace_root: &Path,
    group: &NormalizedContextGroup,
) -> Result<Vec<ContextItem>, EngineError> {
    result
        .hits
        .iter()
        .map(|hit| {
            let target = context_item_target(hit)?;
            Ok(ContextItem {
                kind: ContextItemKind::IndexedEntity,
                rank: hit.rank,
                absolute_path: workspace_root.join(hit.file.relative_path()),
                relative_path: hit.file.relative_path().to_path_buf(),
                range: ContentRange::from_entity(&hit.entity),
                excerpt_range: target.excerpt_range,
                content_range: target.content_range,
                match_ranges: Vec::new(),
                content: target.content,
                outline: crate::extraction::code_outline(
                    hit.file.relative_path().as_ref(),
                    hit.entity.content(),
                    hit.entity.metadata(),
                ),
                content_windows: Vec::new(),
                pdf_boxes: crate::api::context::result::PdfBox::from_entity(&hit.entity),
                content_role: Some(target.content_role),
                status: file_freshness_status(workspace_root, &hit.file),
                score: Some(hit.score),
                matched_by: hit.matched_by,
                metadata: hit.entity.metadata().cloned(),
                entity_id: Some(hit.entity.id().as_str().to_owned()),
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
            })
        })
        .collect()
}

fn select_and_rank_context_items(
    items: Vec<ContextItem>,
    coverage_group_ids: &[String],
) -> Vec<ContextItem> {
    let mut deduped: Vec<ContextItem> = Vec::new();
    for item in items {
        let key = context_item_dedupe_key(&item);
        if let Some(existing) = deduped
            .iter_mut()
            .find(|candidate| context_item_dedupe_key(candidate) == key)
        {
            merge_query_group_matches(&mut existing.query_groups, &item.query_groups);
            existing.matched_by = merged_matched_by(&existing.query_groups);
        } else {
            deduped.push(item);
        }
    }
    let mut globally_ranked = deduped.clone();
    globally_ranked.sort_by(compare_global_rank);
    let mut selected = HashSet::<String>::new();
    let mut prioritized = Vec::new();

    for group_id in coverage_group_ids {
        if prioritized.len() >= DEFAULT_CONTEXT_PRIORITY_LIMIT {
            break;
        }
        let mut candidates = deduped
            .iter()
            .filter(|item| !selected.contains(&context_item_dedupe_key(item)))
            .filter(|item| query_group_match(item, group_id).is_some())
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| compare_group_rank(left, right, group_id));
        if let Some(mut candidate) = candidates.into_iter().next() {
            selected.insert(context_item_dedupe_key(&candidate));
            candidate.selection_reason = Some(ContextSelectionReason::Coverage);
            candidate.coverage_group = Some(group_id.clone());
            prioritized.push(candidate);
        }
    }
    for mut candidate in globally_ranked.iter().cloned() {
        if prioritized.len() >= DEFAULT_CONTEXT_PRIORITY_LIMIT {
            break;
        }
        let key = context_item_dedupe_key(&candidate);
        if selected.insert(key) {
            candidate.selection_reason = Some(ContextSelectionReason::GlobalFill);
            candidate.coverage_group = None;
            prioritized.push(candidate);
        }
    }
    prioritized.extend(
        globally_ranked
            .into_iter()
            .filter(|item| !selected.contains(&context_item_dedupe_key(item))),
    );
    for (index, item) in prioritized.iter_mut().enumerate() {
        item.rank = index + 1;
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

fn compare_global_rank(left: &ContextItem, right: &ContextItem) -> std::cmp::Ordering {
    global_rrf_score(right)
        .total_cmp(&global_rrf_score(left))
        .then_with(|| best_group_rank(left).cmp(&best_group_rank(right)))
        .then_with(|| first_group_number(left).cmp(&first_group_number(right)))
        .then_with(|| context_item_dedupe_key(left).cmp(&context_item_dedupe_key(right)))
}

fn global_rrf_score(item: &ContextItem) -> f64 {
    item.query_groups
        .iter()
        .map(|group| 1.0 / (CONTEXT_GROUP_RRF_K + rank_as_f64(group.rank)))
        .sum()
}

fn compare_group_rank(
    left: &ContextItem,
    right: &ContextItem,
    group_id: &str,
) -> std::cmp::Ordering {
    let left_rank = query_group_match(left, group_id).map_or(usize::MAX, |group| group.rank);
    let right_rank = query_group_match(right, group_id).map_or(usize::MAX, |group| group.rank);
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

fn context_item_dedupe_key(item: &ContextItem) -> String {
    item.entity_id.as_ref().map_or_else(
        || format!("range:{}:{:?}", item.absolute_path.display(), item.range),
        |id| format!("entity:{id}"),
    )
}

struct ContextItemTarget {
    content: String,
    content_range: ContentRange,
    content_role: ContextContentRole,
    excerpt_range: Option<ContentRange>,
}

fn context_item_target(hit: &SearchHit) -> Result<ContextItemTarget, EngineError> {
    let Some(evidence) = hit.evidence.first() else {
        return Ok(ContextItemTarget {
            content: content_to_text(hit.entity.content()),
            content_range: ContentRange::from_entity(&hit.entity),
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
    let (content, content_range, excerpt_range) = match (
        fragment.range.clone(),
        hit.entity.content(),
    ) {
        (Range::Full, content) => (
            content_to_text(content),
            ContentRange::from_entity(&hit.entity),
            None,
        ),
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
            let excerpt =
                ContentRange::project_text(&hit.entity, start..end).map_err(invalid_fragment)?;
            // Fragments locate evidence; full previews retain the complete entity.
            let whole =
                ContentRange::project_text(&hit.entity, 0..text.len()).map_err(invalid_fragment)?;
            (text.clone(), whole, Some(excerpt))
        }
        _ => {
            return Err(invalid_fragment(EngineError::invalid_argument(
                "fragments use Full or entity-relative byte ranges for text and code; images require Full",
            )));
        }
    };
    Ok(ContextItemTarget {
        content,
        content_range,
        content_role: ContextContentRole::Source,
        excerpt_range,
    })
}

fn rank_as_f64(rank: usize) -> f64 {
    f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
}

fn content_to_text(content: &Content) -> String {
    match content {
        Content::Text(text) | Content::Code(text) => text.clone(),
        Content::Image(image) => format!(
            "[image:{} bytes={}]",
            image.format().as_str(),
            image.data().len()
        ),
    }
}

#[cfg(test)]
mod fragment_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::api::context::{
        ContextOptions,
        options::{ContextRoute, ContextRouteMode},
        result::{
            ContentRange, ContextContentRole, ContextItem, ContextItemKind, ContextItemStatus,
            ContextQueryGroupMatch, ContextQueryGroupRole, ContextSelectionReason, MatchedBy,
        },
    };

    use super::{fair_allocations, normalize_context_request, select_and_rank_context_items};

    #[test]
    fn lends_unused_slots_in_stable_group_order() {
        assert_eq!(fair_allocations(&[0, 3, 8], 8), [0, 3, 5]);
        assert_eq!(fair_allocations(&[5, 5], 3), [2, 1]);
        assert_eq!(fair_allocations(&[0, 1], 10), [0, 1]);
    }

    #[test]
    fn pdf_context_returns_full_entity_and_maps_fragment_to_its_physical_page() {
        use crate::{
            domain::{
                ByteRange, Content, Entity, EntityFragment, EntityId, FileId, FileIndexStatus,
                FileRecord, FileSnapshot, FragmentId, PdfBlock, PdfSourceMap, PdfSourcePart, Range,
                SourcePath,
            },
            pipelines::indexed_search::pipeline::{SearchEvidence, SearchHit},
        };
        let text = "# Recovery\n\n等待 starts here\ncontinuation on the next physical page";
        let split = text.find("continuation").expect("split");
        let content = Content::Text(text.into());
        let map = PdfSourceMap {
            markdown_start: 100,
            text_len: text.len(),
            parts: vec![
                PdfSourcePart {
                    range: 0..split,
                    block: PdfBlock {
                        page: 2,
                        id: 1,
                        page_width: 612.0,
                        page_height: 792.0,
                        bbox: None,
                    },
                },
                PdfSourcePart {
                    range: split..text.len(),
                    block: PdfBlock {
                        page: 4,
                        id: 2,
                        page_width: 612.0,
                        page_height: 792.0,
                        bbox: None,
                    },
                },
            ],
        };
        let source_range = Range::Pdf(map.locate(0..map.text_len).expect("location").into());
        let file_id = FileId::new(1);
        let id = EntityId::new(file_id, &content, &source_range).expect("ID");
        let fragment = EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Byte(
                ByteRange::new(
                    u64::try_from(split).expect("start"),
                    u64::try_from(text.len()).expect("end"),
                )
                .expect("range"),
            ),
        };
        let hit = SearchHit {
            entity: Entity::new(
                id,
                file_id,
                source_range,
                content,
                None,
                vec![fragment.clone()],
            )
            .expect("entity")
            .with_source_map(Some(std::sync::Arc::new(map)))
            .expect("source map"),
            file: FileRecord::new(
                file_id,
                SourcePath::new("manual.pdf").expect("path"),
                FileSnapshot {
                    size_bytes: 42,
                    modified_epoch_ms: None,
                    content_hash: None,
                },
                FileIndexStatus::NotIndexed,
            )
            .expect("file"),
            evidence: vec![SearchEvidence { fragment }],
            rank: 1,
            score: 1.0,
            matched_by: MatchedBy::Fts,
            trace: None,
        };
        let target = super::context_item_target(&hit).expect("context");
        assert_eq!(target.content, text);
        assert_eq!(
            target.content_range,
            ContentRange::Pdf {
                pages: vec![2, 4],
                start_byte_offset: 100,
                end_byte_offset: 100 + text.len(),
            }
        );
        assert_eq!(
            target.excerpt_range,
            Some(ContentRange::Pdf {
                pages: vec![4],
                start_byte_offset: 100 + split,
                end_byte_offset: 100 + text.len(),
            })
        );
        assert_eq!(target.content_range.start_line(), None);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "End-to-end assertions for fragment and entity boundaries"
    )]
    fn best_fragment_preserves_evidence_and_returns_the_complete_entity() {
        use crate::{
            domain::{
                ByteRange, Content, Entity, EntityFragment, EntityId, FileId, FileIndexStatus,
                FileRecord, FileSnapshot, FragmentId, Range, TextRange,
            },
            pipelines::indexed_search::pipeline::{SearchEvidence, SearchHit, SearchPlanResult},
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
        let content = Content::Text(source.into());
        let entity_id = EntityId::new(file_id, &content, &source_range).expect("entity id");
        let fragment = EntityFragment {
            id: FragmentId::new(&entity_id, 0),
            range: fragment_range,
        };
        let full = EntityFragment {
            id: FragmentId::new(&entity_id, 1),
            range: Range::Full,
        };
        let mut hit = SearchHit {
            entity: Entity::new(
                entity_id,
                file_id,
                source_range.clone(),
                content,
                None,
                vec![fragment.clone(), full.clone()],
            )
            .expect("entity"),
            file: FileRecord::new(
                file_id,
                crate::domain::SourcePath::new("file.txt").expect("source path"),
                FileSnapshot {
                    size_bytes: (9 + source.len()) as u64,
                    modified_epoch_ms: None,
                    content_hash: None,
                },
                FileIndexStatus::NotIndexed,
            )
            .expect("file"),
            evidence: vec![
                SearchEvidence { fragment },
                SearchEvidence { fragment: full },
            ],
            rank: 1,
            score: 1.0,
            matched_by: MatchedBy::Fts,
            trace: None,
        };
        let target = super::context_item_target(&hit).expect("fragment content");
        assert_eq!(target.content_role, ContextContentRole::Source);
        assert_eq!(target.content, source);
        assert_eq!(
            target.excerpt_range,
            Some(fragment_source_range.clone().into())
        );
        assert_eq!(target.content_range, source_range.clone().into());
        assert_eq!(hit.entity.content(), &Content::Text(source.into()));
        let search = SearchPlanResult {
            routes: Vec::new(),
            hits: vec![hit.clone()],
            timings: Vec::new(),
            stage_trace: None,
        };
        let request = normalize_context_request(&ContextOptions {
            query: Some("source".into()),
            ..ContextOptions::default()
        })
        .expect("request");
        let items = super::search_plan_to_context_items(
            &search,
            std::path::Path::new("."),
            &request.groups[0],
        )
        .expect("context items");
        assert_eq!(items[0].range, source_range.clone().into());
        assert_eq!(
            items[0].excerpt_range,
            Some(fragment_source_range.clone().into())
        );
        assert_eq!(items[0].content_range, source_range.clone().into());
        assert_eq!(items[0].content, source);
        hit.evidence.reverse();
        let target = super::context_item_target(&hit).expect("full content");
        assert_eq!(target.content, source);
        assert_eq!(target.content_range, source_range.clone().into());
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
                .fragments
                .remove(0);
            let file_id = FileId::new(1);
            let id = EntityId::new(file_id, &extracted.content, &extracted.source_range)
                .expect("entity ID");
            let fragment = EntityFragment {
                id: FragmentId::new(&id, 0),
                range: extracted
                    .fragments
                    .last()
                    .expect("EOF fragment")
                    .range
                    .clone(),
            };
            let Range::Byte(bytes) = fragment.range else {
                panic!("chunked source")
            };
            let hit = SearchHit {
                entity: Entity::new(
                    id,
                    file_id,
                    extracted.source_range,
                    extracted.content,
                    extracted.metadata,
                    vec![fragment.clone()],
                )
                .expect("entity"),
                file: FileRecord::new(
                    file_id,
                    input.relative_path,
                    FileSnapshot {
                        size_bytes: source.len() as u64,
                        modified_epoch_ms: None,
                        content_hash: None,
                    },
                    FileIndexStatus::NotIndexed,
                )
                .expect("file"),
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
            assert_eq!(target.content, source);
            assert_eq!(target.content_range.start_line(), Some(1));
            assert_eq!(target.content_range.last_line(), Some(2));
            let matched = crate::utils::text_range_from_offsets(
                &source,
                &crate::utils::line_byte_offsets(&source),
                start,
                end,
            )
            .expect("original fragment range");
            assert_eq!(target.excerpt_range, Some(Range::Text(matched).into()));
            assert_eq!(target.content.ends_with('\n'), !newline.is_empty());
            assert!(matches!(hit.entity.content(), Content::Text(_)));
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
                Content, Entity, EntityFragment, EntityId, FileId, FileIndexStatus, FileRecord,
                FileSnapshot, FragmentId, IndexDescriptor, IndexState, Range, TextRange, Workspace,
                WorkspaceLayout,
                model::{EmbeddingMetric, EmbeddingModelInfo},
            },
            pipelines::indexed_search::pipeline::{SearchHit, SearchPlanResult},
            utils::sha256_hex,
        };

        let directory = tempfile::tempdir().expect("temporary directory");
        let base = directory.path().canonicalize().expect("source parent");
        let original_root = base.join("original");
        let moved_root = base.join("moved");
        std::fs::create_dir_all(original_root.join("src")).expect("source directory");
        let content = "source contents";
        std::fs::write(original_root.join("src/file.txt"), content).expect("source file");
        let source = FileRecord::new(
            FileId::new(1),
            crate::domain::SourcePath::new("src/file.txt").expect("source path"),
            FileSnapshot {
                size_bytes: content.len() as u64,
                modified_epoch_ms: None,
                content_hash: Some(sha256_hex(content.as_bytes())),
            },
            FileIndexStatus::NotIndexed,
        )
        .expect("file");
        let mut file = source.clone();
        file.set_index_status(FileIndexStatus::Indexed {
            indexed_epoch_ms: 0,
            entity_count: 1,
        })
        .expect("indexed file");
        let source_range = Range::Text(
            TextRange::from_coordinates(0, content.len(), 1, 1, 0, content.len()).expect("range"),
        );
        let content = Content::Text(content.to_owned());
        let id = EntityId::new(source.id(), &content, &source_range).expect("entity id");
        let fragment = EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        };
        let search = SearchPlanResult {
            routes: Vec::new(),
            stage_trace: None,
            hits: vec![SearchHit {
                entity: Entity::new(id, source.id(), source_range, content, None, vec![fragment])
                    .expect("entity"),
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
        let index = IndexState::Enabled(Box::new(
            IndexDescriptor::new(
                EmbeddingModelInfo::new(
                    crate::domain::model::ModelInfo::new(
                        "local".to_owned(),
                        "fixture".to_owned(),
                        [
                            crate::domain::ContentKind::Text,
                            crate::domain::ContentKind::Code,
                            crate::domain::ContentKind::Image,
                        ],
                        32,
                    )
                    .expect("valid model metadata"),
                    2,
                    EmbeddingMetric::Cosine,
                    None,
                    None,
                )
                .expect("valid embedding metadata"),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                false,
                crate::domain::FTS_CONFIG,
            )
            .expect("index descriptor"),
        ));

        let generation = uuid::Uuid::new_v4().to_string();
        for root in [&original_root, &moved_root] {
            if root == &moved_root {
                std::fs::rename(&original_root, &moved_root).expect("move workspace");
            }
            let workspace = Workspace::new(
                root,
                WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout"),
                crate::domain::ScanRules::default(),
                crate::workspace::CURRENT_INDEX_VERSION,
                Some(generation.clone()),
                0,
                0,
                index.clone(),
            )
            .expect("workspace");
            // The request may start in a subdirectory; source paths retain the workspace base.
            let requested_root = root.join("src");
            let result_group = super::IndexResultGroup {
                kinds: vec![crate::domain::ContentKind::Text],
                models: vec!["local/model".to_owned()],
                item_start: 0,
                item_count: 0,
            };
            let result = super::build_context_result(
                &requested_root,
                &workspace,
                &request,
                vec![vec![(result_group.clone(), search.clone())]],
                vec![result_group],
                10,
            )
            .expect("context result");
            assert_eq!(result.root, requested_root);
            let workspace_index = result.workspace_index.as_ref().expect("workspace index");
            assert_eq!(workspace_index.root, *root);
            assert_eq!(workspace_index.path, root.join(".zvec-grep"));
            assert_eq!(result.items[0].absolute_path, root.join("src/file.txt"));
            assert_eq!(result.items[0].relative_path, PathBuf::from("src/file.txt"));
            assert_eq!(result.items[0].status, ContextItemStatus::Fresh);
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
        future_indexed
            .set_index_status(FileIndexStatus::Indexed {
                indexed_epoch_ms: u64::MAX,
                entity_count: 1,
            })
            .expect("future indexed file");
        assert_eq!(
            super::file_freshness_status(&moved_root, &future_indexed),
            ContextItemStatus::PossiblyStale,
            "an index clock ahead of file mtime does not prove the source is unchanged"
        );
    }

    #[test]
    fn source_freshness_requires_a_snapshot_mismatch_or_missing_file() {
        use crate::domain::{FileId, FileIndexStatus, FileRecord, FileSnapshot, SourcePath};

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "source contents").expect("source");
        let mut file = FileRecord::new(
            FileId::new(1),
            SourcePath::new("source.txt").expect("source path"),
            FileSnapshot {
                size_bytes: 15,
                modified_epoch_ms: None,
                content_hash: None,
            },
            FileIndexStatus::NotIndexed,
        )
        .expect("file");
        assert_eq!(
            super::file_freshness_status(directory.path(), &file),
            ContextItemStatus::Fresh,
            "missing index/mtime/hash proof is not evidence of an updated file"
        );
        file.set_snapshot(FileSnapshot {
            content_hash: Some(crate::utils::sha256_hex(b"source contents")),
            ..file.snapshot().clone()
        })
        .expect("file snapshot");
        assert_eq!(
            super::file_freshness_status(directory.path(), &file),
            ContextItemStatus::Fresh
        );
        std::fs::write(&path, "edited contents").expect("same-size update");
        assert_eq!(
            super::file_freshness_status(directory.path(), &file),
            ContextItemStatus::PossiblyStale
        );
        std::fs::remove_file(&path).expect("remove source");
        assert_eq!(
            super::file_freshness_status(directory.path(), &file),
            ContextItemStatus::PossiblyStale
        );
    }

    #[test]
    fn normalizes_primary_and_supplemental_query_groups_like_main() {
        let mut options = ContextOptions {
            query: Some(" alpha ".to_owned()),
            queries: vec![" beta ".to_owned()],
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: " gamma ".to_owned(),
            }],
            ..ContextOptions::default()
        };
        let request = normalize_context_request(&options).expect("normalized context request");

        assert_eq!(request.display_query, "alpha | beta");
        assert_eq!(request.groups.len(), 3);
        assert_eq!(request.groups[0].id, "Q1");
        assert_eq!(request.groups[0].role, ContextQueryGroupRole::Primary);
        assert_eq!(request.groups[0].routes.len(), 2);
        assert_eq!(request.groups[2].id, "Q3");
        assert_eq!(request.groups[2].role, ContextQueryGroupRole::Supplemental);
        assert!(!request.groups[2].uses_vectors());

        options.fuse = true;
        let fused = normalize_context_request(&options).expect("fused context request");
        let [group] = fused.groups.as_slice() else {
            panic!("expected one fused group")
        };
        assert_eq!(group.query, "alpha | beta");
        assert_eq!(group.role, ContextQueryGroupRole::Primary);
        let routes: Vec<_> = request
            .groups
            .into_iter()
            .flat_map(|group| group.routes)
            .collect();
        assert_eq!(group.routes, routes);
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
        assert_eq!(selected[0].entity_id.as_deref(), Some("shared"));
        assert_eq!(selected[0].query_groups.len(), 2);
        assert_eq!(selected[0].matched_by, MatchedBy::FtsAndVector);
        assert_eq!(
            selected[0].selection_reason,
            Some(ContextSelectionReason::Coverage)
        );
        assert_eq!(selected[0].coverage_group.as_deref(), Some("Q1"));
        assert_eq!(selected[1].entity_id.as_deref(), Some("q2-only"));
        assert_eq!(selected[1].coverage_group.as_deref(), Some("Q2"));
        assert_eq!(
            selected.iter().map(|item| item.rank).collect::<Vec<_>>(),
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

    fn item(id: &str, query_group: ContextQueryGroupMatch) -> ContextItem {
        ContextItem {
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
            match_ranges: Vec::new(),
            content: id.to_owned(),
            outline: None,
            content_windows: Vec::new(),
            pdf_boxes: Vec::new(),
            content_role: Some(ContextContentRole::Source),
            status: ContextItemStatus::Fresh,
            score: Some(1.0),
            matched_by: query_group.matched_by,
            metadata: None,
            entity_id: Some(id.to_owned()),
            container: None,
            trace: None,
            query_groups: vec![query_group],
            selection_reason: None,
            coverage_group: None,
        }
    }
}
