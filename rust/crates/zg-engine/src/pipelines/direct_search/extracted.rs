//! Literal search over canonical extracted text, with original source coordinates.
use crate::{
    EngineError, EngineResult,
    api::context::result::{
        ContentRange, ContextContentRole, ContextItem, ContextItemKind, ContextItemStatus,
        MatchedBy,
    },
    domain::{Content, Entity, FileRecord, IndexDescriptor},
    lexical::{
        extracted::{ExtractedFileState, ExtractedMatcher},
        types::LexicalSearchRequest,
    },
    pipelines::{
        indexed_search::writer::WriterContexts,
        read_access::{ReadSource, acquire_read_source},
    },
    storage::{IndexStore, read_session::ReadSessionLease},
    workspace::{find_nearest_workspace, lock::LockWait, manifest::read_workspace_manifest},
};
use std::{
    collections::BTreeMap,
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
        let absolute = std::path::absolute(&path)
            .map_err(|e| EngineError::from_io("resolve extracted source path", &e))?;
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
        let source =
            acquire_read_source(&root, &layout, &wait, "extracted.rg", Some(writers), |_| {
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
                    entities.sort_by_key(|entity| {
                        entity.source_range().byte_offsets().map(|(start, _)| start)
                    });
                    let mut state = ExtractedFileState::default();
                    for entity in entities {
                        if let Some(item) =
                            search_entity(&matcher, &request, &path, &file, &entity, &mut state)?
                        {
                            items.push(item);
                        }
                        if state.stopped {
                            break;
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
        .map_err(|e| EngineError::internal(format!("Extracted search worker failed: {e}")))??;
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
    state: &mut ExtractedFileState,
) -> EngineResult<Option<ContextItem>> {
    let (Content::Text(text) | Content::Code(text)) = entity.content() else {
        return Ok(None);
    };
    let source = entity.source_range();
    let project = |span| ContentRange::project_text(&source, text, span);
    let hits = matcher.search(text, state)?;
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
    let selection = ContentRange::project_selections(
        &source,
        text,
        hits.iter().flat_map(|hit| {
            // Inverted searches select lines; zero-width matches have no highlight.
            let ranges = if hit.match_ranges.is_empty() {
                std::slice::from_ref(&hit.range)
            } else {
                &hit.match_ranges
            };
            ranges
                .iter()
                .map(|range| range.start_byte_offset()..range.end_byte_offset())
        }),
    )?;
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
        content_range: project(start..end)?,
        match_ranges: hits
            .iter()
            .flat_map(|item| item.match_ranges.iter())
            .map(|r| project(r.start_byte_offset()..r.end_byte_offset()))
            .collect::<EngineResult<_>>()?,
        content_windows: windows
            .into_iter()
            .map(project)
            .collect::<EngineResult<_>>()?,
        pdf_boxes: crate::api::context::result::PdfBox::from_range(&source),
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
