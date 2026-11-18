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
    /// Last line the bound name stays in effect, for a loop-scoped binding.
    pub bound_until: Option<usize>,
    /// Exclusive end of a branch/loop-local binding, in source coordinates.
    pub scope_end: Option<(usize, usize)>,
    pub arity: Option<usize>,
    /// Whether the call stands at a construction site, where a type name is a
    /// legitimate target.
    pub constructs: Constructs,
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

/// Declarations binding one name to another, per owner: a function-pointer
/// initializer (`void (*work)(void) = target;` records `target` as the bound
/// *type*) and a value initializer (`const dispatch = send` records `send`).
/// A call spelled through the bound name reaches the named function rather than
/// a same-named declaration the local hides — but only as far as the binding
/// that is in effect where the call stands, so the bindings stay in source
/// order instead of collapsing into one answer per name.
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

/// The name a reference spelled through `edge.ref_name` reaches at its own
/// position: the last local binding written before it that has not gone out of
/// scope, and failing that the file's own declaration, which every body of the
/// file can see. Position is the whole point — a `const target = other` inside
/// a block stops answering when the block ends, and a later declaration must
/// not reach backwards past an earlier one.
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

/// Whether a binding states a value this file can place. `?` marks a
/// declaration whose value is unreadable here — a parameter, or an initializer
/// that is neither a construction nor a factory call — and `typeof X` marks a
/// callable copy. Binding the spelled name instead would claim the file's
/// namesake even where a parameter or a nested definition of that name is the
/// value, while the cross-file resolver still reads the declared binding at the
/// call's position and can tell the two apart.
fn states_a_placeable_value(bound: &str) -> bool {
    !(bound == "?" || bound.starts_with("typeof "))
}

/// The entity the file declares exactly once under `name`; two same-named
/// declarations are not evidence for either.
fn unique_symbol(symbols: &HashMap<String, Vec<usize>>, name: &str) -> Option<usize> {
    symbols
        .get(name)
        .and_then(|indices| (indices.len() == 1).then(|| indices[0]))
}

/// Shape used to collapse references without per-site binding evidence.
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
        module_scopes,
        function_scopes,
        import_bindings,
        shadowed_names,
        borrowed_names,
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
    // Containment is the only record of which entity sits inside which, so it
    // is what lets a call read the using-declarations of the classes above it.
    let enclosing = partitioned
        .contains
        .iter()
        .map(|edge| (edge.target_index, edge.source_index))
        .collect::<HashMap<usize, usize>>();
    let aliases = alias_table(&name_edges);
    let mut collapsed: HashSet<RefSite> = HashSet::new();
    for edge in name_edges {
        if !matches!(
            edge.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Declares | GraphEdgeKind::Imports
        ) {
            // Preserve call sites, declarations and import statements: two imports
            // of the same module can bind different local names in their raw text.
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
        let resolved_target = in_file_target(
            &edge,
            shadowed_at_site(&edge, &enclosing, &shadowed_names, &borrowed_names),
            InFileTables {
                symbols: &symbols,
                scoped_symbols: &scoped_symbols,
                module_scopes: &module_scopes,
                function_scopes: &function_scopes,
                import_bindings: &import_bindings,
                file_symbols: &file_symbols,
                aliases: &aliases,
                value_aliases: &value_aliases,
                enclosing: &enclosing,
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
                line: edge.line,
                column: edge.column,
            }),
        }
    }
    partitioned
}

/// Whether the name a call spells is already taken nearer than the file's own
/// table, so no enclosing item scope may answer it.
///
/// A bare name the owner's body took for itself is not the file's name:
/// `invoke(dispatch: () => void)` reaching `dispatch()` calls the parameter,
/// never an imported function some alias table happens to spell. A name one of
/// the classes around the owner imports (`using First<T>::Run;`) is that
/// class's member rather than the file's namesake, and the cross-file resolver
/// decides which member it reaches.
fn shadowed_at_site(
    edge: &NameEdge,
    enclosing: &HashMap<usize, usize>,
    shadowed_names: &HashMap<usize, HashSet<String>>,
    borrowed_names: &HashMap<usize, HashSet<String>>,
) -> bool {
    if edge.receiver_name.is_some() {
        return false;
    }
    shadowed_names
        .get(&edge.owner_index)
        .is_some_and(|names| names.contains(&edge.ref_name))
        || borrowed_by_enclosing_class(edge.owner_index, &edge.ref_name, enclosing, borrowed_names)
}

/// Whether one of the entities around `owner` imports `name` from another
/// scope: a class body's `using First<T>::Run;` answers `Run()` in the
/// methods written in that class, so the file's own namesake is not the call.
fn borrowed_by_enclosing_class(
    owner: usize,
    name: &str,
    enclosing: &HashMap<usize, usize>,
    borrowed_names: &HashMap<usize, HashSet<String>>,
) -> bool {
    let mut current = owner;
    // The file node is not an entity, so the walk ends where containment does.
    for _ in 0..64 {
        let Some(parent) = enclosing.get(&current) else {
            return false;
        };
        if borrowed_names
            .get(parent)
            .is_some_and(|names| names.contains(name))
        {
            return true;
        }
        current = *parent;
    }
    false
}

/// The entity a bare call reaches among the ones one body declares under one
/// spelling. Rust keeps the type and value namespaces apart, so a later
/// `mod save` never takes the call away from `fn save`: a module is not a
/// value, and a spelling only a module holds leaves the call pending.
fn local_target(declared: &[usize], module_scopes: &HashSet<usize>) -> Option<usize> {
    declared
        .iter()
        .rev()
        .copied()
        .find(|index| !module_scopes.contains(index))
}

/// What the item scopes around one bare call say about the name it spells.
enum BareName {
    /// No enclosing body or module binds the name, so the file's own table and
    /// the cross-file resolver still get their say.
    Unbound,
    /// A `use` written in one of the enclosing bodies binds the name: the call
    /// reaches what that import names, which only the resolver can read.
    Imported,
    /// An item scope declares the spelling. `Some` is the entity a bare call
    /// reaches; `None` means only a module holds it — a module is not a value —
    /// so the call stays pending instead of climbing to an outer scope.
    Declared(Option<usize>),
}

/// What the modules and function bodies around `owner` say about `name`.
///
/// Bare scope holds a module's items for every body written inside it, and a
/// function's items for the bodies nested in it: `mod storage` answers its
/// `flush`, and `fn outer` answers the `save()` its nested `inner` writes. An
/// outer module never answers — reaching it needs `super::` — and an `impl`, a
/// trait or a class body is skipped, because its members need a receiver. A
/// `use` written in one of those bodies is nearer than the items: it binds the
/// spelling for the calls that see it, and where it points is the resolver's
/// question.
fn enclosing_bare_name(
    owner: usize,
    name: &str,
    enclosing: &HashMap<usize, usize>,
    module_scopes: &HashSet<usize>,
    function_scopes: &HashSet<usize>,
    scoped_symbols: &HashMap<usize, HashMap<String, Vec<usize>>>,
    import_bindings: &HashMap<usize, HashSet<String>>,
) -> BareName {
    let mut current = owner;
    // The file node is not an entity, so the walk ends where containment does.
    for _ in 0..64 {
        if import_bindings
            .get(&current)
            .is_some_and(|names| names.contains(name))
        {
            return BareName::Imported;
        }
        let module = module_scopes.contains(&current);
        if module || function_scopes.contains(&current) {
            if let Some(declared) = scoped_symbols
                .get(&current)
                .and_then(|names| names.get(name))
            {
                return BareName::Declared(local_target(declared, module_scopes));
            }
            if module {
                // The nearest module ends the walk: the items of the modules
                // around it need `super::`. The file's own table is still read,
                // because a glob import (`use super::*;`) states what an inner
                // module sees of it.
                return BareName::Unbound;
            }
        }
        let Some(&parent) = enclosing.get(&current) else {
            return BareName::Unbound;
        };
        current = parent;
    }
    BareName::Unbound
}

/// The file's own name tables, as one reference reads them.
#[derive(Clone, Copy)]
struct InFileTables<'a> {
    symbols: &'a HashMap<String, Vec<usize>>,
    scoped_symbols: &'a HashMap<usize, HashMap<String, Vec<usize>>>,
    module_scopes: &'a HashSet<usize>,
    function_scopes: &'a HashSet<usize>,
    import_bindings: &'a HashMap<usize, HashSet<String>>,
    file_symbols: &'a HashMap<String, Vec<usize>>,
    aliases: &'a HashMap<(usize, String), Vec<NameEdge>>,
    value_aliases: &'a HashMap<(usize, String), String>,
    /// Containment, target ordinal → containing ordinal: the chain a module
    /// lookup climbs.
    enclosing: &'a HashMap<usize, usize>,
}

/// The entity one name reference binds to inside its own file, or `None` when
/// the file holds no single answer and the reference stays pending for the
/// cross-file resolver.
fn in_file_target(edge: &NameEdge, shadowed: bool, tables: InFileTables<'_>) -> Option<usize> {
    let InFileTables {
        symbols,
        scoped_symbols,
        module_scopes,
        function_scopes,
        import_bindings,
        file_symbols,
        aliases,
        value_aliases,
        enclosing,
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
        // A bare call resolves through, in order: the binding its owner's body
        // gave the name *at the call's own position*, the file-level bindings
        // (a module-level `const dispatch = send` is visible to every body in
        // the file, whatever its order), its owner's local name table (the
        // nested spelling an entity declares in its body shadows any file-level
        // name inside it), then the item scopes around the owner — the module's
        // own `save` answers its `flush`, and a nested `fn save` answers the
        // bodies inside the function that declares it — and finally the
        // file-level table. Members and nested definitions stay out of that
        // last table: a top-level `caller()` naming `run` does not see `A::run`
        // just because the file declares it once. Without better evidence the
        // reference stays pending for the cross-file resolver, which also
        // weighs imports.
        GraphEdgeKind::Calls => match alias_at(aliases, edge) {
            // The binding states no value this file can place, so the spelled
            // name is not the answer: the resolver reads the declared binding
            // at the call's position and can still tell a parameter, a nested
            // definition or a later reassignment apart from the file's name.
            Some(bound) if !states_a_placeable_value(bound) => None,
            Some(bound) => unique_symbol(symbols, bound),
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
                    Some(bound) => unique_symbol(symbols, bound),
                    None => scoped_symbols
                        .get(&edge.owner_index)
                        .and_then(|locals| locals.get(&edge.ref_name))
                        .and_then(|declared| local_target(declared, module_scopes))
                        .or_else(|| {
                            // A parameter or an untyped local of that name in
                            // the owner's own body is the value the call
                            // reaches; no enclosing scope may answer it.
                            if shadowed {
                                return None;
                            }
                            match enclosing_bare_name(
                                edge.owner_index,
                                &edge.ref_name,
                                enclosing,
                                module_scopes,
                                function_scopes,
                                scoped_symbols,
                                import_bindings,
                            ) {
                                BareName::Declared(target) => target,
                                BareName::Imported => None,
                                BareName::Unbound => unique_symbol(file_symbols, &edge.ref_name),
                            }
                        }),
                }
            }
        },
        GraphEdgeKind::Extends | GraphEdgeKind::Implements => {
            unique_symbol(symbols, &edge.ref_name)
        }
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
    fn module_local_definitions_answer_bare_calls_before_the_file_scope() {
        // `mod storage { fn save() {} fn flush() { save() } }` beside a
        // top-level `save`: `save()` inside `flush` reaches the module's own
        // definition, and the top-level one keeps only the callers that name it
        // from the file scope.
        let mut context = WalkContext::new();
        let normalize = context.next_entity_index();
        context.register_symbol(Some("normalize"), normalize);
        let file_save = context.next_entity_index();
        context.register_symbol(Some("save"), file_save);
        let import_record = context.next_entity_index();
        context.register_symbol(Some("import_record"), import_record);
        let export_record = context.next_entity_index();
        context.register_symbol(Some("export_record"), export_record);
        let storage = context.next_entity_index();
        context.register_symbol(Some("storage"), storage);
        context.register_module_scope(storage);
        context.push_scope(storage);
        let module_save = context.next_entity_index();
        context.register_symbol(Some("save"), module_save);
        context.register_scoped_symbol(Some("save"), module_save);
        context.add_contains_edge(module_save, 14, 4);
        let flush = context.next_entity_index();
        context.register_symbol(Some("flush"), flush);
        context.register_scoped_symbol(Some("flush"), flush);
        context.add_contains_edge(flush, 16, 4);
        context.pop_scope();

        context.add_call_edge(import_record, "save", None, None, Some(1), 6, 43);
        context.add_call_edge(export_record, "save", None, None, Some(1), 8, 43);
        context.add_call_edge(flush, "save", None, None, Some(1), 16, 39);

        let graph = partition_walk(context, (0..7).collect());
        let calls: Vec<(usize, usize)> = graph
            .resolved
            .iter()
            .filter(|edge| edge.ref_kind == GraphEdgeKind::Calls)
            .map(|edge| (edge.source_index, edge.target_index))
            .collect();
        assert_eq!(
            calls,
            vec![
                (import_record, file_save),
                (export_record, file_save),
                (flush, module_save),
            ],
            "the module's own `save` answers `flush`; the file-level one keeps only its callers"
        );
        assert!(graph.pending.is_empty());
    }

    #[test]
    fn outer_modules_do_not_answer_bare_calls_in_inner_modules() {
        // `mod outer { fn save() {} mod inner { fn flush() { save() } } }`: an
        // outer module's items need `super::`, so `inner`'s missing `save` must
        // not fall through to `outer::save` — the resolver's evidence decides,
        // not a name the call cannot reach.
        let mut context = WalkContext::new();
        let outer = context.next_entity_index();
        context.register_symbol(Some("outer"), outer);
        context.register_module_scope(outer);
        context.push_scope(outer);
        let outer_save = context.next_entity_index();
        context.register_symbol(Some("save"), outer_save);
        context.register_scoped_symbol(Some("save"), outer_save);
        let inner = context.next_entity_index();
        context.register_symbol(Some("inner"), inner);
        context.register_scoped_symbol(Some("inner"), inner);
        context.add_contains_edge(inner, 2, 4);
        context.register_module_scope(inner);
        context.push_scope(inner);
        let flush = context.next_entity_index();
        context.register_symbol(Some("flush"), flush);
        context.register_scoped_symbol(Some("flush"), flush);
        context.add_contains_edge(flush, 3, 8);
        context.pop_scope();
        context.pop_scope();

        context.add_call_edge(flush, "save", None, None, Some(0), 3, 20);

        let graph = partition_walk(context, (0..4).collect());
        assert!(
            graph.resolved.is_empty(),
            "an outer module's function must not answer a bare call in an inner module"
        );
        assert_eq!(graph.pending.len(), 1);
        assert_eq!(graph.pending[0].ref_name, "save");
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
