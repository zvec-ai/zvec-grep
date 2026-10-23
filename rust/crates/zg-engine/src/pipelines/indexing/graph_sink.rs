//! Bridges partition results into the graph persistence types.
//!
//! The sink activates once the coordinator writes snapshots to graph storage;
//! until then [`file_graph`] has no production caller. Ownership, edge
//! direction, and line-number semantics stay with `write_file_graph` — this
//! layer only converts types and rejects values that cannot be represented.

use std::collections::BTreeMap;

use super::graph_assembly::file_node_id;
use crate::domain::{
    EdgeProvenance, FileFormat, FileGraphResult, FileId, GraphEdgeKind, GraphRefKind,
    PendingRefStatus,
};
use crate::storage::graph;

impl From<GraphEdgeKind> for graph::EdgeKind {
    fn from(kind: GraphEdgeKind) -> Self {
        match kind {
            GraphEdgeKind::Contains => Self::Contains,
            GraphEdgeKind::Calls => Self::Calls,
            GraphEdgeKind::Imports => Self::Imports,
            GraphEdgeKind::Extends => Self::Extends,
            GraphEdgeKind::Implements => Self::Implements,
            GraphEdgeKind::Declares => Self::Declares,
        }
    }
}

impl From<GraphRefKind> for graph::EdgeKind {
    fn from(kind: GraphRefKind) -> Self {
        match kind {
            GraphRefKind::Calls => Self::Calls,
            GraphRefKind::Imports => Self::Imports,
            GraphRefKind::Extends => Self::Extends,
            GraphRefKind::Implements => Self::Implements,
            GraphRefKind::Declares => Self::Declares,
        }
    }
}

impl From<EdgeProvenance> for graph::Provenance {
    fn from(provenance: EdgeProvenance) -> Self {
        match provenance {
            EdgeProvenance::FileLocal => Self::FileLocal,
            EdgeProvenance::ImportScoped => Self::ImportScoped,
            EdgeProvenance::PreferredFile => Self::PreferredFile,
            EdgeProvenance::WorkspaceUnique => Self::WorkspaceUnique,
        }
    }
}

/// Converts a partitioned file graph into the persistence snapshot.
///
/// Pending references owned by the file-scope node are remapped onto the
/// storage endpoint `f{file_id}`, which is how `write_file_graph` models
/// file-scoped rows. Resolution candidates stay empty: populating them is the
/// resolver's job, not extraction's.
///
/// # Errors
/// Returns an invalid-input error for reference statuses other than pending
/// and for positions that exceed the storage layer's `u32` range.
pub(super) fn file_graph(
    file_id: FileId,
    language: FileFormat,
    result: &FileGraphResult,
) -> Result<graph::FileGraph, graph::Error> {
    let file_scope = file_node_id(&file_id.to_string());
    let storage_file_id = format!("f{}", file_id.get());

    let entity_ids = result.nodes.iter().map(|node| node.id.clone()).collect();

    let mut edges = Vec::with_capacity(result.edges.len());
    for edge in &result.edges {
        edges.push(graph::Edge {
            kind: edge.kind.into(),
            source: edge.source.clone(),
            target: edge.target.clone(),
            line: position(edge.line)?,
            column: position(edge.column)?,
            provenance: edge.provenance.into(),
            metadata: metadata(&edge.metadata),
        });
    }

    let mut pending_refs = Vec::with_capacity(result.pending_refs.len());
    for pending in &result.pending_refs {
        if pending.status != PendingRefStatus::Pending {
            return Err(graph::Error::InvalidInput(
                "partition only buffers pending references",
            ));
        }
        pending_refs.push(graph::PendingRef {
            from_node_id: owner_endpoint(&pending.owner_id, &file_scope, &storage_file_id),
            reference_name: pending.ref_name.clone(),
            receiver_name: pending.receiver_name.clone(),
            reference_kind: pending.ref_kind.into(),
            arity: position(pending.arity)?,
            line: scalar(pending.line)?,
            col: scalar(pending.column)?,
            metadata: metadata(&pending.metadata),
            candidates: None,
            language: language.as_str().to_owned(),
            name_tail: name_tail(&pending.ref_name),
        });
    }

    Ok(graph::FileGraph {
        entity_ids,
        edges,
        pending_refs,
    })
}

/// File-scope owners move from the hashed node id to the storage endpoint.
fn owner_endpoint(owner_id: &str, file_scope: &str, storage_file_id: &str) -> String {
    if owner_id == file_scope {
        storage_file_id.to_owned()
    } else {
        owner_id.to_owned()
    }
}

/// The resolver matches unqualified names, so keep only the final segment.
fn name_tail(reference_name: &str) -> String {
    reference_name
        .rsplit("::")
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn position(value: Option<usize>) -> Result<Option<u32>, graph::Error> {
    value.map(scalar).transpose()
}

fn scalar(value: usize) -> Result<u32, graph::Error> {
    u32::try_from(value).map_err(|_| graph::Error::InvalidInput("graph positions exceed u32"))
}

fn metadata(source: &BTreeMap<String, serde_json::Value>) -> graph::Metadata {
    source
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileEdge, FileGraphNode, PendingRef, SymbolType};
    use serde_json::json;

    fn sample_file_id() -> FileId {
        FileId::new(7)
    }

    fn sample_language() -> FileFormat {
        FileFormat::TypeScript
    }

    fn sample_pending(owner_id: &str) -> PendingRef {
        PendingRef {
            owner_id: owner_id.to_owned(),
            ref_name: "alpha::beta".to_owned(),
            receiver_name: Some("alpha".to_owned()),
            ref_kind: GraphEdgeKind::Imports,
            arity: Some(2),
            line: 10,
            column: 4,
            status: PendingRefStatus::Pending,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn maps_edges_and_entity_ids_without_nodes() {
        let mut metadata = BTreeMap::new();
        metadata.insert("rawText".to_owned(), json!("a(b)"));
        let result = FileGraphResult {
            nodes: vec![FileGraphNode {
                id: "entity-1".to_owned(),
                kind: SymbolType::Function,
                name: Some("a".to_owned()),
                qualified_name: "a".to_owned(),
                language: "typescript".to_owned(),
                start_line: 1,
                end_line: 2,
                start_column: 0,
                end_column: 8,
                signature: None,
                doc: None,
                arity: None,
                visibility: None,
                is_exported: false,
                is_declaration: false,
            }],
            edges: vec![FileEdge {
                kind: GraphEdgeKind::Contains,
                source: "f7".to_owned(),
                target: "entity-1".to_owned(),
                line: Some(1),
                column: Some(0),
                provenance: EdgeProvenance::FileLocal,
                metadata,
            }],
            pending_refs: Vec::new(),
        };

        let snapshot =
            file_graph(sample_file_id(), sample_language(), &result).expect("snapshot converts");

        assert_eq!(snapshot.entity_ids, vec!["entity-1".to_owned()]);
        assert_eq!(snapshot.pending_refs, Vec::new());
        let [edge] = snapshot.edges.as_slice() else {
            panic!("expected one edge");
        };
        assert_eq!(edge.kind, graph::EdgeKind::Contains);
        assert_eq!(edge.provenance, graph::Provenance::FileLocal);
        assert_eq!(edge.line, Some(1));
        assert_eq!(edge.metadata["rawText"], json!("a(b)"));
    }

    #[test]
    fn remaps_the_file_scope_owner_and_derives_name_tail() {
        let file_id = sample_file_id();
        let file_scope = file_node_id(&file_id.to_string());
        let entity_scope = "entity-9".to_owned();
        let result = FileGraphResult {
            pending_refs: vec![sample_pending(&file_scope), sample_pending(&entity_scope)],
            ..FileGraphResult::default()
        };

        let snapshot = file_graph(file_id, sample_language(), &result).expect("snapshot converts");

        let [file_owned, entity_owned] = snapshot.pending_refs.as_slice() else {
            panic!("expected two pending refs");
        };
        assert_eq!(file_owned.from_node_id, "f7");
        assert_eq!(entity_owned.from_node_id, "entity-9");
        assert_eq!(file_owned.name_tail, "beta");
        assert_eq!(file_owned.reference_kind, graph::EdgeKind::Imports);
        assert_eq!(file_owned.language, "typescript");
        assert_eq!(file_owned.candidates, None);
        assert_eq!(file_owned.line, 10);
        assert_eq!(file_owned.col, 4);
        assert_eq!(file_owned.arity, Some(2));
    }

    #[test]
    fn rejects_resolved_references_and_positions_beyond_u32() {
        let mut resolved = sample_pending("f7");
        resolved.status = PendingRefStatus::Resolved;
        let result = FileGraphResult {
            pending_refs: vec![resolved],
            ..FileGraphResult::default()
        };
        assert!(file_graph(sample_file_id(), sample_language(), &result).is_err());

        let mut far_away = sample_pending("f7");
        far_away.line = usize::MAX;
        let result = FileGraphResult {
            pending_refs: vec![far_away],
            ..FileGraphResult::default()
        };
        assert!(file_graph(sample_file_id(), sample_language(), &result).is_err());
    }
}
