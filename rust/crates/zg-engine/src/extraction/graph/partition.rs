//! Post-walk partition of buffered graph references.
//!
//! Runs after the walk completes, when the file's symbol table is complete, so
//! forward references resolve as readily as backward ones. Each buffered name
//! reference is either:
//!
//! * resolved in-file → a [`ResolvedNameEdge`] carrying the target's entity
//!   ordinal (provenance `file_local`), or
//! * unresolvable here → a [`PendingNameRef`] for the cross-file resolver
//!   (module paths, external symbols, genuinely missing definitions).
//!
//! Output stays in entity-ordinal space: the pipeline-side assembly maps each
//! ordinal onto its head fragment index and then onto the minted `EntityId`.
//! Mirrors the TypeScript partition step over the walk context.

use std::collections::{HashMap, HashSet};

use crate::domain::{EdgeProvenance, GraphRefKind};

use super::{WalkContext, WalkEdge};

/// A name reference resolved against this file's own symbol table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedNameEdge {
    pub ref_kind: GraphRefKind,
    pub source_index: usize,
    pub target_index: usize,
    /// Original reference text as written in the source, for edge metadata.
    pub raw_text: Option<String>,
    pub arity: Option<usize>,
    pub receiver_name: Option<String>,
    pub line: usize,
    pub column: usize,
    pub provenance: EdgeProvenance,
}

/// A name reference that left this file unresolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingNameRef {
    pub ref_kind: GraphRefKind,
    /// Entity ordinal owning the reference; [`FILE_SCOPE_INDEX`] for imports.
    pub owner_index: usize,
    pub ref_name: String,
    pub receiver_name: Option<String>,
    /// Original reference text as written in the source, for ref metadata.
    pub raw_text: Option<String>,
    /// Declared type of the bound name for `Declares` edges.
    pub bound_type: Option<String>,
    pub arity: Option<usize>,
    pub line: usize,
    pub column: usize,
}

/// Partitioned walk output, still in entity-ordinal space.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PartitionedGraph {
    /// Walk-time `contains` edges (`contains` never partitions).
    pub contains: Vec<WalkEdge>,
    /// Name references resolved within this file.
    pub resolved: Vec<ResolvedNameEdge>,
    /// Name references deferred to the cross-file resolver.
    pub pending: Vec<PendingNameRef>,
    /// Head fragment index per entity ordinal: the extraction appends each
    /// entity's fragments contiguously in walk order, so the head index maps
    /// an ordinal onto the entity's representative fragment — whose index the
    /// pipeline turns into the minted `EntityId`.
    pub head_fragment_indices: Vec<usize>,
}

/// Partitions the walk buffers against the completed symbol table.
///
/// Resolution policy, mirroring the TypeScript original: a bare `ref_name` hit
/// resolves only when the file declares the name exactly once (an ambiguous
/// hit stays pending — the cross-file resolver has better evidence, e.g.
/// imports). Receiver-qualified calls never resolve in-file from the short
/// name alone: the receiver's type is unknown at walk time, and any same-file
/// `receiver.method` pair would be coincidental, so they stay pending.
/// Imports never resolve in-file: their target is another file by definition.
pub(crate) fn partition_walk(
    context: WalkContext,
    head_fragment_indices: Vec<usize>,
) -> PartitionedGraph {
    let WalkContext {
        resolved_edges,
        name_edges,
        symbols,
        scoped_symbols,
        ..
    } = context;
    let mut partitioned = PartitionedGraph {
        contains: resolved_edges,
        resolved: Vec::new(),
        pending: Vec::new(),
        head_fragment_indices,
    };
    // Function-pointer initializers (`void (*work)(void) = target;`) alias
    // one name to a same-file function per owner: a call spelled through the
    // alias reaches that function rather than a same-named declaration the
    // local hides.
    let aliases: HashMap<(usize, String), String> = name_edges
        .iter()
        .filter(|edge| edge.ref_kind == GraphRefKind::Declares)
        .filter_map(|edge| {
            edge.bound_type
                .clone()
                .map(|bound| ((edge.owner_index, edge.ref_name.clone()), bound))
        })
        .collect();
    // Receivers whose owner binds them to more than one type (`x = new X()`
    // ... later `x = new Y()`) keep their call sites apart: only per-site
    // evidence lets the resolver pick the binding in force at each call.
    // Every other receiver resolves identically from any of its sites, so
    // repeated spellings of one call shape collapse to the first.
    let mut binding_types: HashMap<(usize, String), HashSet<&str>> = HashMap::new();
    for edge in name_edges
        .iter()
        .filter(|edge| edge.ref_kind == GraphRefKind::Declares)
    {
        if let Some(bound) = edge.bound_type.as_deref() {
            binding_types
                .entry((edge.owner_index, edge.ref_name.clone()))
                .or_default()
                .insert(bound);
        }
    }
    let rebinding_receivers: HashSet<(usize, String)> = binding_types
        .into_iter()
        .filter(|(_, types)| types.len() > 1)
        .map(|(key, _)| key)
        .collect();
    let mut collapsed: HashSet<(GraphRefKind, usize, Option<String>, String)> = HashSet::new();
    for edge in name_edges {
        let rebinding_call = edge.ref_kind == GraphRefKind::Calls
            && edge.receiver_name.as_deref().is_some_and(|receiver| {
                rebinding_receivers.contains(&(edge.owner_index, receiver.to_owned()))
            });
        if !rebinding_call && edge.ref_kind != GraphRefKind::Declares {
            // Binding rows never collapse: the resolver's binding table
            // orders them by line, so a rebinding row is load-bearing even
            // when an earlier row binds the same name.
            let site = (
                edge.ref_kind,
                edge.owner_index,
                edge.receiver_name.clone(),
                edge.ref_name.clone(),
            );
            if !collapsed.insert(site) {
                continue;
            }
        }
        let resolved_target = match edge.ref_kind {
            GraphRefKind::Imports => None,
            // Declares edges bind a variable to its type name; the name is
            // not an entity in this file's symbol table, and the type entity
            // may live elsewhere — the cross-file resolver consumes them.
            GraphRefKind::Declares => None,
            GraphRefKind::Calls if edge.receiver_name.is_some() => None,
            // A bare call resolves through, in order: its owner's alias
            // initializers, its owner's local name table (the nested
            // spelling an entity declares in its body shadows any
            // file-level name inside it), then the file-level table.
            GraphRefKind::Calls => aliases
                .get(&(edge.owner_index, edge.ref_name.clone()))
                .and_then(|bound| {
                    symbols
                        .get(bound)
                        .and_then(|indices| (indices.len() == 1).then(|| indices[0]))
                })
                .or_else(|| {
                    scoped_symbols
                        .get(&edge.owner_index)
                        .and_then(|locals| locals.get(&edge.ref_name))
                        .copied()
                })
                .or_else(|| {
                    symbols
                        .get(&edge.ref_name)
                        .and_then(|indices| (indices.len() == 1).then(|| indices[0]))
                }),
            GraphRefKind::Extends | GraphRefKind::Implements => symbols
                .get(&edge.ref_name)
                .and_then(|indices| (indices.len() == 1).then(|| indices[0])),
        };
        match resolved_target {
            Some(target_index) => partitioned.resolved.push(ResolvedNameEdge {
                ref_kind: edge.ref_kind,
                source_index: edge.owner_index,
                target_index,
                raw_text: edge.raw_text,
                arity: edge.arity,
                receiver_name: edge.receiver_name,
                line: edge.line,
                column: edge.column,
                provenance: EdgeProvenance::FileLocal,
            }),
            None => partitioned.pending.push(PendingNameRef {
                ref_kind: edge.ref_kind,
                owner_index: edge.owner_index,
                ref_name: edge.ref_name,
                receiver_name: edge.receiver_name,
                raw_text: edge.raw_text,
                bound_type: edge.bound_type,
                arity: edge.arity,
                line: edge.line,
                column: edge.column,
            }),
        }
    }
    partitioned
}

#[cfg(test)]
mod tests {
    use super::super::FILE_SCOPE_INDEX;
    use super::*;

    fn context_with_entities(count: usize) -> WalkContext {
        let mut context = WalkContext::new();
        for _ in 0..count {
            context.next_entity_index();
        }
        context
    }

    #[test]
    fn resolves_unique_in_file_symbols() {
        let mut context = context_with_entities(2);
        context.register_symbol(Some("helper"), 1);
        context.add_call_edge(0, "helper", None, None, Some(0), 5, 2);
        let graph = partition_walk(context, vec![0, 1]);
        assert_eq!(
            graph.resolved,
            vec![ResolvedNameEdge {
                ref_kind: GraphRefKind::Calls,
                source_index: 0,
                target_index: 1,
                raw_text: None,
                arity: Some(0),
                receiver_name: None,
                line: 5,
                column: 2,
                provenance: EdgeProvenance::FileLocal,
            }]
        );
        assert!(graph.pending.is_empty());
    }

    #[test]
    fn forward_references_resolve_after_the_walk() {
        // The definition (ordinal 1) appears after the call site textually;
        // partition runs post-walk, so it resolves anyway.
        let mut context = context_with_entities(2);
        context.register_symbol(Some("later"), 1);
        context.add_call_edge(0, "later", None, None, None, 2, 0);
        let graph = partition_walk(context, vec![0, 1]);
        assert_eq!(graph.resolved.len(), 1);
        assert_eq!(graph.resolved[0].target_index, 1);
    }

    #[test]
    fn ambiguous_symbols_stay_pending() {
        // Two entities declare `helper`; picking one locally would be a guess.
        let mut context = context_with_entities(3);
        context.register_symbol(Some("helper"), 1);
        context.register_symbol(Some("helper"), 2);
        context.add_call_edge(0, "helper", None, None, None, 4, 0);
        let graph = partition_walk(context, vec![0, 1, 2]);
        assert!(graph.resolved.is_empty());
        assert_eq!(graph.pending.len(), 1);
        assert_eq!(graph.pending[0].ref_name, "helper");
    }

    #[test]
    fn receiver_qualified_calls_stay_pending() {
        let mut context = context_with_entities(2);
        context.register_symbol(Some("bark"), 1);
        context.add_call_edge(0, "bark", Some("dog"), None, None, 6, 4);
        let graph = partition_walk(context, vec![0, 1]);
        assert!(graph.resolved.is_empty());
        assert_eq!(graph.pending.len(), 1);
        assert_eq!(graph.pending[0].receiver_name.as_deref(), Some("dog"));
    }

    #[test]
    fn imports_and_missing_names_stay_pending() {
        let mut context = context_with_entities(1);
        context.add_import_edge("./utils.js", None, 1, 0);
        context.add_call_edge(0, "nowhere", None, None, None, 3, 8);
        let graph = partition_walk(context, vec![0]);
        assert!(graph.resolved.is_empty());
        assert_eq!(graph.pending.len(), 2);
        assert_eq!(graph.pending[0].owner_index, FILE_SCOPE_INDEX);
        assert_eq!(graph.pending[0].ref_kind, GraphRefKind::Imports);
        assert_eq!(graph.pending[1].ref_kind, GraphRefKind::Calls);
    }

    #[test]
    fn contains_edges_pass_through_unpartitioned() {
        let mut context = WalkContext::new();
        let outer = context.next_entity_index();
        context.push_scope(outer);
        let inner = context.next_entity_index();
        context.add_contains_edge(inner, 8, 2);
        context.pop_scope();
        let graph = partition_walk(context, vec![0, 1]);
        assert_eq!(
            graph.contains,
            vec![WalkEdge {
                source_index: outer,
                target_index: inner,
                line: 8,
                column: 2
            }]
        );
        assert_eq!(graph.head_fragment_indices, vec![0, 1]);
    }
}
