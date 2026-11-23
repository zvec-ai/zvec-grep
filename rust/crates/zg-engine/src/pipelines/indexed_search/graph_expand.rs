use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    time::Instant,
};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineResult,
    api::context::result::{
        GraphCallTrace, GraphExpansionTrace, GraphRejection, RecallEntityTrace,
    },
    domain::{Entity, EntityId, EntityMetadata, FileEdge, FileRecord, GraphEdgeKind, Range},
    storage::{
        IndexStore, ReadView,
        types::{StoragePathFilter, StorageSearchFilter},
    },
};

use super::pipeline::SearchHit;

mod budget;
mod relevance;
pub(super) use relevance::Relevance;

const MAX_HOPS: usize = 3;
const ENDPOINT_BATCH: usize = 128;

pub(super) const ALL_EDGES: &[GraphEdgeKind] = &[
    GraphEdgeKind::Calls,
    GraphEdgeKind::Extends,
    GraphEdgeKind::Contains,
    GraphEdgeKind::Imports,
    GraphEdgeKind::Implements,
    GraphEdgeKind::Declares,
];

#[derive(Clone, Copy)]
pub(super) struct GraphSearch<'a> {
    pub storage: &'a IndexStore,
    pub signal: Option<&'a CancellationToken>,
    pub query: &'a str,
}

#[derive(Default)]
pub(super) struct Expansion {
    pub added: usize,
    pub trace: GraphExpansionTrace,
    pub relevance: Option<Relevance>,
}

struct Endpoint {
    entity: Option<RecallEntityTrace>,
    file: Option<FileRecord>,
    rejection: Option<GraphRejection>,
}

pub(super) fn expand(
    graph: GraphSearch<'_>,
    hits: &[SearchHit],
    filter: Option<&StorageSearchFilter>,
    trace: bool,
) -> EngineResult<Expansion> {
    let started = Instant::now();
    let mut result = Expansion {
        relevance: Some(Relevance::default()),
        ..Expansion::default()
    };
    check_cancelled(graph.signal)?;
    let outcome = graph.storage.read_snapshot(|view| {
        view.ensure_graph()?;
        budget::walk(
            view,
            hits,
            filter,
            graph.signal,
            graph.query,
            trace,
            &mut result,
        )
    });
    if let Err(error) = outcome {
        if error.code() == EngineError::CANCELLED {
            return Err(error);
        }
        result.trace.error = Some(error.to_string());
        result.relevance = None;
    }
    result.trace.duration_micros = micros(started);
    Ok(result)
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn seed_frontier(
    view: &ReadView<'_>,
    hits: &[SearchHit],
    filter: Option<&StorageSearchFilter>,
    signal: Option<&CancellationToken>,
    endpoints: &mut HashMap<EntityId, Endpoint>,
) -> EngineResult<BTreeSet<EntityId>> {
    let seeds = hits
        .iter()
        .filter(|hit| matches!(hit.entity.metadata(), Some(EntityMetadata::Code(_))))
        .collect::<Vec<_>>();
    let mut frontier = BTreeSet::new();
    for batch in seeds.chunks(ENDPOINT_BATCH) {
        check_cancelled(signal)?;
        let ids = batch.iter().map(|hit| hit.entity.id().clone()).collect();
        hydrate(view, ids, filter, endpoints)?;
        for hit in batch {
            let id = hit.entity.id();
            let endpoint = &endpoints[id];
            if endpoint.rejection.is_none() && endpoint.file.as_ref() == Some(&hit.file) {
                frontier.insert(id.clone());
            }
        }
    }
    Ok(frontier)
}

fn edge_trace(edge: FileEdge, endpoints: &HashMap<EntityId, Endpoint>) -> GraphCallTrace {
    let source = &endpoints[&EntityId::from_string(edge.source.clone())];
    GraphCallTrace {
        kind: Some(edge.kind),
        caller_path: relative_path(source.file.as_ref().expect("visible edge source")),
        caller_id: edge.source,
        callee_id: edge.target,
        line: edge.line,
        column: edge.column,
    }
}

fn hydrate(
    view: &ReadView<'_>,
    ids: Vec<EntityId>,
    filter: Option<&StorageSearchFilter>,
    endpoints: &mut HashMap<EntityId, Endpoint>,
) -> EngineResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let files = view.visible_endpoint_files(&ids)?;
    let entities = view.fetch_entities(&ids)?;
    for id in ids {
        let file = files.get(&id);
        let entity = entities.get(&id);
        let file_scope = entity.is_none()
            && file.is_some_and(|file| id.as_str() == format!("f{}", file.id().get()));
        let rejection = match file {
            Some(file)
                if file_scope
                    || entity.is_some_and(|entity| {
                        entity.file_id() == file.id()
                            && matches!(entity.source_range(), Range::Text(_))
                    }) =>
            {
                (!matches_filter(filter, file, entity)).then_some(GraphRejection::Filtered)
            }
            _ => Some(GraphRejection::Invisible),
        };
        let coordinates = file
            .zip(entity)
            .map(|(file, entity)| recall_entity(entity, file));
        endpoints.insert(
            id,
            Endpoint {
                entity: coordinates,
                file: file.cloned(),
                rejection,
            },
        );
    }
    Ok(())
}

fn matches_filter(
    filter: Option<&StorageSearchFilter>,
    file: &FileRecord,
    entity: Option<&Entity>,
) -> bool {
    filter.is_none_or(|filter| {
        filter
            .file_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&file.id()))
            && filter
                .path
                .as_ref()
                .is_none_or(|predicate| matches_path(predicate, file.relative_path().as_path()))
            && filter.symbol_types.as_ref().is_none_or(|types| {
                match entity.and_then(Entity::metadata) {
                    Some(EntityMetadata::Code(code)) => {
                        code.symbol_type.is_some_and(|kind| types.contains(&kind))
                    }
                    _ => entity.is_none(),
                }
            })
    })
}

fn matches_path(predicate: &StoragePathFilter, path: &Path) -> bool {
    match predicate {
        StoragePathFilter::Directory(directory) => path
            .parent()
            .is_some_and(|parent| parent.starts_with(directory.as_path())),
        StoragePathFilter::And(parts) => parts.iter().all(|part| matches_path(part, path)),
        StoragePathFilter::Or(parts) => parts.iter().any(|part| matches_path(part, path)),
        StoragePathFilter::Not(part) => !matches_path(part, path),
        _ => path
            .file_name()
            .is_some_and(|name| super::format_filter::matches_file_name(predicate, name)),
    }
}

fn neighbor<'a>(edge: &'a FileEdge, endpoint: &EntityId) -> &'a str {
    if edge.source == endpoint.as_str() {
        &edge.target
    } else {
        &edge.source
    }
}

fn check_cancelled(signal: Option<&CancellationToken>) -> EngineResult<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(EngineError::cancelled("call graph expansion was cancelled"));
    }
    Ok(())
}

pub(super) fn recall_entity(entity: &Entity, file: &FileRecord) -> RecallEntityTrace {
    RecallEntityTrace {
        entity_id: entity.id().as_str().to_owned(),
        path: relative_path(file),
        range: entity.source_range().into(),
    }
}

fn relative_path(file: &FileRecord) -> String {
    file.relative_path()
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
