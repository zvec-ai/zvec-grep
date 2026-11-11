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
    GraphEdgeKind, PendingRef, PendingRefStatus, Range, SymbolType,
};
use crate::extraction::{FILE_SCOPE_INDEX, PartitionedGraph};

/// File-node ID marking the file scope itself (import owner).
///
/// Matches the implicit file endpoint the graph store writes (`f{file_id}`),
/// so pending references owned by the file pass the store's ownership check.
pub(super) fn file_node_id(file_id: &str) -> String {
    format!("f{file_id}")
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
    FileGraphResult {
        nodes: entity_nodes,
        edges: file_edges(graph, entities, &file_scope),
        pending_refs: file_pending_refs(graph, entities, &file_scope),
    }
}

/// The resolved edges of one file: walk-time containment plus name references
/// the partition already bound inside it.
fn file_edges(graph: &PartitionedGraph, entities: &[Entity], file_scope: &str) -> Vec<FileEdge> {
    let mut edges = Vec::with_capacity(graph.contains.len() + graph.resolved.len());
    for edge in &graph.contains {
        if let (Some(source), Some(target)) = (
            node_id(entities, edge.source_index),
            node_id(entities, edge.target_index),
        ) {
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
        if let (Some(source), Some(target)) = (
            node_id(entities, edge.source_index),
            node_id(entities, edge.target_index),
        ) {
            let mut metadata = BTreeMap::new();
            insert_ref_metadata(
                &mut metadata,
                edge.raw_text.as_deref(),
                edge.arity,
                edge.receiver_name.as_deref(),
            );
            if edge.ref_kind.spells_inheritance() {
                insert_inheritance_metadata(
                    &mut metadata,
                    &owner_identity(edge.source_index, entities, file_scope),
                    Some(&edge.ref_name),
                );
            }
            edges.push(FileEdge {
                kind: edge.ref_kind,
                source,
                target,
                line: Some(edge.line),
                column: Some(edge.column),
                provenance: edge.provenance,
                metadata,
            });
        }
    }
    edges
}

/// The references one file leaves for the cross-file resolver.
fn file_pending_refs(
    graph: &PartitionedGraph,
    entities: &[Entity],
    file_scope: &str,
) -> Vec<PendingRef> {
    graph
        .pending
        .iter()
        .map(|reference| {
            let owner_id = match reference.owner_index {
                FILE_SCOPE_INDEX => file_scope.to_owned(),
                ordinal => node_id(entities, ordinal).unwrap_or_else(|| file_scope.to_owned()),
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
            if reference.constructs {
                // Only construction sites carry the flag: the resolver keeps
                // type candidates for exactly these calls.
                metadata.insert("constructs".to_owned(), serde_json::json!(true));
            }
            if reference.ref_kind.spells_inheritance() {
                insert_inheritance_metadata(
                    &mut metadata,
                    &owner_identity(reference.owner_index, entities, file_scope),
                    None,
                );
            }
            PendingRef {
                owner_id,
                ref_name: reference.ref_name.clone(),
                receiver_name: reference.receiver_name.clone(),
                ref_kind: reference.ref_kind,
                arity: reference.arity,
                line: reference.line,
                column: reference.column,
                status: PendingRefStatus::Pending,
                metadata,
            }
        })
        .collect()
}

fn node_id(entities: &[Entity], ordinal: usize) -> Option<String> {
    entities
        .get(ordinal)
        .map(|entity| entity.id().as_str().to_owned())
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
        range,
        signature,
        doc,
        arity,
        visibility: visibility.map(|visibility| visibility.as_str().to_owned()),
        is_exported,
        is_declaration,
    }
}

/// Metadata keys an inheritance clause carries for the write-time comparison.
/// Entity IDs hash the entity's text, so a method-body edit mints a new ID
/// while the clause itself, its owner's name and its base stay put. The store
/// therefore compares the clause's declaration identity, not its endpoints.
const INHERITANCE_OWNER_KEY: &str = "owner";
const INHERITANCE_BASE_KEY: &str = "base";

fn insert_inheritance_metadata(
    metadata: &mut BTreeMap<String, serde_json::Value>,
    owner: &str,
    base: Option<&str>,
) {
    metadata.insert(
        INHERITANCE_OWNER_KEY.to_owned(),
        serde_json::Value::String(owner.to_owned()),
    );
    if let Some(base) = base {
        metadata.insert(
            INHERITANCE_BASE_KEY.to_owned(),
            serde_json::Value::String(base.to_owned()),
        );
    }
}

/// Qualified name of the entity owning a name reference; the file scope owns
/// nothing inheritable, so it keeps its synthetic file-node ID.
fn owner_identity(ordinal: usize, entities: &[Entity], file_scope: &str) -> String {
    if ordinal == FILE_SCOPE_INDEX {
        return file_scope.to_owned();
    }
    entities
        .get(ordinal)
        .and_then(qualified_name)
        .unwrap_or_else(|| file_scope.to_owned())
}

fn qualified_name(entity: &Entity) -> Option<String> {
    let Some(EntityMetadata::Code(code)) = entity.metadata() else {
        return None;
    };
    let name = code.symbol_name.as_ref()?;
    Some(match &code.scope {
        Some(scope) => format!("{scope}::{name}"),
        None => name.clone(),
    })
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
    use crate::domain::{
        CodeMetadata, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileId,
        FragmentId, Range, SymbolType,
    };
    use crate::extraction::{PendingNameRef, ResolvedNameEdge};

    fn code_entity(id: &str, name: &str, scope: Option<&str>) -> Entity {
        let file_id = FileId::new(7);
        let entity_id = EntityId::from_string(id.to_owned());
        Entity::new(
            entity_id.clone(),
            file_id,
            Range::Full,
            Content::Text(format!("class {name}")),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Class),
                symbol_name: Some(name.to_owned()),
                scope: scope.map(str::to_owned),
                ..CodeMetadata::default()
            })),
            vec![EntityFragment {
                id: FragmentId::new(&entity_id, 0),
                range: Range::Full,
            }],
        )
        .expect("entity")
    }

    #[test]
    fn inheritance_clauses_record_their_declaration_identity() {
        let entities = vec![
            code_entity("e1", "Mid", Some("ns")),
            code_entity("e2", "Base", Some("ns")),
        ];
        let mut graph = PartitionedGraph {
            head_fragment_indices: vec![0, 1],
            ..PartitionedGraph::default()
        };
        graph.resolved.push(ResolvedNameEdge {
            ref_kind: GraphEdgeKind::Extends,
            source_index: 0,
            target_index: 1,
            ref_name: "Base".into(),
            raw_text: Some("ns::Base".into()),
            arity: None,
            receiver_name: None,
            line: 1,
            column: 12,
            provenance: EdgeProvenance::FileLocal,
        });
        graph.pending.push(PendingNameRef {
            ref_kind: GraphEdgeKind::Implements,
            owner_index: 0,
            ref_name: "Remote".into(),
            receiver_name: None,
            raw_text: Some("Remote".into()),
            bound_type: None,
            arity: None,
            constructs: false,
            line: 1,
            column: 20,
        });

        let assembled = assemble_file_graph("7", FileFormat::Rust, &graph, &entities);
        let inheritance = assembled
            .edges
            .iter()
            .find(|edge| edge.kind.spells_inheritance())
            .expect("in-file inheritance edge");
        assert_eq!(
            inheritance.metadata[INHERITANCE_OWNER_KEY],
            serde_json::json!("ns::Mid")
        );
        assert_eq!(
            inheritance.metadata[INHERITANCE_BASE_KEY],
            serde_json::json!("Base")
        );
        assert_eq!(
            assembled.pending_refs[0].metadata[INHERITANCE_OWNER_KEY],
            serde_json::json!("ns::Mid")
        );
    }

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
