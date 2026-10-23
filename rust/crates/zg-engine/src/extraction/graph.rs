//! Walk-time state for code graph edge collection.
//!
//! Mirrors the TypeScript three-segment collection design:
//!
//! 1. `scope_stack` — entity indices on the containment path. A new entity
//!    pushes its index; `contains` edges are emitted from the stack top, so
//!    both endpoints are position-known at walk time (no name lookup needed).
//! 2. `symbols` — name → entity indices, registered as entities are produced.
//!    The post-walk partition consults it to resolve in-file references
//!    (forward references included, because partition runs after the walk).
//! 3. `resolved_edges` / `name_edges` — the two output buffers. Position-based
//!    edges (`contains`) go straight to `resolved_edges`; name-based references
//!    (calls/imports/extends/implements) are buffered in `name_edges` because
//!    their targets are unknown during the walk.
//!
//! Unlike the TypeScript original, which mints string entity IDs during the
//! walk, this context works in *entity-index space*: the indexing pipeline
//! mints `EntityId`s from `sha256(fileId \0 headFragmentIndex)` and the
//! extraction layer never sees the file ID. Indices here are per-file entity
//! ordinals allocated in walk order; `extract_code` appends fragments in the
//! same order, so each ordinal maps onto its entity's head fragment index
//! during the append loop, and the pipeline-side assembly resolves that index
//! into the minted `EntityId`.
//!
//! Walker integration: `walk_code_node` threads a `WalkContext` through its
//! recursion, so every indexed code file buffers its containment edges and
//! name references during extraction. The post-walk
//! [`partition`](self::partition::partition_walk) step then splits those
//! buffers into in-file resolved edges and pending references.

mod collector;
mod partition;
mod vocabulary;

pub(crate) use collector::{
    collect_import_edge, collect_inheritance_edges, is_import_node, scan_binding_declarations,
    scan_call_edges, scan_return_types,
};
pub(crate) use partition::{PartitionedGraph, partition_walk};

use std::collections::{HashMap, HashSet};

use crate::domain::GraphRefKind;

/// Owner index standing in for file scope. Import references belong to the
/// file, not to any entity; the TypeScript original passes the file ID here.
/// The partition step maps this sentinel onto the file node's ID. Never
/// allocated by [`WalkContext::next_entity_index`].
pub(crate) const FILE_SCOPE_INDEX: usize = usize::MAX;

/// An edge whose both endpoints are known at walk time. Only `contains`
/// qualifies: the target entity is created during the same walk, so its index
/// is available from the scope stack. Positions use one-based lines and
/// zero-based columns, matching [`crate::domain::TextRange`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WalkEdge {
    pub source_index: usize,
    pub target_index: usize,
    pub line: usize,
    pub column: usize,
}

/// A name-based reference buffered during the walk. The target is only a name
/// at this point (the definition may live in another file, later in this file,
/// or not exist at all), so these are partitioned after the walk: in-file
/// resolvable ones become edges, the rest become pending refs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NameEdge {
    pub ref_kind: GraphRefKind,
    /// Entity index owning the reference ([`FILE_SCOPE_INDEX`] for imports).
    pub owner_index: usize,
    /// Short name of the referenced symbol (last segment for `a.b()`).
    pub ref_name: String,
    /// Receiver text for member calls (`a.b()` → `Some("a")`).
    pub receiver_name: Option<String>,
    /// Original reference text as written in the source.
    pub raw_text: Option<String>,
    /// Declared type of the bound name for `Declares` edges
    /// (`var x = new X()` → `Some("X")`).
    pub bound_type: Option<String>,
    /// Argument count at the call site, when the reference is a call.
    pub arity: Option<usize>,
    pub line: usize,
    pub column: usize,
}

/// Mutable state carried through a single AST walk.
#[derive(Debug, Default)]
pub(crate) struct WalkContext {
    resolved_edges: Vec<WalkEdge>,
    name_edges: Vec<NameEdge>,
    scope_stack: Vec<usize>,
    symbols: HashMap<String, Vec<usize>>,
    /// Symbols each containing entity declares directly in its body, the
    /// innermost-scope shadow table: a call inside `outer` that names
    /// `work` binds the nested `outer::work` before any file-level name.
    scoped_symbols: HashMap<usize, HashMap<String, usize>>,
    seen_name_edges: HashSet<(GraphRefKind, usize, Option<String>, String, usize, usize)>,
    /// Factory return types this file spells: `Some` answers a bare-call
    /// initializer (`x = makeX()`), `None` marks a name registered with two
    /// different types whose factory answer is ambiguous.
    return_types: HashMap<String, Option<String>>,
    entity_counter: usize,
}

impl WalkContext {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Allocates the next per-file entity ordinal. Ordinals follow walk order,
    /// so they are stable for unchanged file content.
    pub(crate) fn next_entity_index(&mut self) -> usize {
        let index = self.entity_counter;
        self.entity_counter += 1;
        index
    }

    /// Index of the innermost containing entity, or `None` at file top level.
    pub(crate) fn current_scope_index(&self) -> Option<usize> {
        self.scope_stack.last().copied()
    }

    /// Enters an entity's scope; must be paired with [`Self::pop_scope`].
    pub(crate) fn push_scope(&mut self, index: usize) {
        self.scope_stack.push(index);
    }

    /// Leaves the innermost entity scope.
    pub(crate) fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    /// Registers a symbol so partition can resolve in-file references to it.
    /// Absent and empty names are skipped, mirroring the TypeScript guard.
    pub(crate) fn register_symbol(&mut self, name: Option<&str>, index: usize) {
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return;
        };
        self.symbols.entry(name.to_owned()).or_default().push(index);
    }

    /// Registers a nested entity under its containing entity's local name
    /// table: inside that entity's body the nested spelling shadows any
    /// file-level name. The registration is a no-op at file top level.
    pub(crate) fn register_scoped_symbol(&mut self, name: Option<&str>, index: usize) {
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return;
        };
        let Some(owner) = self.current_scope_index() else {
            return;
        };
        self.scoped_symbols
            .entry(owner)
            .or_default()
            .insert(name.to_owned(), index);
    }

    #[allow(dead_code)]
    /// The per-entity local name tables partition consults before the
    /// file-level one.
    pub(crate) fn scoped_symbols(&self) -> &HashMap<usize, HashMap<String, usize>> {
        &self.scoped_symbols
    }

    /// Entity indices in this file that declare `name`, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn lookup_symbol(&self, name: &str) -> Option<&[usize]> {
        self.symbols.get(name).map(Vec::as_slice)
    }

    /// True when exactly one entity in this file declares `name`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_unique_symbol(&self, name: &str) -> bool {
        self.symbols
            .get(name)
            .is_some_and(|indices| indices.len() == 1)
    }

    /// Emits a `contains` edge from the innermost scope. Top-level entities
    /// produce no walk-time edge; the graph assembly links them to the file
    /// node once it is materialized.
    pub(crate) fn add_contains_edge(&mut self, target_index: usize, line: usize, column: usize) {
        let Some(source_index) = self.current_scope_index() else {
            return;
        };
        self.resolved_edges.push(WalkEdge {
            source_index,
            target_index,
            line,
            column,
        });
    }

    /// Buffers a name-based reference. Duplicate references (same owner, kind,
    /// receiver, and name — e.g. a callee invoked in a loop) collapse to the
    /// first occurrence; per-site evidence can be accumulated later via
    /// metadata. Distinct receivers stay separate: `left_api.work()` and
    /// `right_api.work()` can bind different declarations.
    pub(crate) fn add_name_edge(&mut self, edge: NameEdge) {
        // The site coordinates close the key: two calls of one shape at
        // different sites are distinct evidence (`x.foo()` before and after
        // `x = new Y()` bind different types), while a twice-collected site
        // still collapses.
        let key = (
            edge.ref_kind,
            edge.owner_index,
            edge.receiver_name.clone(),
            edge.ref_name.clone(),
            edge.line,
            edge.column,
        );
        if !self.seen_name_edges.insert(key) {
            return;
        }
        self.name_edges.push(edge);
    }

    /// Convenience wrapper for buffered call references.
    // Mirrors the NameEdge fields callers already have in hand; folding them
    // into a struct would only relocate the verbosity to every call site.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_call_edge(
        &mut self,
        owner_index: usize,
        ref_name: &str,
        receiver_name: Option<&str>,
        raw_text: Option<&str>,
        arity: Option<usize>,
        line: usize,
        column: usize,
    ) {
        self.add_name_edge(NameEdge {
            ref_kind: GraphRefKind::Calls,
            owner_index,
            ref_name: ref_name.to_owned(),
            receiver_name: receiver_name.map(str::to_owned),
            raw_text: raw_text.map(str::to_owned),
            bound_type: None,
            arity,
            line,
            column,
        });
    }

    /// Buffers a variable/parameter type binding (`var x = new X()`,
    /// `caller(Left a)`): the owner declares that `name`'s receiver type is
    /// `bound_type`. Always pending — the type entity may live in another
    /// file — and consumed by the resolver when a call names `name` as its
    /// receiver at or after this line.
    pub(crate) fn add_declare_binding(
        &mut self,
        owner_index: usize,
        name: &str,
        bound_type: &str,
        line: usize,
        column: usize,
    ) {
        // Bindings bypass the name-edge dedup: rebinding one name to a
        // different type is exactly the evidence the resolver orders by
        // line, and identical redeclarations collapse harmlessly at
        // resolution time.
        self.name_edges.push(NameEdge {
            ref_kind: GraphRefKind::Declares,
            owner_index,
            ref_name: name.to_owned(),
            receiver_name: None,
            raw_text: None,
            bound_type: Some(bound_type.to_owned()),
            arity: None,
            line,
            column,
        });
    }

    /// Registers one function's factory return type: the signature's
    /// annotation, or the type its `return` construction spells. A name
    /// registered with two different types (same-named methods, overloads)
    /// becomes ambiguous and stops answering factory lookups.
    pub(crate) fn register_return_type(&mut self, name: &str, return_type: &str) {
        let entry = self.return_types.entry(name.to_owned()).or_insert(None);
        match entry {
            None => *entry = Some(return_type.to_owned()),
            Some(existing) if existing == return_type => {}
            Some(_) => *entry = None,
        }
    }

    /// The factory return type a bare call (`makeX()`) resolves to, as
    /// registered by this file's function signatures and return bodies.
    pub(crate) fn factory_return_of(&self, name: &str) -> Option<&str> {
        self.return_types.get(name)?.as_deref()
    }

    /// Convenience wrapper for buffered import references. The owner is the
    /// file scope: imports belong to the file, not to any entity.
    pub(crate) fn add_import_edge(
        &mut self,
        module_path: &str,
        raw_text: Option<&str>,
        line: usize,
        column: usize,
    ) {
        self.add_name_edge(NameEdge {
            ref_kind: GraphRefKind::Imports,
            owner_index: FILE_SCOPE_INDEX,
            ref_name: module_path.to_owned(),
            receiver_name: None,
            raw_text: raw_text.map(str::to_owned),
            bound_type: None,
            arity: None,
            line,
            column,
        });
    }

    /// Convenience wrapper for buffered `extends`/`implements` references.
    pub(crate) fn add_inheritance_edge(
        &mut self,
        ref_kind: GraphRefKind,
        owner_index: usize,
        ref_name: &str,
        raw_text: Option<&str>,
        line: usize,
        column: usize,
    ) {
        debug_assert!(matches!(
            ref_kind,
            GraphRefKind::Extends | GraphRefKind::Implements
        ));
        self.add_name_edge(NameEdge {
            ref_kind,
            owner_index,
            ref_name: ref_name.to_owned(),
            receiver_name: None,
            raw_text: raw_text.map(str::to_owned),
            bound_type: None,
            arity: None,
            line,
            column,
        });
    }

    /// Walk-time resolved (`contains`) edges collected so far. Consumed by the
    /// post-walk partition and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resolved_edges(&self) -> &[WalkEdge] {
        &self.resolved_edges
    }

    /// Buffered name references collected so far. Consumed by the post-walk
    /// partition and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn name_edges(&self) -> &[NameEdge] {
        &self.name_edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_entity_indices_in_walk_order() {
        let mut context = WalkContext::new();
        assert_eq!(context.next_entity_index(), 0);
        assert_eq!(context.next_entity_index(), 1);
        assert_eq!(context.next_entity_index(), 2);
    }

    #[test]
    fn contains_edges_follow_the_scope_stack() {
        let mut context = WalkContext::new();
        let outer = context.next_entity_index();
        context.push_scope(outer);
        let inner = context.next_entity_index();
        context.add_contains_edge(inner, 10, 4);
        context.pop_scope();
        let top_level = context.next_entity_index();
        context.add_contains_edge(top_level, 20, 0);
        // Top-level entities are linked to the file node during assembly, so
        // they produce no walk-time edge.
        assert_eq!(
            context.resolved_edges(),
            &[WalkEdge {
                source_index: outer,
                target_index: inner,
                line: 10,
                column: 4,
            }]
        );
    }

    #[test]
    fn registers_and_looks_up_symbols() {
        let mut context = WalkContext::new();
        context.register_symbol(None, 0);
        context.register_symbol(Some(""), 1);
        context.register_symbol(Some("helper"), 2);
        context.register_symbol(Some("helper"), 3);
        context.register_symbol(Some("unique"), 4);
        assert_eq!(context.lookup_symbol("helper"), Some(&[2, 3][..]));
        assert_eq!(context.lookup_symbol("missing"), None);
        assert!(context.is_unique_symbol("unique"));
        assert!(!context.is_unique_symbol("helper"));
        assert!(!context.is_unique_symbol("missing"));
    }

    #[test]
    fn collapses_duplicate_name_edges() {
        let mut context = WalkContext::new();
        context.add_call_edge(0, "helper", None, None, None, 3, 1);
        // Same site: the twice-collected call collapses even when the
        // decorative evidence (raw text, arity) differs.
        context.add_call_edge(0, "helper", None, Some("helper()"), Some(0), 3, 1);
        // The same shape at another site is distinct evidence: `x.foo()`
        // before and after `x = new Y()` bind different declarations.
        context.add_call_edge(0, "helper", None, None, None, 9, 8);
        // A distinct receiver binds independently: `this.helper()` can
        // resolve to another declaration than a bare `helper()`.
        context.add_call_edge(
            0,
            "helper",
            Some("this"),
            Some("this.helper()"),
            Some(0),
            10,
            9,
        );
        context.add_call_edge(1, "helper", None, None, None, 4, 2);
        context.add_call_edge(0, "other", None, None, None, 5, 3);
        assert_eq!(context.name_edges().len(), 5);
    }

    #[test]
    fn buffers_call_import_and_inheritance_references() {
        let mut context = WalkContext::new();
        let owner = context.next_entity_index();
        context.add_call_edge(
            owner,
            "bark",
            Some("dog"),
            Some("dog.bark()"),
            Some(2),
            7,
            11,
        );
        context.add_import_edge("./utils.js", Some("import './utils.js'"), 1, 0);
        context.add_inheritance_edge(GraphRefKind::Extends, owner, "Animal", None, 2, 5);
        context.add_inheritance_edge(GraphRefKind::Implements, owner, "Runnable", None, 2, 5);

        let edges = context.name_edges();
        assert_eq!(
            edges[0],
            NameEdge {
                ref_kind: GraphRefKind::Calls,
                owner_index: owner,
                ref_name: "bark".to_owned(),
                receiver_name: Some("dog".to_owned()),
                raw_text: Some("dog.bark()".to_owned()),
                bound_type: None,
                arity: Some(2),
                line: 7,
                column: 11,
            }
        );
        assert_eq!(edges[1].ref_kind, GraphRefKind::Imports);
        assert_eq!(edges[1].owner_index, FILE_SCOPE_INDEX);
        assert_eq!(edges[1].ref_name, "./utils.js");
        assert_eq!(edges[2].ref_kind, GraphRefKind::Extends);
        assert_eq!(edges[3].ref_kind, GraphRefKind::Implements);
    }
}
