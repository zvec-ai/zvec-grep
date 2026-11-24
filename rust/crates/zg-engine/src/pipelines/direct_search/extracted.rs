//! Literal search over canonical extracted text, with original source coordinates.
use crate::{
    EngineError, EngineResult,
    api::context::result::{
        ContentRange, ContextContentRole, ContextItem, ContextItemKind, ContextItemStatus,
        MatchedBy,
    },
    domain::{Content, Entity, IndexDescriptor, WorkspaceLayout},
    lexical::{
        extracted::{ExtractedFileState, ExtractedMatcher},
        types::LexicalSearchRequest,
    },
    pipelines::{
        freshness::file_freshness_status,
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

#[derive(Default)]
pub(super) struct SearchResult {
    pub items: Vec<ContextItem>,
    pub truncated: bool,
    pub unavailable: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub(super) fn enabled(root: &Path) -> EngineResult<bool> {
    let start = if root.is_file() {
        root.parent().unwrap_or(root)
    } else {
        root
    };
    let Some((root, layout)) = find_nearest_workspace(start)? else {
        return Ok(false);
    };
    Ok(read_workspace_manifest(&root, &layout)?.is_some_and(|workspace| workspace.index_enabled()))
}

pub(super) fn index_warning(path: &Path, error: EngineError) -> EngineResult<String> {
    match error.code() {
        EngineError::STORAGE_FAILURE
        | EngineError::INDEX_INCOMPATIBLE
        | EngineError::NOT_FOUND
        | EngineError::PERMISSION_DENIED
        | EngineError::RESOURCE_BUSY
        | EngineError::RESOURCE_CLOSED => Ok(format!(
            "Cannot read extraction index for {}: {error}{}; searching raw files instead.",
            path.display(),
            error
                .help()
                .map(|help| format!(" ({help})"))
                .unwrap_or_default()
        )),
        _ => Err(error),
    }
}

pub(super) async fn search(
    paths: Vec<PathBuf>,
    request: &LexicalSearchRequest,
    timeout_ms: Option<u64>,
    writers: &WriterContexts,
) -> EngineResult<SearchResult> {
    let mut workspaces = BTreeMap::new();
    let mut result = SearchResult::default();
    for path in paths {
        let absolute = std::path::absolute(&path)
            .map_err(|e| EngineError::from_io("resolve extracted source path", &e))?;
        let (root, layout) = match find_nearest_workspace(absolute.parent().unwrap_or(&absolute)) {
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                result.unavailable.push(absolute);
                continue;
            }
            Err(error) => {
                result.warnings.push(index_warning(&absolute, error)?);
                result.unavailable.push(absolute);
                continue;
            }
        };
        workspaces
            .entry(root)
            .or_insert_with(|| (layout, Vec::new()))
            .1
            .push(absolute);
    }
    let wait = LockWait::new(request.signal.as_ref(), timeout_ms)?;
    for (root, (layout, paths)) in workspaces {
        let mut found = match search_workspace(&root, layout, &paths, request, &wait, writers).await
        {
            Ok(found) => found,
            Err(error) => {
                result.warnings.push(index_warning(&root, error)?);
                result.unavailable.extend(paths);
                continue;
            }
        };
        result.items.append(&mut found.items);
        result.unavailable.append(&mut found.unavailable);
        result.truncated |= found.truncated;
    }
    wait.check_cancelled()?;
    Ok(result)
}

async fn search_workspace(
    root: &Path,
    layout: WorkspaceLayout,
    paths: &[PathBuf],
    request: &LexicalSearchRequest,
    wait: &LockWait<'_>,
    writers: &WriterContexts,
) -> EngineResult<SearchResult> {
    let source = acquire_read_source(root, &layout, wait, "extracted.rg", Some(writers), |_| {
        Ok(Some(()))
    })
    .await?;
    let (root, paths, request) = (root.to_path_buf(), paths.to_vec(), request.clone());
    tokio::task::spawn_blocking(move || {
        let run = |storage: &IndexStore| search_store(storage, &root, &paths, &request);
        let _lock = match source {
            ReadSource::Writer { lease, .. } => return run(&lease.session.storage),
            ReadSource::Read(lock) => lock,
        };
        let Some(workspace) = read_workspace_manifest(&root, &layout)?
            .filter(crate::domain::Workspace::index_enabled)
        else {
            return Ok(SearchResult {
                unavailable: paths,
                ..SearchResult::default()
            });
        };
        let graph = workspace
            .index()
            .descriptor()
            .is_some_and(IndexDescriptor::graph_indexing);
        let lease = ReadSessionLease::open(workspace.layout().index_dir(), graph)?;
        let result = run(lease.storage());
        let close = lease.close();
        let result = result?;
        close?;
        Ok(result)
    })
    .await
    .map_err(|e| EngineError::internal(format!("Extracted search worker failed: {e}")))?
}

fn search_store(
    storage: &IndexStore,
    root: &Path,
    paths: &[PathBuf],
    request: &LexicalSearchRequest,
) -> EngineResult<SearchResult> {
    let files = storage
        .list_files()?
        .into_iter()
        .map(|file| (root.join(file.relative_path()), file))
        .collect::<BTreeMap<_, _>>();
    let search_root = request.root.as_deref().unwrap_or(root);
    let matcher = ExtractedMatcher::new(search_root, request)?;
    let mut result = SearchResult::default();
    for path in paths {
        let Some(file) = files
            .get(path)
            .filter(|file| file.index_status().is_indexed())
        else {
            result.unavailable.push(path.clone());
            continue;
        };
        let snapshot = storage.read_snapshot(|view| {
            let Some(file) = view
                .fetch_files(&[file.id()])?
                .remove(&file.id())
                .filter(|file| file.index_status().is_indexed())
            else {
                return Ok(None);
            };
            let entities = view.entities_for_file(file.id())?;
            Ok(Some((file, entities)))
        })?;
        let Some((file, mut entities)) = snapshot else {
            result.unavailable.push(path.clone());
            continue;
        };
        let status = file_freshness_status(root, &file);
        entities.sort_by_key(|entity| entity.source_range().byte_offsets().map(|(start, _)| start));
        let mut state = ExtractedFileState::default();
        for entity in entities {
            if let Some(item) = search_entity(&matcher, request, path, status, &entity, &mut state)?
            {
                result.items.push(item);
            }
            if state.stopped {
                break;
            }
        }
    }
    result.truncated = matcher.truncated.load(Ordering::Relaxed);
    Ok(result)
}

fn search_entity(
    matcher: &ExtractedMatcher,
    request: &LexicalSearchRequest,
    path: &Path,
    status: ContextItemStatus,
    entity: &Entity,
    state: &mut ExtractedFileState,
) -> EngineResult<Option<ContextItem>> {
    let (Content::Text(text) | Content::Code(text)) = entity.content() else {
        return Ok(None);
    };
    let source = entity.source_range();
    let project = |span| ContentRange::project_text(&source, text, span);
    let start = source
        .byte_offsets()
        .ok_or_else(|| EngineError::internal("Extracted entity has no source offsets"))?
        .0;
    let hits = matcher.search(text, start, state)?;
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
        status,
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
