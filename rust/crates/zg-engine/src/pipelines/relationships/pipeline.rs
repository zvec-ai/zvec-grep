//! Match canonical symbols, read call edges, and assemble bounded result groups.

use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path},
};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineResult,
    api::relationships::{
        RelationshipEdge, RelationshipOptions, RelationshipResult, RelationshipSymbol,
        SymbolRelationships,
    },
    domain::{EntityId, EntityMetadata, GraphEdgeKind, Range},
    storage::{IndexStore, RelationshipReadSession, graph::Direction, types::StoredEntity},
};

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
    storage: &IndexStore,
    name: &str,
    scope: Option<&str>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    let path: Option<std::path::PathBuf> = options.path.as_ref().map(|path| {
        path.components()
            .filter(|part| *part != Component::CurDir)
            .collect()
    });
    check_cancelled(options.signal.as_ref())?;
    // All definitions, edges and endpoints belong to the same store snapshot.
    // Sorting the owned result does not need to keep the store locked.
    let mut result = storage.with_relationship_read(|session| {
        query_session(session, name, scope, path.as_deref(), options, direction)
    })?;
    result.matches.sort_by(|a, b| {
        a.symbol
            .file_path
            .cmp(&b.symbol.file_path)
            .then_with(|| a.symbol.start_line.cmp(&b.symbol.start_line))
            .then_with(|| a.symbol.name.cmp(&b.symbol.name))
    });
    Ok(result)
}

fn query_session(
    storage: &RelationshipReadSession<'_>,
    name: &str,
    scope: Option<&str>,
    path: Option<&Path>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    check_cancelled(options.signal.as_ref())?;
    let definitions = storage.find_symbols(name, scope, path)?;
    let mut symbols: HashMap<_, _> = definitions
        .iter()
        .map(|stored| (stored.entity.id().clone(), describe(stored)))
        .collect();
    let mut matches = Vec::new();
    for stored in &definitions {
        check_cancelled(options.signal.as_ref())?;
        if let Some(group) = collect_match(
            storage,
            stored,
            options.limit.unwrap_or(20),
            direction,
            &mut symbols,
        )? {
            matches.push(group);
        }
    }
    // Stored definitions may lack text coordinates and be impossible to describe.
    // Only usable matches should suppress suggestions, including exact-name suggestions.
    let mut suggestions = Vec::new();
    if matches.is_empty() {
        if scope.is_some() {
            suggestions = storage
                .suggest_exact_symbols(name, path)
                .unwrap_or_default()
                .iter()
                .filter_map(describe)
                .collect();
        }
        if suggestions.is_empty() {
            // Suggestions are optional: FTS syntax/storage errors must not turn
            // a successful exact lookup with no usable matches into a failed query.
            suggestions = storage
                .suggest_symbols(name, path)
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
    storage: &RelationshipReadSession<'_>,
    stored: &StoredEntity,
    limit: usize,
    direction: Direction,
    symbols: &mut HashMap<EntityId, Option<RelationshipSymbol>>,
) -> EngineResult<Option<SymbolRelationships>> {
    let Some(symbol) = symbols.get(stored.entity.id()).cloned().flatten() else {
        return Ok(None);
    };
    let id = stored.entity.id();
    let mut edges = storage.neighborhood(
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
    let endpoints = storage.get_entities(&missing)?;
    for id in missing {
        symbols.insert(id.clone(), endpoints.get(&id).and_then(describe));
    }
    Ok(Some(SymbolRelationships {
        symbol,
        truncated,
        edges: edges
            .into_iter()
            .zip(targets)
            .map(|((edge, file_path), target)| RelationshipEdge {
                file_path,
                symbol: symbols.get(&target).cloned().flatten(),
                line: edge.line,
                column: edge.column,
            })
            .collect(),
    }))
}

fn describe(stored: &StoredEntity) -> Option<RelationshipSymbol> {
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
    Some(RelationshipSymbol {
        name: match metadata.scope.as_deref().filter(|scope| !scope.is_empty()) {
            Some(scope) => format!("{scope}::{name}"),
            None => name.to_owned(),
        },
        file_path: stored.file.relative_path().to_path_buf(),
        start_line: range.start_line(),
        end_line: range.end_line(),
    })
}
