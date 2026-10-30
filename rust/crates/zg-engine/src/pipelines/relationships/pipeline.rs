//! Match canonical symbols, read call edges, and assemble bounded result groups.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use super::storage::{RelationshipRead, RelationshipStorage};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineResult,
    api::relationships::{
        CallSite, MAX_SYMBOL_MATCHES, RelationshipOptions, RelationshipResult, SymbolDefinition,
        SymbolRelationships,
    },
    domain::{
        Entity, EntityId, EntityMetadata, FileEdge, FileId, FileIndexStatus, FileRecord,
        GraphEdgeKind, Range,
    },
    pipelines::indexed_search::path_filter::resolve_glob_filter,
    storage::{
        graph::Direction,
        types::{StorageSearchFilter, StoredEntity},
    },
};

const MAX_SYMBOL_SUGGESTIONS: usize = 5;
const ENDPOINT_BATCH_SIZE: usize = 256;

pub(super) fn normalize(options: &RelationshipOptions) -> EngineResult<(String, Option<String>)> {
    options.validate()?;
    let symbol = options.symbol.trim();
    // Accept the dotted symbols shown by MCP rg, including mixed separators.
    let symbol = symbol.replace('.', "::");
    let (absolute, path) = symbol
        .strip_prefix("::")
        .map_or((false, symbol.as_str()), |rest| (true, rest));
    let (scope, name) = match path.rsplit_once("::") {
        Some((scope, name)) => (Some(scope), name),
        // `::main` names a top-level definition; plain `main` matches any scope.
        None if absolute => (Some(""), path),
        None => (None, path),
    };
    Ok((name.to_owned(), scope.map(str::to_owned)))
}

pub(super) fn query_symbols(
    storage: &impl RelationshipStorage,
    name: &str,
    scope: Option<&str>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    check_cancelled(options.signal.as_ref())?;
    let filter = resolve_glob_filter(
        std::path::Path::new(""),
        &options.globs,
        || storage.has_non_unicode_file_names(),
        false,
        || storage.list_file_paths(),
    )?;
    let mut result =
        collect_relationships(storage, name, scope, Some(&filter), options, direction)?;
    result.matches.sort_by(|a, b| {
        a.symbol
            .file_path
            .cmp(&b.symbol.file_path)
            .then_with(|| a.symbol.start_line.cmp(&b.symbol.start_line))
            .then_with(|| a.symbol.name.cmp(&b.symbol.name))
    });
    Ok(result)
}

fn collect_relationships(
    storage: &impl RelationshipStorage,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    check_cancelled(options.signal.as_ref())?;
    // Selection reads metadata, file records and bodies together: file_id alone
    // cannot detect a same-ID rewrite between these steps.
    let definitions = storage.read_snapshot(|view| {
        check_cancelled(options.signal.as_ref())?;
        if !view.has_graph()? {
            return Err(EngineError::not_found(
                "workspace graph is missing; this index has no persisted relationship data",
            ));
        }
        find_symbols(view, name, scope, filter)
    })?;
    let mut matches = Vec::new();
    for stored in &definitions {
        check_cancelled(options.signal.as_ref())?;
        // Keep the root recheck, edges, every visibility batch and endpoint
        // descriptions together. Writers may run between definitions, never
        // between reading an edge and checking its endpoints.
        if let Some(group) = storage.read_snapshot(|view| {
            check_cancelled(options.signal.as_ref())?;
            collect_match(view, stored, options.limit.unwrap_or(20), direction)
        })? {
            matches.push(group);
        }
    }
    // Stored definitions may lack text coordinates and be impossible to describe.
    // Only usable matches should suppress suggestions, including exact-name suggestions.
    let mut suggestions = Vec::new();
    if matches.is_empty() {
        if scope.is_some() {
            suggestions = storage
                .read_snapshot(|view| suggest_exact_symbols(view, name, filter))
                .unwrap_or_default()
                .iter()
                .filter_map(describe)
                .collect();
        }
        if suggestions.is_empty() {
            // Suggestions are optional: FTS syntax/storage errors must not turn
            // a successful exact lookup with no usable matches into a failed query.
            suggestions = storage
                .read_snapshot(|view| suggest_symbols(view, name, filter))
                .unwrap_or_default()
                .iter()
                .filter_map(describe)
                .collect();
        }
    }
    Ok(RelationshipResult {
        matches,
        suggestions,
    })
}

fn check_cancelled(signal: Option<&CancellationToken>) -> EngineResult<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(EngineError::cancelled("relationship query was cancelled"));
    }
    Ok(())
}

fn collect_match(
    storage: &dyn RelationshipRead,
    stored: &StoredEntity,
    limit: usize,
    direction: Direction,
) -> EngineResult<Option<SymbolRelationships>> {
    let id = stored.entity.id();
    let current = get_entities(storage, std::slice::from_ref(id))?.remove(id);
    // Entity IDs do not encode metadata or the containing file's revision.
    // Skip roots changed since selection rather than mix their old description
    // with newer edges. A later query can select the updated definition.
    if current.as_ref() != Some(stored) {
        return Ok(None);
    }
    let Some(symbol) = describe(stored) else {
        return Ok(None);
    };
    // Cache only within this definition's snapshot, including recursive calls.
    let mut symbols = HashMap::from([(id.clone(), Some(symbol.clone()))]);
    let mut edges = neighborhood(
        storage,
        id.as_str(),
        direction,
        Some(&[GraphEdgeKind::Calls]),
        limit.saturating_add(1),
    )?;
    let truncated = edges.len() > limit;
    edges.truncate(limit);
    let targets: Vec<_> = edges
        .iter()
        .map(|(edge, _)| {
            EntityId::from_string(if edge.source == id.as_str() {
                edge.target.clone()
            } else {
                edge.source.clone()
            })
        })
        .collect();
    let missing: Vec<_> = targets
        .iter()
        .filter(|target| !symbols.contains_key(*target))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let endpoints = get_entities(storage, &missing)?;
    for id in missing {
        symbols.insert(id.clone(), endpoints.get(&id).and_then(describe));
    }
    Ok(Some(SymbolRelationships {
        symbol,
        truncated,
        edges: edges
            .into_iter()
            .zip(targets)
            .map(|((edge, file_path), target)| CallSite {
                file_path,
                symbol: symbols.get(&target).cloned().flatten(),
                line: edge.line,
                column: edge.column,
            })
            .collect(),
    }))
}

fn describe(stored: &StoredEntity) -> Option<SymbolDefinition> {
    let Some(EntityMetadata::Code(metadata)) = stored.entity.metadata() else {
        return None;
    };
    let name = metadata
        .symbol_name
        .as_deref()
        .filter(|name| !name.is_empty())?;
    let Range::Text(range) = stored.entity.source_range() else {
        return None;
    };
    Some(SymbolDefinition {
        name: match metadata.scope.as_deref().filter(|scope| !scope.is_empty()) {
            Some(scope) => format!("{scope}::{name}"),
            None => name.to_owned(),
        },
        file_path: stored.file.relative_path().to_path_buf(),
        start_line: range.start_line(),
        end_line: range.end_line(),
    })
}

/// Bounded best-effort suggestions, never relationship query roots.
/// Do not require the requested scope: a misspelled scope is a useful retry case.
fn suggest_symbols(
    storage: &dyn RelationshipRead,
    name: &str,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let query = name.to_lowercase();
    let hits = storage.search_fts(&query, 100, filter)?;
    let mut seen = HashSet::new();
    let ids: Vec<_> = hits
        .into_iter()
        .map(|hit| hit.entity_id)
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let mut metadata = storage.fetch_code_metadata(&ids)?;
    let mut candidates: Vec<_> = ids
        .into_iter()
        .filter_map(|id| {
            let (file_id, metadata) = metadata.remove(&id)?;
            let rank = suggestion_rank(&query, metadata.symbol_name.as_deref()?)?;
            Some((id, file_id, rank))
        })
        .collect();
    // Stable sort preserves FTS relevance within each name-match category.
    // Rank before limiting, and hydrate only the selected visible definitions.
    candidates.sort_by_key(|(_, _, rank)| *rank);
    let file_ids: Vec<_> = candidates
        .iter()
        .map(|(_, file_id, _)| *file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let files = storage.fetch_files(&file_ids)?;
    let selected: Vec<_> = candidates
        .into_iter()
        .filter_map(|(id, file_id, _)| {
            let file = files.get(&file_id)?;
            file.index_status().is_indexed().then(|| (id, file.clone()))
        })
        .take(MAX_SYMBOL_SUGGESTIONS)
        .collect();
    load_symbols(storage, selected)
}

/// Resolve bounded roots and reuse their checked file records while hydrating.
pub(super) fn find_symbols(
    storage: &dyn RelationshipRead,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let selected = select_symbols(storage, name, scope, filter, MAX_SYMBOL_MATCHES + 1)?;
    if selected.len() > MAX_SYMBOL_MATCHES {
        return Err(EngineError::invalid_argument(format!(
            "symbol query matches more than {MAX_SYMBOL_MATCHES} indexed definitions; use a narrower scope::name or glob filter",
        )));
    }
    load_symbols(storage, selected)
}

/// Exact bare-name candidates for a wrong scope, bounded without an overflow error.
fn suggest_exact_symbols(
    storage: &dyn RelationshipRead,
    name: &str,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let selected = select_symbols(storage, name, None, filter, MAX_SYMBOL_SUGGESTIONS)?;
    load_symbols(storage, selected)
}

fn select_symbols(
    storage: &dyn RelationshipRead,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
    limit: usize,
) -> EngineResult<Vec<(EntityId, FileRecord)>> {
    if name.is_empty() {
        return Err(EngineError::invalid_argument(
            "symbol name must not be empty",
        ));
    }
    let ids = storage.find_symbol_entity_ids(name, filter)?;
    let mut selected = Vec::new();
    'batches: for batch in ids.chunks(128) {
        let mut owners: HashMap<_, _> = storage
            .fetch_code_metadata(batch)?
            .into_iter()
            .filter_map(|(id, (file_id, metadata))| {
                (metadata.symbol_name.as_deref() == Some(name)
                    && scope.is_none_or(|scope| {
                        metadata.scope.as_deref().unwrap_or("").replace('.', "::") == scope
                    }))
                .then_some((id, file_id))
            })
            .collect();
        let file_ids: Vec<_> = owners
            .values()
            .copied()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let files = storage.fetch_files(&file_ids)?;
        for id in batch {
            if let Some(file) = owners.remove(id).and_then(|file_id| files.get(&file_id))
                && file.index_status().is_indexed()
            {
                selected.push((id.clone(), file.clone()));
                if selected.len() == limit {
                    break 'batches;
                }
            }
        }
    }
    Ok(selected)
}

fn load_symbols(
    storage: &dyn RelationshipRead,
    selected: Vec<(EntityId, FileRecord)>,
) -> EngineResult<Vec<StoredEntity>> {
    let ids: Vec<_> = selected.iter().map(|(id, _)| id.clone()).collect();
    let mut entities = storage.fetch_entities(&ids)?;
    // Selection already read and checked these records under the same store lock.
    Ok(selected
        .into_iter()
        .filter_map(|(id, file)| {
            let entity = entities.remove(&id)?;
            (entity.file_id() == file.id()).then_some(StoredEntity { entity, file })
        })
        .collect())
}

/// Filter resolved edges by endpoint visibility before applying the limit.
fn neighborhood(
    storage: &dyn RelationshipRead,
    id: &str,
    direction: Direction,
    kinds: Option<&[GraphEdgeKind]>,
    limit: usize,
) -> EngineResult<Vec<(FileEdge, PathBuf)>> {
    let mut rows = storage
        .graph_neighborhood(id, direction, kinds)?
        .into_iter();
    let mut edges = Vec::new();
    let mut hydration_size = limit.clamp(1, ENDPOINT_BATCH_SIZE);
    let mut checked = HashSet::new();
    let mut visible = HashMap::new();
    while edges.len() < limit {
        let batch: Vec<_> = rows.by_ref().take(hydration_size).collect();
        if batch.is_empty() {
            break;
        }
        // SQLite can be ahead of zvec after a failed write. Filter both
        // endpoints before counting a row toward the requested limit.
        let ids: Vec<_> = batch
            .iter()
            .flat_map(|edge| [&edge.source, &edge.target])
            .map(|id| EntityId::from_string(id.clone()))
            .filter(|id| checked.insert(id.clone()))
            .collect();
        visible.extend(endpoint_paths(storage, &ids)?);
        let before = edges.len();
        let remaining = limit - before;
        edges.extend(
            batch
                .into_iter()
                .filter_map(|edge| {
                    let path = visible.get(&EntityId::from_string(edge.source.clone()))?;
                    visible.get(&EntityId::from_string(edge.target.clone()))?;
                    Some((edge, path.clone()))
                })
                .take(remaining),
        );
        // Avoid reading unused endpoints, but grow batches across invisible stretches.
        hydration_size = if edges.len() == before {
            hydration_size.saturating_mul(2).min(ENDPOINT_BATCH_SIZE)
        } else {
            (limit - edges.len()).clamp(1, ENDPOINT_BATCH_SIZE)
        };
    }
    Ok(edges)
}

/// Smaller ranks are better; reverse containment needs a meaningful name length.
fn suggestion_rank(query: &str, candidate: &str) -> Option<u8> {
    let candidate = candidate.to_lowercase();
    if candidate.is_empty() {
        None
    } else if candidate == query {
        Some(0)
    } else if candidate.starts_with(query) {
        Some(1)
    } else if candidate.contains(query) {
        Some(2)
    } else if candidate.chars().count() >= 3 && query.contains(&candidate) {
        Some(3)
    } else {
        None
    }
}

fn get_entities(
    storage: &dyn RelationshipRead,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, StoredEntity>> {
    let entities = storage.fetch_entities(ids)?;
    let file_ids = entities
        .values()
        .map(Entity::file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = storage.fetch_files(&file_ids)?;
    Ok(entities
        .into_iter()
        .filter_map(|(id, entity)| {
            let file = files.get(&entity.file_id())?;
            if !matches!(file.index_status(), FileIndexStatus::Indexed { .. }) {
                return None;
            }
            Some((
                id,
                StoredEntity {
                    entity,
                    file: file.clone(),
                },
            ))
        })
        .collect())
}

fn endpoint_paths(
    storage: &dyn RelationshipRead,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, PathBuf>> {
    let mut owners = storage.fetch_entity_file_ids(ids)?;
    for id in ids {
        if let Some(file_id) = id
            .as_str()
            .strip_prefix('f')
            .and_then(|id| id.parse::<u32>().ok())
        {
            // Only canonical f{file_id} spellings represent implicit file endpoints.
            if id.as_str() == format!("f{file_id}") {
                owners.insert(id.clone(), FileId::new(file_id));
            }
        }
    }
    let file_ids: Vec<_> = owners
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let files = storage.fetch_files(&file_ids)?;
    Ok(owners
        .into_iter()
        .filter_map(|(id, file_id)| {
            files
                .get(&file_id)
                .filter(|file| file.index_status().is_indexed())
                .map(|file| (id, file.relative_path().to_path_buf()))
        })
        .collect())
}
