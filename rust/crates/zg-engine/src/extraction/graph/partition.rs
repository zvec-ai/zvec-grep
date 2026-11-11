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

use crate::domain::{EdgeProvenance, GraphEdgeKind, PreprocessorDirective};

use super::{Constructs, NameEdge, WalkContext, WalkEdge};

/// A name reference resolved against this file's own symbol table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedNameEdge {
    pub ref_kind: GraphEdgeKind,
    pub source_index: usize,
    pub target_index: usize,
    /// Name the reference spells. Inheritance keeps it as evidence because the
    /// resolved target is an entity ID, not the spelling a rewrite repeats.
    pub ref_name: String,
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
    pub ref_kind: GraphEdgeKind,
    /// Entity ordinal owning the reference; [`FILE_SCOPE_INDEX`] for imports.
    pub owner_index: usize,
    pub ref_name: String,
    pub receiver_name: Option<String>,
    /// Original reference text as written in the source, for ref metadata.
    pub raw_text: Option<String>,
    /// Declared type of the bound name for `Declares` edges.
    pub bound_type: Option<String>,
    pub bound_until: Option<usize>,
    /// Exclusive end of a branch/loop-local binding, in source coordinates.
    pub scope_end: Option<(usize, usize)>,
    pub arity: Option<usize>,
    /// Whether the call stands at a construction site, where a type name is a
    /// legitimate target.
    pub constructs: Constructs,
    /// Whether the owner's body already took the name for a local value or
    /// parameter, so no file-level symbol answers it.
    pub shadowed: bool,
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
    /// C/C++ preprocessor directives in source order; empty for every other
    /// language. Partition does not read them — they describe the file, not a
    /// reference — but the pipeline persists them beside the graph so the
    /// resolver can replay the unit a call site belongs to.
    pub directives: Vec<PreprocessorDirective>,
}

/// Function-pointer initializers (`void (*work)(void) = target;`) alias
/// one name to a same-file function per owner: a call spelled through the
/// alias reaches that function rather than a same-named declaration the
/// local hides.
fn alias_table(name_edges: &[NameEdge]) -> HashMap<(usize, String), Vec<NameEdge>> {
    let mut aliases: HashMap<_, Vec<_>> = HashMap::new();
    for edge in name_edges
        .iter()
        .filter(|edge| edge.ref_kind == GraphEdgeKind::Declares)
    {
        aliases
            .entry((edge.owner_index, edge.ref_name.clone()))
            .or_default()
            .push(edge.clone());
    }
    aliases
}

fn alias_at<'a>(
    aliases: &'a HashMap<(usize, String), Vec<NameEdge>>,
    edge: &NameEdge,
) -> Option<&'a str> {
    let position = (edge.line, edge.column);
    let local = aliases
        .get(&(edge.owner_index, edge.ref_name.clone()))
        .and_then(|bindings| {
            bindings.iter().rev().find(|binding| {
                (binding.line, binding.column) <= position
                    && binding.scope_end.is_none_or(|end| position < end)
            })
        });
    local
        .or_else(|| {
            aliases
                .get(&(super::FILE_SCOPE_INDEX, edge.ref_name.clone()))
                .and_then(|bindings| bindings.last())
        })
        .and_then(|binding| binding.bound_type.as_deref())
}

/// Shape used to collapse non-call, non-binding references.
type RefSite = (GraphEdgeKind, usize, Option<String>, String, Option<usize>);

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
        file_symbols,
        scoped_symbols,
        shadowed_names,
        value_aliases,
        preprocessor,
        ..
    } = context;
    let mut partitioned = PartitionedGraph {
        contains: resolved_edges,
        resolved: Vec::new(),
        pending: Vec::new(),
        head_fragment_indices,
        directives: preprocessor,
    };
    let aliases = alias_table(&name_edges);
    let mut collapsed: HashSet<RefSite> = HashSet::new();
    for edge in name_edges {
        if !matches!(
            edge.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Declares
        ) {
            // Preserve each call position and binding declaration. The walk already
            // deduplicates collecting the same call site more than once.
            let site = (
                edge.ref_kind,
                edge.owner_index,
                edge.receiver_name.clone(),
                edge.ref_name.clone(),
                edge.arity,
            );
            if !collapsed.insert(site) {
                continue;
            }
        }
            // A bare name the owner's body already took for itself is not the
            // file's name: `invoke(dispatch: () => void)` reaching `dispatch()`
            // calls the parameter, never an imported function some alias table
            // happens to spell. Only the owner's own tables may answer it, and a
            // name no table can place stays pending instead of falling back.
            let shadowed = edge.receiver_name.is_none()
                && shadowed_names
                    .get(&edge.owner_index)
                    .is_some_and(|names| names.contains(&edge.ref_name));
            let resolved_target = in_file_target(
                &edge,
                shadowed,
                InFileTables {
                    symbols: &symbols,
                    scoped_symbols: &scoped_symbols,
                    file_symbols: &file_symbols,
                    aliases: &aliases,
                    value_aliases: &value_aliases,
                },
            );
            match resolved_target {
                Some(target_index) => partitioned.resolved.push(ResolvedNameEdge {
                    ref_kind: edge.ref_kind,
                    source_index: edge.owner_index,
                    target_index,
                    ref_name: edge.ref_name,
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
                    bound_until: edge.bound_until,
                    scope_end: edge.scope_end,
                    arity: edge.arity,
                    constructs: edge.constructs,
                    shadowed,
                    line: edge.line,
                    column: edge.column,
                }),
            }
    }
    partitioned
}

/// The file's own name tables, as one reference reads them.
#[derive(Clone, Copy)]
struct InFileTables<'a> {
    symbols: &'a HashMap<String, Vec<usize>>,
    scoped_symbols: &'a HashMap<usize, HashMap<String, usize>>,
    file_symbols: &'a HashMap<String, Vec<usize>>,
    aliases: &'a HashMap<(usize, String), Vec<NameEdge>>,
    value_aliases: &'a HashMap<(usize, String), String>,
}

/// The entity one name reference binds to inside its own file, or `None` when
/// the file holds no single answer and the reference stays pending for the
/// cross-file resolver.
fn in_file_target(edge: &NameEdge, shadowed: bool, tables: InFileTables<'_>) -> Option<usize> {
    let InFileTables {
        symbols,
        scoped_symbols,
        file_symbols,
        aliases,
        value_aliases,
    } = tables;
    match edge.ref_kind {
        // Containment is emitted as a resolved edge during the walk, so it
        // normally never reaches this queue; if one does, it stays pending.
        // Imports stay pending because their target is another file by
        // definition, and Declares edges bind a variable to a type name that
        // may live elsewhere — the cross-file resolver consumes all three.
        GraphEdgeKind::Contains | GraphEdgeKind::Imports | GraphEdgeKind::Declares => None,
        GraphEdgeKind::Calls if edge.receiver_name.is_some() => None,
        // A construction site (`new X()`) names a type, and which type a
        // bare name denotes is a namespace question: the file's lone
        // same-named class in an unrelated namespace must not answer it
        // (`ui::Paint`'s `new Widget()` is `ui::Widget`, not
        // `legacy::Widget`). Only the cross-file resolver sees the
        // enclosing namespaces, so constructions stay pending.
        GraphEdgeKind::Calls if edge.constructs.is_some() => None,
        // A bare call resolves through, in order: its owner's alias
        // initializers, the file-level aliases (a module-level
        // `const dispatch = send` is visible to every body in the file),
        // its owner's local name table (the nested spelling an entity
        // declares in its body shadows any file-level name inside it),
        // then the file-level table. Members and nested definitions stay
        // out of that last table: a top-level `caller()` naming `run`
        // does not see `A::run` just because the file declares it once.
        // Without better evidence the reference stays pending for the
        // cross-file resolver, which also weighs imports.
        GraphEdgeKind::Calls => match alias_at(aliases, edge) {
            Some(bound) => symbols
                .get(bound)
                .and_then(|indices| (indices.len() == 1).then(|| indices[0])),
            None => {
                let value_alias = value_aliases
                    .get(&(edge.owner_index, edge.ref_name.clone()))
                    .or_else(|| {
                        value_aliases.get(&(super::FILE_SCOPE_INDEX, edge.ref_name.clone()))
                    });
                match value_alias {
                    // A pair the walk recorded without a position of its own
                    // (`x = y` where the declaration states no type) is the
                    // only evidence there is.
                    Some(bound) => symbols
                        .get(bound)
                        .and_then(|indices| (indices.len() == 1).then(|| indices[0])),
                    None => scoped_symbols
                        .get(&edge.owner_index)
                        .and_then(|locals| locals.get(&edge.ref_name))
                        .copied()
                        .or_else(|| {
                            (!shadowed)
                                .then(|| {
                                    file_symbols
                                        .get(&edge.ref_name)
                                        .and_then(|indices| (indices.len() == 1).then(|| indices[0]))
                                })
                                .flatten()
                        }),
                }
            }
        },
        GraphEdgeKind::Extends | GraphEdgeKind::Implements => symbols
            .get(&edge.ref_name)
            .and_then(|indices| (indices.len() == 1).then(|| indices[0])),
    }
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
                ref_kind: GraphEdgeKind::Calls,
                source_index: 0,
                target_index: 1,
                ref_name: "helper".into(),
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
        assert_eq!(graph.pending[0].ref_kind, GraphEdgeKind::Imports);
        assert_eq!(graph.pending[1].ref_kind, GraphEdgeKind::Calls);
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

    #[test]
    fn file_scope_calls_do_not_bind_to_container_members() {
        // `class A { run() {} } function caller() { run(); }`: `A::run` is a
        // member, not a file-level name. Binding it from a top-level caller
        // would claim an edge the resolver can no longer revisit, and the
        // import of the real `run` would lose its chance to bind.
        let mut context = WalkContext::new();
        let caller = context.next_entity_index();
        context.register_symbol(Some("caller"), caller);
        let container = context.next_entity_index();
        context.register_symbol(Some("A"), container);
        context.push_scope(container);
        let member = context.next_entity_index();
        context.register_symbol(Some("run"), member);
        context.register_scoped_symbol(Some("run"), member);
        context.pop_scope();
        context.add_call_edge(caller, "run", None, None, Some(0), 4, 2);

        let graph = partition_walk(context, vec![0, 1, 2]);
        assert!(
            graph.resolved.is_empty(),
            "a container member must not bind from file scope"
        );
        assert_eq!(graph.pending.len(), 1);
        assert_eq!(graph.pending[0].ref_name, "run");
    }

    #[test]
    fn different_arities_at_one_receiver_are_not_collapsed() {
        // `x.run()` and `x.run(1)` may name different overloads, so the call
        // shape carries the arity rather than collapsing to the first site.
        let mut context = context_with_entities(2);
        context.register_symbol(Some("run"), 1);
        context.add_call_edge(0, "run", Some("x"), None, Some(0), 5, 2);
        context.add_call_edge(0, "run", Some("x"), None, Some(1), 5, 8);

        let graph = partition_walk(context, vec![0, 1]);
        assert!(graph.resolved.is_empty());
        assert_eq!(graph.pending.len(), 2);
        assert_eq!(graph.pending[0].arity, Some(0));
        assert_eq!(graph.pending[1].arity, Some(1));
    }
}
