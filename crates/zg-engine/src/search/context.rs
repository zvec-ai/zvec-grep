use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    EngineError,
    api::context::{
        ContextOptions, ContextResult,
        options::{ContextRoute, ContextRouteMode},
        result::{
            ContentRange, ContextContentRole, ContextCoverage, ContextDiagnostics,
            ContextGroupResult, ContextItem, ContextItemKind, ContextItemStatus,
            ContextQueryGroupMatch, ContextQueryGroupRole, ContextSelectionReason, ContextSource,
            ContextWorkspaceIndex, EmptyReason, IndexDiagnostics, IndexQueryGroupDiagnostics,
            IndexRouteDiagnostics, MatchedBy,
        },
    },
    domain::{Content, EntityContent, EntityFragment},
    storage::spi::{StoredFile, WorkspaceIndexStorage},
};

use super::pipeline::{
    SearchEmbeddingRuntime, SearchHit, SearchPlan, SearchPlanResult, search_workspace_index,
};

const DEFAULT_CONTEXT_LIMIT: usize = 10;
const DEFAULT_CONTEXT_TOTAL_LIMIT: usize = 30;
const DEFAULT_CONTEXT_PRIORITY_LIMIT: usize = 6;
const CONTEXT_GROUP_RRF_K: f64 = 60.0;

#[derive(Clone, Debug)]
pub(crate) struct NormalizedContextRequest {
    pub display_query: String,
    pub rg_patterns: Vec<String>,
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
    if primary_queries.is_empty()
        && routes.is_empty()
        && (!options.rg || options.rg_options.pattern_files.is_empty())
    {
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
    let rg_patterns = primary_queries
        .iter()
        .cloned()
        .chain(routes.iter().map(|route| route.query.clone()))
        .collect::<Vec<_>>();
    let display_query = if !primary_queries.is_empty() {
        primary_queries.join(" | ")
    } else if !routes.is_empty() {
        routes
            .iter()
            .map(|route| route.query.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    } else {
        options
            .rg_options
            .pattern_files
            .iter()
            .map(|path| format!("@{}", path.display()))
            .collect::<Vec<_>>()
            .join(" | ")
    };

    Ok(NormalizedContextRequest {
        display_query,
        rg_patterns,
        routes: all_routes,
        groups,
    })
}

pub(crate) async fn context_from_index(
    root: &Path,
    workspace_index: &crate::api::info::result::WorkspaceIndexInfo,
    storage: &dyn WorkspaceIndexStorage,
    embedding_model: Option<&dyn SearchEmbeddingRuntime>,
    options: &ContextOptions,
    request: &NormalizedContextRequest,
) -> Result<ContextResult, EngineError> {
    let groups = if options.fuse {
        vec![NormalizedContextGroup {
            id: "Q1".to_owned(),
            query: request.display_query.clone(),
            role: if request
                .groups
                .iter()
                .any(|group| group.role == ContextQueryGroupRole::Primary)
            {
                ContextQueryGroupRole::Primary
            } else {
                ContextQueryGroupRole::Supplemental
            },
            routes: request.routes.clone(),
        }]
    } else {
        request.groups.clone()
    };
    let limit = context_group_limit(options.limit, groups.len());
    let mut searches = Vec::with_capacity(groups.len());
    for group in &groups {
        searches.push(
            search_workspace_index(
                SearchPlan {
                    routes: group.routes.clone(),
                    limit: Some(limit),
                    trace: options.trace,
                    prefer_symbol: options.prefer_symbol,
                    symbol_types: options.symbol_types.clone(),
                    include_paths: options.include_paths.clone(),
                    exclude_paths: options.exclude_paths.clone(),
                    globs: options.globs.clone(),
                    insensitive_globs: options.insensitive_globs.clone(),
                    file_types: options.file_types.clone(),
                    excluded_file_types: options.excluded_file_types.clone(),
                    modified_after_epoch_ms: options.modified_after_epoch_ms,
                    modified_before_epoch_ms: options.modified_before_epoch_ms,
                },
                storage,
                embedding_model,
            )
            .await?,
        );
    }

    Ok(build_context_result(
        root,
        workspace_index,
        request,
        &groups,
        searches,
    ))
}

fn build_context_result(
    root: &Path,
    workspace_index: &crate::api::info::result::WorkspaceIndexInfo,
    request: &NormalizedContextRequest,
    groups: &[NormalizedContextGroup],
    searches: Vec<SearchPlanResult>,
) -> ContextResult {
    let group_items = searches
        .iter()
        .zip(groups)
        .map(|(search, group)| search_plan_to_context_items(search, root, group))
        .collect::<Vec<_>>();
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

    ContextResult {
        query: request.display_query.clone(),
        root: root.to_path_buf(),
        source: ContextSource::Index,
        coverage: ContextCoverage::RankedSample,
        workspace_index: Some(ContextWorkspaceIndex {
            id: workspace_index.id.clone(),
            name: workspace_index.name.clone(),
            path: workspace_index.path.clone(),
            generation: workspace_index.generation,
        }),
        items,
        group_results: groups
            .iter()
            .zip(group_items)
            .map(|(group, items)| ContextGroupResult {
                id: group.id.clone(),
                query: group.query.clone(),
                role: group.role,
                items,
            })
            .collect(),
        diagnostics: ContextDiagnostics {
            empty_reason: (hits_returned == 0).then_some(EmptyReason::NoMatches),
            index: Some(IndexDiagnostics {
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
    }
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
    root: &Path,
    group: &NormalizedContextGroup,
) -> Vec<ContextItem> {
    result
        .hits
        .iter()
        .map(|hit| {
            let target = context_item_target(hit);
            ContextItem {
                kind: ContextItemKind::IndexedEntity,
                rank: hit.rank,
                absolute_path: hit.file.source.absolute_path.clone(),
                relative_path: display_relative_path(root, &hit.file),
                range: hit.entity.range.into(),
                excerpt_range: target.excerpt_range,
                content: target.content,
                content_role: Some(target.content_role),
                outline: target.outline,
                status: file_freshness_status(&hit.file),
                score: Some(hit.score),
                matched_by: hit.matched_by,
                metadata: hit.entity.metadata.as_ref().map(Into::into),
                entity_id: Some(hit.entity.id.as_str().to_owned()),
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
            }
        })
        .collect()
}

fn display_relative_path(root: &Path, file: &StoredFile) -> PathBuf {
    if file.source.relative_path.as_os_str().is_empty() {
        file.source
            .absolute_path
            .strip_prefix(root)
            .map_or_else(|_| file.source.absolute_path.clone(), Path::to_path_buf)
    } else {
        file.source.relative_path.clone()
    }
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
    content_role: ContextContentRole,
    excerpt_range: Option<ContentRange>,
    outline: Option<String>,
}

fn context_item_target(hit: &SearchHit) -> ContextItemTarget {
    let window = hit
        .evidence
        .iter()
        .find_map(|evidence| match &evidence.fragment {
            EntityFragment::Window(window) => Some(window),
            EntityFragment::Standalone(_) | EntityFragment::Representative(_) => None,
        });
    if let Some(window) = window {
        let same_source = hit.entity.range == window.range
            && matches!(&hit.entity.content, EntityContent::Source(contents) if contents == &window.contents);
        let content = contents_to_text(&window.contents);
        let outline = match &hit.entity.content {
            EntityContent::Outline(outline)
                if !outline.trim().is_empty() && outline.trim() != content.trim() =>
            {
                Some(outline.trim().to_owned())
            }
            EntityContent::Source(_) | EntityContent::Outline(_) => None,
        };
        return ContextItemTarget {
            content,
            content_role: ContextContentRole::Source,
            excerpt_range: (!same_source).then(|| window.range.into()),
            outline,
        };
    }
    let (content, content_role) = match &hit.entity.content {
        EntityContent::Source(contents) => (contents_to_text(contents), ContextContentRole::Source),
        EntityContent::Outline(outline) => (outline.clone(), ContextContentRole::Outline),
    };
    ContextItemTarget {
        content,
        content_role,
        excerpt_range: None,
        outline: None,
    }
}

fn file_freshness_status(file: &StoredFile) -> ContextItemStatus {
    let Some(indexed) = file
        .index_status
        .as_ref()
        .and_then(|status| status.indexed_epoch_ms)
    else {
        return ContextItemStatus::PossiblyStale;
    };
    let Ok(metadata) = fs::metadata(&file.source.absolute_path) else {
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
    if modified.is_some_and(|modified| indexed >= modified) {
        return ContextItemStatus::Fresh;
    }
    if let Some(expected) = &file.source.snapshot.content_hash
        && fs::read(&file.source.absolute_path).is_ok_and(|bytes| sha256_hex(&bytes) == *expected)
    {
        return ContextItemStatus::Fresh;
    }
    ContextItemStatus::PossiblyStale
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn rank_as_f64(rank: usize) -> f64 {
    f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
}

fn contents_to_text(contents: &[Content]) -> String {
    contents
        .iter()
        .map(content_to_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_to_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Image(image) => format!(
            "[image:{} bytes={}]",
            image.format().as_str(),
            image.data().len()
        ),
        Content::Table(table) => {
            let mut cells = table.cells.iter().collect::<Vec<_>>();
            cells.sort_unstable_by_key(|cell| (cell.row, cell.column));
            let mut output = String::new();
            let mut previous_row = None;
            for cell in cells {
                if let Some(row) = previous_row {
                    output.push(if row == cell.row { '\t' } else { '\n' });
                }
                output.push_str(&contents_to_text(&cell.contents));
                previous_row = Some(cell.row);
            }
            output
        }
    }
}

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

    use super::{normalize_context_request, select_and_rank_context_items};

    #[test]
    fn renders_ordered_content_and_nested_table_cells() {
        use crate::domain::{
            Content, FileFormat, ImageContent, TableCell, TableCellRole, TableContent,
        };

        let image = ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image");
        let cell = |row, column, column_span, contents| TableCell {
            row,
            column,
            row_span: 1,
            column_span,
            contents,
            kind: TableCellRole::Unknown,
        };
        let table = Content::Table(TableContent {
            row_count: 2,
            column_count: 2,
            cells: vec![
                cell(0, 0, 2, vec![Content::Text("Product".to_owned())]),
                cell(
                    1,
                    0,
                    1,
                    vec![Content::Text("Keyboard".to_owned()), Content::Image(image)],
                ),
                cell(
                    1,
                    1,
                    1,
                    vec![Content::Table(TableContent {
                        row_count: 1,
                        column_count: 1,
                        cells: vec![cell(0, 0, 1, vec![Content::Text("299".to_owned())])],
                    })],
                ),
            ],
        });
        assert_eq!(
            super::contents_to_text(&[Content::Text("Catalog".to_owned()), table]),
            "Catalog\nProduct\nKeyboard\n[image:png bytes=3]\t299"
        );
    }

    #[test]
    fn explicit_content_roles_preserve_source_and_window_provenance() {
        use crate::{
            domain::{
                Content, Entity, EntityContent, EntityFragment, EntityId, FileFormat, FileId,
                FileSnapshot, FragmentId, SourceFile, SourceRange, TextRange, WindowFragment,
            },
            search::pipeline::{SearchEvidence, SearchHit},
            storage::spi::StoredFile,
        };

        let file_id = FileId::new("file").expect("file id");
        let range = SourceRange::Text(TextRange {
            start_line: 1,
            end_line: 20,
            start_utf16_offset: 0,
            end_utf16_offset: 100,
        });
        let mut hit = SearchHit {
            entity: Entity {
                id: EntityId::new("entity").expect("entity id"),
                file_id: file_id.clone(),
                range,
                content: EntityContent::Source(vec![Content::Text(
                    "A short source excerpt".to_owned(),
                )]),
                metadata: None,
            },
            file: StoredFile {
                source: SourceFile {
                    id: file_id.clone(),
                    absolute_path: PathBuf::from("/workspace/file.txt"),
                    relative_path: PathBuf::from("file.txt"),
                    root_path: PathBuf::from("/workspace"),
                    formats: vec![FileFormat::Text],
                    snapshot: FileSnapshot {
                        size_bytes: 100,
                        modified_epoch_ms: None,
                        content_hash: None,
                    },
                },
                index_status: None,
            },
            evidence: Vec::new(),
            rank: 1,
            score: 1.0,
            matched_by: MatchedBy::Fts,
            trace: None,
        };
        assert_eq!(
            super::context_item_target(&hit).content_role,
            ContextContentRole::Source
        );
        hit.entity.content = EntityContent::Outline("Function outline".to_owned());
        assert_eq!(
            super::context_item_target(&hit).content_role,
            ContextContentRole::Outline
        );
        let window_range = SourceRange::Text(TextRange {
            start_line: 2,
            end_line: 2,
            start_utf16_offset: 10,
            end_utf16_offset: 20,
        });
        hit.evidence.push(SearchEvidence {
            fragment: EntityFragment::Window(WindowFragment {
                id: FragmentId::new("window").expect("fragment id"),
                entity_id: hit.entity.id.clone(),
                file_id,
                range: window_range,
                contents: vec![Content::Text("Exact source".to_owned())],
                metadata: None,
            }),
        });
        let target = super::context_item_target(&hit);
        assert_eq!(target.content_role, ContextContentRole::Source);
        assert_eq!(target.content, "Exact source");
        assert_eq!(target.outline.as_deref(), Some("Function outline"));
        assert_eq!(target.excerpt_range, Some(window_range.into()));
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
        assert_eq!(request.rg_patterns, ["alpha", "beta", "gamma"]);
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
                start_offset: 0,
                end_offset: 1,
            },
            excerpt_range: None,
            content: id.to_owned(),
            content_role: Some(ContextContentRole::Source),
            outline: None,
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
