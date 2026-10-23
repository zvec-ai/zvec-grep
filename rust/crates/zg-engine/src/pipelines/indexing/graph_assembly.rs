//! Pipeline-side assembly of the per-file graph result.
//!
//! The extraction layer partitions the walk output in entity-ordinal space
//! (see `extraction::graph::partition`); this module maps those ordinals onto
//! the `EntityId`s the pipeline mints during fragment binding and assembles
//! the final [`FileGraphResult`] for the graph persistence layer.
//!
//! Nodes are built from the bound entities themselves: the pipeline owns the
//! ID mint, so the extraction layer never needed the file ID.

use std::collections::BTreeMap;

use crate::domain::{
    EdgeProvenance, Entity, EntityMetadata, FileEdge, FileFormat, FileGraphNode, FileGraphResult,
    GraphEdgeKind, PendingRef, PendingRefStatus, Range, SymbolType, TextRange,
};
use crate::extraction::{FILE_SCOPE_INDEX, PartitionedGraph};
use crate::utils::sha256_hex;

/// File-node ID marking the file scope itself (import owner).
pub(super) fn file_node_id(file_id: &str) -> String {
    sha256_hex(format!("{file_id}\0file").as_bytes())
}

/// Assembles the final `FileGraphResult` for a prepared file.
///
/// `entities` must be the same bound values produced from the extraction
/// output that generated `graph` (their ordinals still match).
pub(super) fn assemble_file_graph(
    file_id: &str,
    language: FileFormat,
    graph: &PartitionedGraph,
    entities: &[Entity],
) -> FileGraphResult {
    let file_scope = file_node_id(file_id);
    let entity_nodes = graph
        .head_fragment_indices
        .iter()
        .enumerate()
        .filter_map(|(ordinal, _head)| {
            entities
                .get(ordinal)
                .map(|entity| entity_node(entity, language))
        })
        .collect::<Vec<_>>();

    let mut edges = Vec::with_capacity(graph.contains.len() + graph.resolved.len());
    let node_ids = |ordinal: usize| -> Option<String> {
        entities
            .get(ordinal)
            .map(|entity| entity.id().as_str().to_owned())
    };
    for edge in &graph.contains {
        if let (Some(source), Some(target)) =
            (node_ids(edge.source_index), node_ids(edge.target_index))
        {
            edges.push(FileEdge {
                kind: GraphEdgeKind::Contains,
                source,
                target,
                line: Some(edge.line),
                column: Some(edge.column),
                provenance: EdgeProvenance::FileLocal,
                metadata: BTreeMap::new(),
            });
        }
    }
    for edge in &graph.resolved {
        if let (Some(source), Some(target)) =
            (node_ids(edge.source_index), node_ids(edge.target_index))
        {
            let mut metadata = BTreeMap::new();
            insert_ref_metadata(
                &mut metadata,
                edge.raw_text.as_deref(),
                edge.arity,
                edge.receiver_name.as_deref(),
            );
            edges.push(FileEdge {
                kind: edge.ref_kind.into(),
                source,
                target,
                line: Some(edge.line),
                column: Some(edge.column),
                provenance: edge.provenance,
                metadata,
            });
        }
    }

    let pending = graph
        .pending
        .iter()
        .map(|reference| {
            let owner_id = match reference.owner_index {
                FILE_SCOPE_INDEX => file_scope.clone(),
                ordinal => node_ids(ordinal).unwrap_or_else(|| file_scope.clone()),
            };
            let mut metadata = BTreeMap::new();
            insert_ref_metadata(
                &mut metadata,
                reference.raw_text.as_deref(),
                reference.arity,
                reference.receiver_name.as_deref(),
            );
            if let Some(bound_type) = &reference.bound_type {
                metadata.insert("type".to_owned(), serde_json::json!(bound_type));
            }
            PendingRef {
                owner_id,
                ref_name: reference.ref_name.clone(),
                receiver_name: reference.receiver_name.clone(),
                ref_kind: reference.ref_kind.into(),
                arity: reference.arity,
                line: reference.line,
                column: reference.column,
                status: PendingRefStatus::Pending,
                metadata,
            }
        })
        .collect();

    FileGraphResult {
        nodes: entity_nodes,
        edges,
        pending_refs: pending,
    }
}

/// Builds a graph node from a bound entity. Node identity is the entity ID
/// string; qualified names join the breadcrumb with `::`.
fn entity_node(entity: &Entity, language: FileFormat) -> FileGraphNode {
    let (
        name,
        kind,
        qualified_name,
        signature,
        doc,
        visibility,
        arity,
        is_exported,
        is_declaration,
    ) = match entity.metadata() {
        Some(EntityMetadata::Code(code)) => {
            let name = code.symbol_name.clone();
            let qualified_name = match (&name, &code.scope) {
                (Some(name), Some(scope)) => format!("{scope}::{name}"),
                (Some(name), None) => name.clone(),
                (None, _) => String::new(),
            };
            (
                name,
                code.symbol_type.unwrap_or(SymbolType::Value),
                qualified_name,
                code.signature.clone(),
                code.documentation.clone(),
                code.visibility,
                code.arity,
                code.is_exported,
                code.is_declaration,
            )
        }
        _ => (
            None,
            SymbolType::Value,
            String::new(),
            None,
            None,
            None,
            None,
            false,
            false,
        ),
    };
    let range = match entity.source_range() {
        Range::Text(range) => Some(range),
        _ => None,
    };
    FileGraphNode {
        id: entity.id().as_str().to_owned(),
        kind,
        name,
        qualified_name,
        language: language.as_str().to_owned(),
        start_line: range.as_ref().map_or(0, TextRange::start_line),
        end_line: range.as_ref().map_or(0, TextRange::end_line),
        start_column: range.as_ref().map_or(0, TextRange::start_byte_column),
        end_column: range.as_ref().map_or(0, TextRange::end_byte_column),
        signature,
        doc,
        arity,
        visibility: visibility.map(|visibility| visibility.as_str().to_owned()),
        is_exported,
        is_declaration,
    }
}

fn insert_ref_metadata(
    metadata: &mut BTreeMap<String, serde_json::Value>,
    raw_text: Option<&str>,
    arity: Option<usize>,
    receiver_name: Option<&str>,
) {
    if let Some(raw_text) = raw_text {
        metadata.insert(
            "rawText".to_owned(),
            serde_json::Value::String(raw_text.to_owned()),
        );
    }
    if let Some(arity) = arity {
        metadata.insert("arity".to_owned(), serde_json::json!(arity));
    }
    if let Some(receiver_name) = receiver_name {
        metadata.insert(
            "receiverName".to_owned(),
            serde_json::Value::String(receiver_name.to_owned()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests exercise the file-node ID mint and metadata assembly; full graph
    // assembly needs a prepared file, covered by the pipeline integration
    // tests once the storage sink lands.
    #[test]
    fn file_node_id_is_stable_and_distinct() {
        let a = file_node_id("f1");
        let b = file_node_id("f1");
        let c = file_node_id("f2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn metadata_carries_optional_evidence() {
        let mut metadata = BTreeMap::new();
        insert_ref_metadata(&mut metadata, Some("dog.bark()"), Some(2), Some("dog"));
        assert_eq!(metadata.len(), 3);
        assert_eq!(metadata["rawText"], serde_json::json!("dog.bark()"));
        assert_eq!(metadata["arity"], serde_json::json!(2));
        assert_eq!(metadata["receiverName"], serde_json::json!("dog"));

        let mut empty = BTreeMap::new();
        insert_ref_metadata(&mut empty, None, None, None);
        assert!(empty.is_empty());
    }
}
