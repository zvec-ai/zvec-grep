//! Literal PDF search reads canonical entities; it never searches PDF binary bytes.
use crate::{
    EngineError, EngineResult,
    api::context::result::{
        ContentRange, ContextContentRole, ContextItem, ContextItemKind, ContextItemStatus,
        MatchedBy,
    },
    domain::{Content, Entity, FileRecord, IndexDescriptor, PdfSourceMap, Range},
    lexical::{
        extracted::ExtractedMatcher,
        types::{LexicalMatch, LexicalSearchRequest},
    },
    pipelines::{
        indexed_search::writer::WriterContexts,
        read_access::{ReadSource, acquire_read_source},
    },
    storage::{IndexStore, read_session::ReadSessionLease},
    workspace::{find_nearest_workspace, lock::LockWait, manifest::read_workspace_manifest},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

pub(super) async fn search(
    paths: Vec<PathBuf>,
    request: &LexicalSearchRequest,
    timeout_ms: Option<u64>,
    writers: &WriterContexts,
) -> EngineResult<(Vec<ContextItem>, bool)> {
    let mut workspaces = BTreeMap::new();
    for path in paths {
        let absolute =
            std::path::absolute(&path).map_err(|e| EngineError::from_io("resolve PDF path", &e))?;
        let (root, layout) = find_nearest_workspace(absolute.parent().unwrap_or(&absolute))?
            .ok_or_else(|| missing_index(&absolute))?;
        workspaces
            .entry(root)
            .or_insert_with(|| (layout, Vec::new()))
            .1
            .push(absolute);
    }
    let wait = LockWait::new(request.signal.as_ref(), timeout_ms)?;
    let mut items = Vec::new();
    let mut truncated = false;
    for (root, (layout, paths)) in workspaces {
        let source = acquire_read_source(&root, &layout, &wait, "pdf.rg", Some(writers), |_| {
            Ok(Some(()))
        })
        .await?;
        let request = request.clone();
        let (mut found, limited) = tokio::task::spawn_blocking(move || {
            let run = |storage: &IndexStore| {
                let files = storage
                    .list_files()?
                    .into_iter()
                    .map(|file| (root.join(file.relative_path()), file))
                    .collect::<BTreeMap<_, _>>();
                let search_root = request.root.as_deref().unwrap_or(&root);
                let matcher = ExtractedMatcher::new(search_root, &request)?;
                let mut items = Vec::new();
                for path in paths {
                    let file = files
                        .get(&path)
                        .filter(|file| file.index_status().is_indexed())
                        .ok_or_else(|| missing_index(&path))?;
                    let (file, mut entities) = storage.read_snapshot(|view| {
                        let file = view
                            .fetch_files(&[file.id()])?
                            .remove(&file.id())
                            .filter(|file| file.index_status().is_indexed())
                            .ok_or_else(|| missing_index(&path))?;
                        let entities = view.entities_for_file(file.id())?;
                        Ok((file, entities))
                    })?;
                    entities.sort_by_key(|entity| match entity.source_range() {
                        Range::Pdf(source) => source.markdown_start,
                        _ => usize::MAX,
                    });
                    let mut count = 0;
                    for entity in entities {
                        if let Some(item) =
                            search_entity(&matcher, &request, &path, &file, &entity, &mut count)?
                        {
                            items.push(item);
                        }
                    }
                }
                Ok((items, matcher.truncated.load(Ordering::Relaxed)))
            };
            let _lock = match source {
                ReadSource::Writer { lease, .. } => return run(&lease.session.storage),
                ReadSource::Read(lock) => lock,
            };
            let workspace =
                read_workspace_manifest(&root, &layout)?.ok_or_else(|| missing_index(&root))?;
            if !workspace.index_enabled() {
                return Err(missing_index(&root));
            }
            let graph = workspace
                .index()
                .descriptor()
                .is_some_and(IndexDescriptor::graph_indexing);
            let lease = ReadSessionLease::open(workspace.layout().index_dir(), graph)?;
            let result = run(lease.storage());
            let close = lease.close();
            let result = result?;
            close?;
            Ok::<_, EngineError>(result)
        })
        .await
        .map_err(|e| EngineError::internal(format!("PDF search worker failed: {e}")))??;
        items.append(&mut found);
        truncated |= limited;
    }
    wait.check_cancelled()?;
    Ok((items, truncated))
}

fn missing_index(path: &Path) -> EngineError {
    EngineError::not_found(format!(
        "PDF extraction index unavailable for {}; run `zg --index` in its workspace first",
        path.display()
    ))
}

fn search_entity(
    matcher: &ExtractedMatcher,
    request: &LexicalSearchRequest,
    path: &Path,
    file: &FileRecord,
    entity: &Entity,
    count: &mut usize,
) -> EngineResult<Option<ContextItem>> {
    let (Range::Pdf(source), Content::Text(text)) = (entity.source_range(), entity.content())
    else {
        return Ok(None);
    };
    let hits = matcher.search(text, count)?;
    if hits.is_empty() {
        return Ok(None);
    }
    let lines = crate::utils::line_byte_offsets(text);
    let mut windows: Vec<std::ops::Range<usize>> = Vec::new();
    for matched in &hits {
        let first_line = matched
            .content_range
            .start_line()
            .saturating_sub(request.options.matching.before_context.saturating_add(1));
        let last_line = ContentRange::from(matched.content_range)
            .last_line()
            .unwrap_or(1)
            .saturating_add(request.options.matching.after_context);
        let window = lines.get(first_line).copied().unwrap_or(0)
            ..lines.get(last_line).copied().unwrap_or(text.len());
        if let Some(previous) = windows
            .last_mut()
            .filter(|previous| previous.end >= window.start)
        {
            previous.end = previous.end.max(window.end);
        } else {
            windows.push(window);
        }
    }
    let start = windows.first().expect("matched window").start;
    let end = windows.last().expect("matched window").end;
    let root = request.root.as_deref().unwrap_or_else(|| Path::new("."));
    let selection = pdf_match_range(&source, &hits);
    let range = if request.paths_selected {
        // Path lookup still describes the complete owning entity.
        entity.source_range().into()
    } else {
        selection.clone()
    };
    Ok(Some(ContextItem {
        kind: ContextItemKind::LexicalMatch,
        rank: 0,
        absolute_path: path.to_path_buf(),
        relative_path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
        range,
        excerpt_range: Some(selection),
        content_range: pdf_range(&source, start..end),
        match_ranges: hits
            .iter()
            .flat_map(|item| item.match_ranges.iter())
            .map(|r| pdf_range(&source, r.start_byte_offset()..r.end_byte_offset()))
            .collect(),
        content_windows: windows
            .into_iter()
            .map(|window| pdf_range(&source, window))
            .collect(),
        pdf_boxes: crate::api::context::result::PdfBox::from_source(&source),
        content: text[start..end].to_owned(),
        outline: None,
        content_role: Some(ContextContentRole::Source),
        status: freshness(path, file),
        score: None,
        matched_by: MatchedBy::Lexical,
        metadata: None,
        entity_id: Some(entity.id().as_str().to_owned()),
        container: None,
        trace: None,
        query_groups: Vec::new(),
        selection_reason: None,
        coverage_group: None,
    }))
}
fn pdf_match_range(source: &PdfSourceMap, hits: &[LexicalMatch]) -> ContentRange {
    let mut pages = BTreeSet::new();
    let mut start = source.text_len;
    let mut end = 0;
    for hit in hits {
        // Inverted searches select lines; zero-width matches have no highlight.
        let ranges = if hit.match_ranges.is_empty() {
            std::slice::from_ref(&hit.range)
        } else {
            &hit.match_ranges
        };
        for range in ranges {
            let span = range.start_byte_offset()..range.end_byte_offset();
            start = start.min(span.start);
            end = end.max(span.end);
            // Do not include unmatched pages between two separate hits.
            pages.extend(pdf_pages(source, span));
        }
    }
    ContentRange::Pdf {
        pages: pages.into_iter().collect(),
        start_byte_offset: source.markdown_start + start,
        end_byte_offset: source.markdown_start + end,
    }
}

fn pdf_range(source: &PdfSourceMap, range: std::ops::Range<usize>) -> ContentRange {
    ContentRange::Pdf {
        pages: pdf_pages(source, range.clone()),
        start_byte_offset: source.markdown_start + range.start,
        end_byte_offset: source.markdown_start + range.end,
    }
}

fn pdf_pages(source: &PdfSourceMap, range: std::ops::Range<usize>) -> Vec<u32> {
    let pages = source.pages(range.clone());
    if !pages.is_empty() {
        return pages;
    }
    // Anchor zero-width matches to the block at that position. Unmapped
    // separators belong to the preceding block (or the first for leading space).
    source
        .parts
        .iter()
        .rev()
        .find(|part| part.range.start <= range.start)
        .or_else(|| source.parts.first())
        .map(|part| vec![part.block.page])
        .unwrap_or_default()
}
fn freshness(path: &Path, file: &FileRecord) -> ContextItemStatus {
    let fresh = std::fs::metadata(path).is_ok_and(|metadata| {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        metadata.len() == file.snapshot().size_bytes
            && modified == file.snapshot().modified_epoch_ms
    });
    if fresh {
        ContextItemStatus::Fresh
    } else {
        ContextItemStatus::PossiblyStale
    }
}
