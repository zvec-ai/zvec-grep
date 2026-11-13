//! Cross-file resolution of pending references against the workspace snapshot.
//!
//! The resolver consumes the graph storage's pending queue and edge evidence,
//! plus one [`ResolutionSnapshot`] of the canonical zvec index, so it needs no
//! extraction state: [`IndexStore::list_pending_refs`] pages the backlog,
//! relative import specifiers resolve through [`ResolutionSnapshot::file_id_by_path`],
//! [`ResolutionSnapshot::lookup_candidates`] narrows name candidates, ambiguous
//! bare names resolve through [`ResolutionSnapshot::imported_file_ids`] when
//! the file's imports pin them to one module, `self`/`this`/`super` receiver
//! calls bind the nearest declaration along the extends/implements
//! hierarchy, qualified `Class::method` receivers bind to the entity whose
//! `qualified_name` spells the same path, Java imports bind to the type or
//! static member their specifier spells, a bare call the translation unit
//! defines as a macro at its own line stays pending (the preprocessor replaced
//! it before any call existed), export/arity evidence narrows the remaining
//! ambiguities, external import specifiers terminate as failed, and
//! [`IndexStore::apply_resolutions`] writes each batch atomically.

mod bindings;
mod cpp_types;
mod module_config;
mod rust_types;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::pipeline::throw_if_cancelled;
use super::preprocessor::MacroTimeline;
use crate::domain::{
    EdgeProvenance, FileFormat, GraphEdgeKind, SymbolType, formats_for_path,
    reference_compatible_with,
};
use crate::storage::graph::{
    DeclaredBinding, ImportReference, NameIndexEntry, Resolution, StoredPendingRef,
};
use crate::storage::{IndexStore, ResolutionSnapshot};

/// Outcome of one resolution sweep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResolutionSweep {
    /// Pending references examined this sweep.
    pub scanned: usize,
    /// References resolved by `apply_resolutions`.
    pub resolved: usize,
    /// Proposals that no longer matched a pending row.
    pub stale: usize,
    /// Ambiguous references resolved through import scoping this sweep.
    pub import_bound: usize,
    /// Receiver-qualified calls bound through the owner's declarations or its
    /// class hierarchy this sweep.
    pub file_scoped: usize,
    /// Qualified references bound through `qualified_name` this sweep.
    pub qualified_name_match: usize,
    /// Ambiguous references narrowed to the one exported candidate.
    pub export_bound: usize,
    /// Ambiguous references narrowed to the one arity-matching candidate.
    pub arity_bound: usize,
    /// Import references terminated as unresolvable by `apply_failures`.
    pub failed: usize,
    /// Ambiguous references whose candidate sets `record_candidates` stored.
    pub candidates_recorded: usize,
}

/// Resolves pending references each sweep: relative import specifiers onto
/// file endpoints, workspace-unique names onto their single definition,
/// ambiguous bare names onto the one module their file imports,
/// `self`/`this` receiver calls onto the class owning the call site, the
/// nearest declaration along its extends/implements chain, and
/// qualified `Class::method` receivers onto the entity whose `qualified_name`
/// spells that scope and member. Ambiguities surviving these rules narrow
/// through export visibility and declared arity when the evidence picks
/// exactly one candidate, and import specifiers that address modules outside
/// the workspace terminate as failed.
///
/// Matching stays conservative. A receiver-qualified same-file hit stays
/// pending — partition already resolved unambiguous local names, so such a
/// survivor needs type inference; a `self`/`this` receiver is answered only
/// through the owner's class hierarchy, and a member the nearest declaring
/// scope holds ambiguously stays pending. A bare name's lone same-file hit,
/// by contrast, binds as `FileScoped`:
/// the in-file symbol table skips declaration entities, so no earlier rule
/// could have claimed it. Names that stay ambiguous after every evidence rule
/// record their candidate set for consumers.
///
/// The entire read/resolve/writeback cycle runs under the caller's graph
/// storage lock, matching the serialization contract of `apply_resolutions`.
/// The pass's cancellation token bounds that work: a cancelled sweep stops at
/// the next boundary — before the whole-workspace snapshot projection, between
/// references, and at every page writeback — so the queue's remainder stays
/// pending and the caller can release the workspace writer without waiting for
/// the backlog to drain.
///
/// # Errors
/// Propagates storage failures. Completed writeback batches remain committed;
/// references not yet written stay pending for a later sweep. A cancelled pass
/// returns [`EngineError::CANCELLED`].
pub(crate) fn resolve_pending(
    store: &IndexStore,
    source_root: Option<&Path>,
    signal: Option<&CancellationToken>,
) -> crate::EngineResult<ResolutionSweep> {
    if !store.graph_enabled() {
        // A workspace without a graph has nothing to resolve. Answering here
        // keeps the snapshot's whole-workspace file and entity projection off
        // every indexing pass, and stops the caller from reporting an absent
        // feature as a failed sweep on each refresh.
        return Ok(ResolutionSweep::default());
    }
    // The projection reads every file and named entity in the workspace: check
    // before paying for it, and let it honor the same token mid-read, so a
    // cancellation during either step ends here instead of growing an index the
    // pass will discard.
    throw_if_cancelled(signal)?;
    let snapshot = store.resolution_snapshot(signal)?;
    let mut sweep = ResolutionSweep::default();
    let mut caches = RuleCaches {
        source_root: source_root.map(Path::to_path_buf),
        ..RuleCaches::default()
    };
    let mut cursor = 0;
    loop {
        let page = store.list_pending_refs(PENDING_PAGE, cursor, &snapshot)?;
        sweep.scanned += page.refs.len();
        let mut batch = QueuedBatch::default();
        for stored in &page.refs {
            // Each reference issues several storage lookups and rule passes, so
            // a full page is not a short step; check between references rather
            // than only at the page boundary.
            throw_if_cancelled(signal)?;
            match classify(store, &snapshot, stored, &mut caches)? {
                // Declares edges stay pending as receiver-typed evidence, and
                // refs no rule reaches wait for a later sweep.
                Outcome::Deferred => {}
                Outcome::Failed => batch.failures.push(stored.id),
                Outcome::Resolved { resolution, rule } => {
                    sweep.record(rule);
                    batch.resolutions.push(resolution);
                }
                Outcome::Ambiguous(candidates) => {
                    // An unchanged candidate set is already durable evidence:
                    // rewriting the same JSON every sweep only burns WAL writes.
                    if stored.candidates.as_deref() != Some(candidates.as_slice()) {
                        batch.candidates.push((stored.id, candidates));
                    }
                }
            }
        }
        // Write back at the page boundary: everything decided so far is
        // durable, so a cancelled or failing later page leaves its references
        // pending without discarding the work already done.
        batch.write_back(store, &snapshot, &mut sweep)?;
        let Some(next) = page.next_cursor else {
            break;
        };
        throw_if_cancelled(signal)?;
        cursor = next;
    }
    Ok(sweep)
}

/// References per queue page: large enough to amortize the cursor query and
/// one writeback transaction, small enough to bound both the memory each page
/// holds and how much work a cancelled pass can still complete.
const PENDING_PAGE: usize = 1000;

/// Decisions gathered for one queue page, flushed before the next page reads.
#[derive(Default)]
struct QueuedBatch {
    resolutions: Vec<Resolution>,
    failures: Vec<i64>,
    candidates: Vec<(i64, Vec<String>)>,
}

impl QueuedBatch {
    /// Applies this page's decisions and folds their counts into `sweep`.
    /// Storage revalidates every row, so proposals whose reference or endpoint
    /// a concurrent write hid are dropped into the stale count instead of
    /// binding.
    fn write_back(
        self,
        store: &IndexStore,
        snapshot: &ResolutionSnapshot,
        sweep: &mut ResolutionSweep,
    ) -> crate::EngineResult<()> {
        if !self.resolutions.is_empty() {
            let stats = store.apply_resolutions(&self.resolutions, snapshot)?;
            sweep.resolved += stats.resolved;
            sweep.stale += stats.stale;
        }
        if !self.failures.is_empty() {
            sweep.failed += store.apply_failures(&self.failures, snapshot)?;
        }
        if !self.candidates.is_empty() {
            sweep.candidates_recorded += store.record_candidates(&self.candidates, snapshot)?;
        }
        Ok(())
    }
}

/// What the rule chain concluded about one pending reference.
enum Outcome {
    /// A rule bound the reference onto an indexed target.
    Resolved { resolution: Resolution, rule: Rule },
    /// The specifier addresses a module outside the workspace.
    Failed,
    /// No rule reaches the reference yet, so it stays pending.
    Deferred,
    /// Every evidence rule left the reference ambiguous.
    Ambiguous(Vec<String>),
}

/// Which rule bound a reference, kept apart so the sweep can count it.
#[derive(Clone, Copy)]
enum Rule {
    /// A relative import specifier placed on a file endpoint.
    Import,
    /// An alias, namespace, or module binding of an ambiguous bare name.
    ImportBound,
    /// A receiver pinned to its enclosing instance, superclass, or declared type.
    Receiver,
    /// A member call answered by the receiver's object-literal property table.
    ObjectMember,
    /// A qualified `Class::method` matched by `qualified_name`.
    QualifiedName,
    /// A workspace-unique name, or the one same-file declaration of a bare call.
    Unique,
    /// The one exported candidate.
    ExportBound,
    /// The one arity-matching candidate.
    ArityBound,
}

/// Per-sweep memos every rule reads and extends.
///
/// Grouping them keeps the chain's hidden coupling visible: each rule both
/// consumes evidence gathered by earlier references and adds its own.
#[derive(Default)]
struct RuleCaches {
    source_root: Option<std::path::PathBuf>,
    module_configs: HashMap<String, Result<Option<Arc<serde_json::Value>>, ()>>,
    tsconfigs: HashMap<String, Result<Option<Arc<module_config::TsConfig>>, ()>>,
    go_modules: HashMap<String, Option<String>>,
    importing_paths: HashMap<u32, Option<String>>,
    /// File formats of each candidate's file, read once per file instead of
    /// re-parsing the path for every reference that names the same symbol.
    candidate_formats: HashMap<u32, Option<Vec<FileFormat>>>,
    importing_files: HashMap<u32, Vec<u32>>,
    module_paths: HashMap<(String, String), bool>,
    alias_bindings: HashMap<u32, Vec<AliasBinding>>,
    alias_targets: HashMap<(String, String), Option<u32>>,
    /// Names one file's import statements bind locally, with the specifier
    /// that names each module. A bound name belongs to that import alone, so
    /// the fallback rules must not answer it from the workspace once the
    /// statement's module is gone.
    import_names: HashMap<u32, Vec<ImportBinding>>,
    /// Declarations read for one owner. Most untyped member calls reach the
    /// same owner and every sweep pages them again, so the read happens once
    /// per owner per sweep instead of once per call.
    declared_bindings: HashMap<String, Vec<DeclaredBinding>>,
    /// Whether one qualified scope names a type, the evidence that tells a
    /// method from a free function when a bare call spells its name. Keyed by
    /// scope, read once per sweep.
    owner_is_type: HashMap<String, bool>,
    /// Files one file's include/import chain reaches, transitively. A TU-local
    /// declaration in an included file is still inside the includer's
    /// translation unit, so the closure answers reachability for the whole
    /// chain at once instead of per candidate.
    include_closures: HashMap<u32, HashSet<u32>>,
    /// One file's replayed macro timeline, the evidence a bare call is a macro
    /// expansion rather than a call. Every reference of a file asks the same
    /// question, and the replay walks that file's whole include chain, so it
    /// runs once per file per sweep.
    tu_macros: HashMap<u32, MacroTimeline>,
    import_references: HashMap<u32, Arc<[ImportReference]>>,
    rust_types: rust_types::TypeCache,
    cpp_types: cpp_types::TypeCache,
    javascript_imports: HashMap<u32, Arc<[bindings::JavascriptImport]>>,
}

impl RuleCaches {
    // The workspace writer serializes the sweep, so imports cannot change
    // until it finishes. Drop this cache before the next indexing pass.
    fn imports(
        &mut self,
        store: &IndexStore,
        file: u32,
    ) -> crate::EngineResult<Arc<[ImportReference]>> {
        Ok(match self.import_references.entry(file) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => Arc::clone(entry.insert(store.import_references(file)?.into())),
        })
    }
}

impl ResolutionSweep {
    fn record(&mut self, rule: Rule) {
        match rule {
            Rule::Import | Rule::Unique => {}
            Rule::ImportBound => self.import_bound += 1,
            Rule::Receiver | Rule::ObjectMember => self.file_scoped += 1,
            Rule::QualifiedName => self.qualified_name_match += 1,
            Rule::ExportBound => self.export_bound += 1,
            Rule::ArityBound => self.arity_bound += 1,
        }
    }
}

/// Runs the path and binding rules in precedence order, then hands the
/// reference to the name rules.
///
/// Order carries the semantics: an import specifier outranks every name rule.
/// Rust's `use` and Java's `import` name an entity, so they continue into the
/// name rules; an import that addresses a module instead defers rather than
/// guessing at a same-named symbol when the path rules could not place it.
fn classify(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Outcome> {
    if stored.reference.ref_kind == GraphEdgeKind::Declares {
        // Binding evidence for `resolve_receiver_typed`, not a call site: the
        // bound variable name must never reach the name-index lookup below.
        // Declares edges stay pending in the table for every later sweep.
        return Ok(Outcome::Deferred);
    }
    if expands_as_macro(store, snapshot, stored, caches)? {
        // The name is a macro at this line, so the call never happens: the
        // preprocessor replaces it with the macro's body before the compiler
        // sees a call. Whatever function the name also spells elsewhere — the
        // decoy, a header's prototype — is not what this site reaches.
        return Ok(Outcome::Deferred);
    }
    if let Some(outcome) = bindings::resolve_java_constructor(store, snapshot, stored, caches)? {
        return Ok(outcome);
    }
    if let Some(outcome) = cpp_types::resolve_constructor(store, snapshot, stored, caches)? {
        return Ok(outcome);
    }
    if let Some(outcome) = cpp_types::resolve_receiver(store, snapshot, stored, caches)? {
        return Ok(outcome);
    }
    if stored.reference.ref_kind == GraphEdgeKind::Calls
        && matches!(stored.language.as_str(), "typescript" | "javascript")
        && let Some(outcome) = bindings::resolve_call(store, snapshot, stored, caches)?
    {
        return Ok(outcome);
    }
    // TS/JS lexical calls were handled above. Keep this new binding rule
    // confined to Go; other languages retain their existing resolution rules.
    if stored.language == "go"
        && stored.reference.ref_kind == GraphEdgeKind::Calls
        && stored.reference.receiver_name.is_none()
    {
        let bindings = match caches
            .declared_bindings
            .entry(stored.reference.owner_id.clone())
        {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                entry.insert(store.declared_bindings(&stored.reference.owner_id)?)
            }
        };
        if bindings.iter().any(|binding| {
            binding.name == stored.name_tail
                && (binding.line as usize, binding.column as usize)
                    <= (stored.reference.line, stored.reference.column)
                && binding
                    .scope_end
                    .is_none_or(|end| (stored.reference.line, stored.reference.column) < end)
        }) {
            return Ok(Outcome::Deferred);
        }
    }
    if is_unresolvable_import(snapshot, stored, &mut caches.module_paths) {
        return Ok(Outcome::Failed);
    }
    if let Some(resolution) = resolve_import(snapshot, stored, &mut caches.importing_paths) {
        // The batch applies after the loop, so fold this sweep's fresh import
        // targets into the in-memory scope for bare names.
        record_import_scope(snapshot, stored, &resolution, &mut caches.importing_files);
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Import,
        });
    }
    if stored.reference.ref_kind == GraphEdgeKind::Imports
        && !matches!(stored.language.as_str(), "rust" | "java")
    {
        // Rust's `use a::b` and Java's `import p.Widget` both address an item
        // by name — a name binding, or a type or static member — so the name
        // index can answer them. A TS/JS or Python import addresses a module
        // instead; when the path rules above could not place it — a scoped
        // package, a `paths` alias — a name match would be a coincidence, and
        // the wrong bind also rewrites this file's import scope for every
        // later bare-name resolution. Defer instead.
        return Ok(Outcome::Deferred);
    }
    if let Some(resolution) = resolve_alias_bound(store, snapshot, stored, caches)? {
        // An `as` alias never names an indexed symbol itself, so the bare-name
        // lookup below would only park the call; the alias binding pins its
        // imported name's module directly.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::ImportBound,
        });
    }
    if let Some(resolution) = resolve_namespace_bound(store, snapshot, stored, caches)? {
        // The receiver names a module object, so the bare-name lookup below
        // would only see workspace-wide ambiguity; the binding scopes the
        // member to its one module.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::ImportBound,
        });
    }
    classify_names(store, snapshot, stored, caches)
}

/// Whether the call site spells a macro the translation unit defines there.
///
/// Only a bare call can stand for a macro: `object.member` and `Scope::member`
/// are resolved after expansion, and a receiver-qualified spelling never names
/// a macro. Only C and C++ have a preprocessor at all.
///
/// # Errors
/// Propagates graph storage failures of the directive and pending-name reads.
fn expands_as_macro(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<bool> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls
        || reference.receiver_name.is_some()
        || !matches!(stored.language.as_str(), "c" | "cpp")
    {
        return Ok(false);
    }
    if let Entry::Vacant(entry) = caches.tu_macros.entry(stored.file_id) {
        entry.insert(MacroTimeline::build(
            store,
            snapshot,
            &stored.language,
            stored.file_id,
        )?);
    }
    Ok(caches
        .tu_macros
        .get(&stored.file_id)
        .is_some_and(|timeline| timeline.defined_at(&stored.name_tail, reference.line)))
}

/// The candidates a name reference may bind to, narrowed by language,
/// reachability, callability and the enclosing scopes; `None` while the
/// snapshot still hides one of the name's candidates.
///
/// # Errors
/// Propagates graph storage failures.
fn name_hits<'s>(
    store: &IndexStore,
    snapshot: &'s ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Vec<&'s NameIndexEntry>>> {
    let Some(raw) = snapshot.lookup_candidates(&stored.name_tail) else {
        return Ok(None);
    };
    // The language gate runs first: preference for definitions is only
    // meaningful among candidates that can host this reference.
    let compatible = filter_compatible_hits(snapshot, stored, raw, &mut caches.candidate_formats);
    // Reachability and callability are facts about the candidates, so they
    // narrow the set before any rule reads it.
    let reachable = linkage_reachable(store, snapshot, stored, compatible, caches)?;
    let callable = callable_targets(snapshot, stored, reachable, &mut caches.owner_is_type);
    // A name a nearer scope also declares binds to that declaration: the
    // enclosing namespace is searched before the workspace.
    let visible = enclosing_scope_hits(snapshot, stored, callable);
    Ok(Some(prefer_definitions(visible)))
}

/// Runs the name-index rules: receivers, qualified names, uniqueness, and the
/// import/export/arity bounds that narrow an ambiguous set.
fn classify_names(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Outcome> {
    let Some(hits) = name_hits(store, snapshot, stored, caches)? else {
        // Hidden candidates must not turn an ambiguous name into a seemingly
        // unique match against an unfinished snapshot. The object member rule
        // needs no candidate of its own — the literal's property table names
        // the value — so it still applies.
        return object_member_outcome(store, snapshot, stored, caches);
    };
    if stored.reference.ref_kind == GraphEdgeKind::Imports && stored.language == "java" {
        // A Java import names an entity rather than a module, and only its own
        // specifier can place it: the package segment never reaches the index,
        // so the rule matches the candidate's declared path against the tail
        // of the specifier and the package against the file it lives in. A
        // specifier that identifies no single candidate stays pending — the
        // bare-name rules below would bind the short name (`work`) and drop
        // the class that disambiguates it.
        return Ok(match resolve_java_import(stored, &hits) {
            Some(resolution) => Outcome::Resolved {
                resolution,
                rule: Rule::ImportBound,
            },
            None => Outcome::Deferred,
        });
    }
    if let Some(resolution) = resolve_receiver_class(store, snapshot, stored, &hits, caches)? {
        // The receiver names the enclosing instance or superclass, so the
        // member's declaring scope is pinned to the owner's class hierarchy
        // before workspace-wide ambiguity is considered.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Receiver,
        });
    }
    if let Some(resolution) = resolve_receiver_typed(store, snapshot, stored, &hits, caches)? {
        // The receiver names a local whose declared type pins the member's
        // declaring scope before workspace-wide ambiguity is considered.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Receiver,
        });
    }
    if let Some(resolution) = resolve_using_member(store, snapshot, stored, caches)? {
        // A using-declaration in the owner's class states which base member a
        // bare name reaches; the class's own scope answers before the
        // namespaces around it do.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Receiver,
        });
    }
    if let Some(resolution) = resolve_object_member(store, snapshot, stored, caches)? {
        // The receiver is an object literal whose own property table says
        // which value this key holds; the method of that name was replaced by
        // a later property of the same key, so the general member rules would
        // bind the overwritten spelling.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::ObjectMember,
        });
    }
    if let Some(resolution) = resolve_qualified_name(snapshot, stored, &hits) {
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::QualifiedName,
        });
    }
    if let Some(resolution) = resolve_receiver_arity(stored, &hits) {
        // The receiver spells a declaring scope, so that scope's own
        // overloads stay distinguishable by their parameter counts.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::ArityBound,
        });
    }
    if stored.reference.receiver_name.is_some() {
        // A receiver this sweep could not pin — not to the owner's class
        // hierarchy, not to a declared type, not to a scope its spelling
        // reproduces — says nothing about which member it reaches.
        // Workspace-wide uniqueness, exports and argument counts answer bare
        // names only; applying them to a member call writes a guess into the
        // graph, and a name that is unique today stops being unique as soon
        // as a second class declares it. Defer to a later sweep instead.
        return Ok(Outcome::Deferred);
    }
    bare_name_outcome(store, snapshot, stored, hits, caches)
}

/// The rules a bare name still allows once its candidates are known: the
/// file's own single declaration, then the import, export and arity bounds,
/// then ambiguity between what is left.
fn bare_name_outcome(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: Vec<&NameIndexEntry>,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Outcome> {
    if import_is_stale(store, snapshot, stored, &hits, caches)? {
        return Ok(Outcome::Deferred);
    }
    let visible = visible_scopes(snapshot, &stored.reference.owner_id);
    if let [hit] = hits.as_slice() {
        if hit.file_id != stored.file_id {
            return Ok(Outcome::Resolved {
                resolution: Resolution {
                    ref_id: stored.id,
                    target_id: called_target_id(snapshot, stored, hit),
                    provenance: EdgeProvenance::WorkspaceUnique,
                },
                rule: Rule::Unique,
            });
        }
        // Bare name, same file, single hit: the in-file symbol table skips
        // declaration entities, so a pending bare call can only mean this
        // one indexed location — provided the caller can see it. A definition
        // nested in another scope is a local there, and file membership alone
        // never makes it the caller's binding. A construction names the class
        // whose constructor answers it, which the class owns rather than the
        // caller's scope.
        if declares_in(hit, &visible) || construction_names(stored, hit) {
            // A call that reaches an overload taking a different number of
            // arguments names nothing the caller can see: the nearer scope
            // hides the unit-scope overload that would answer it, and binding
            // the hidden sibling would report the wrong symbol.
            if matches!(stored.language.as_str(), "c" | "cpp")
                && stored.reference.ref_kind == GraphEdgeKind::Calls
                && !compatible_arity(
                    stored
                        .reference
                        .arity
                        .and_then(|arity| u32::try_from(arity).ok()),
                    hit.arity,
                )
            {
                return Ok(Outcome::Ambiguous(vec![hit.entity_id.clone()]));
            }
            return Ok(Outcome::Resolved {
                resolution: Resolution {
                    ref_id: stored.id,
                    target_id: called_target_id(snapshot, stored, hit),
                    provenance: EdgeProvenance::FileScoped,
                },
                rule: Rule::Unique,
            });
        }
    }
    if hits.len() > 1 {
        if let Some(outcome) = resolve_lexical_binding(snapshot, stored, &hits, &visible) {
            return Ok(outcome);
        }
        if let Some(resolution) =
            resolve_import_bound(snapshot, stored, &hits, &mut caches.importing_files)
        {
            return Ok(Outcome::Resolved {
                resolution,
                rule: Rule::ImportBound,
            });
        }
        if let Some(resolution) = resolve_export_bound(stored, &hits) {
            return Ok(Outcome::Resolved {
                resolution,
                rule: Rule::ExportBound,
            });
        }
        if let Some(resolution) = resolve_arity_bound(stored, &hits) {
            return Ok(Outcome::Resolved {
                resolution,
                rule: Rule::ArityBound,
            });
        }
        return Ok(Outcome::Ambiguous(
            hits.into_iter().map(|hit| hit.entity_id.clone()).collect(),
        ));
    }
    Ok(Outcome::Deferred)
}

/// Folds a fresh import target into this sweep's in-memory import scope.
fn record_import_scope(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    resolution: &Resolution,
    importing_files: &mut HashMap<u32, Vec<u32>>,
) {
    let Some(target_file) = resolution
        .target_id
        .strip_prefix('f')
        .and_then(|digits| digits.parse::<u32>().ok())
    else {
        return;
    };
    let modules = match importing_files.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(snapshot.imported_file_ids(stored.file_id).to_vec()),
    };
    if !modules.contains(&target_file) {
        modules.push(target_file);
    }
}

/// Drops name-index candidates whose file language cannot host the
/// reference. Every downstream rule — same-file, qualified, workspace
/// uniqueness, import/export/arity bounds, and the recorded candidate
/// sets — then reasons over same-language evidence only. The gate never
/// discards on missing information: unknown reference languages, missing
/// candidate paths, and unregistered extensions all keep their entries.
/// [`prefer_definitions`] runs right after it, so a definition only ever
/// supersedes a declaration of the reference's own language.
fn filter_compatible_hits<'s>(
    snapshot: &'s ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &'s [NameIndexEntry],
    candidate_formats: &mut HashMap<u32, Option<Vec<FileFormat>>>,
) -> Vec<&'s NameIndexEntry> {
    let reference = FileFormat::parse(&stored.language);
    let mut filtered = Vec::with_capacity(hits.len());
    for hit in hits {
        let formats = match candidate_formats.entry(hit.file_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(
                snapshot
                    .file_path(hit.file_id)
                    .map(|path| formats_for_path(Path::new(path))),
            ),
        };
        if language_compatible_formats(reference, formats.as_deref()) {
            filtered.push(hit);
        }
    }
    filtered
}

/// Drops candidates the reference's own translation unit cannot name.
///
/// A C/C++ entity with internal linkage — a member of an unnamed namespace or
/// a file-scope `static` declaration — has a name in its own translation unit
/// only. Another file that matches it by name matched a different declaration:
/// sharing a namespace scope and a spelling is not sharing the entity. The
/// rule reads the flag extraction records, so it only applies where the
/// language has the concept.
///
/// Textual inclusion joins a translation unit: a TU-local function in an
/// included header or `.cc` is inside every unit that includes it, directly or
/// through another header. Reachability therefore follows the include chain,
/// not file identity alone — which is also what keeps two sibling `static`
/// definitions of one name from borrowing each other's calls.
///
/// # Errors
/// Propagates graph storage failures of the include-chain read.
fn linkage_reachable<'s>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: Vec<&'s NameIndexEntry>,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Vec<&'s NameIndexEntry>> {
    if !matches!(stored.language.as_str(), "c" | "cpp") {
        return Ok(hits);
    }
    if !hits
        .iter()
        .any(|hit| hit.has_internal_linkage && hit.file_id != stored.file_id)
    {
        return Ok(hits);
    }
    if let Entry::Vacant(entry) = caches.include_closures.entry(stored.file_id) {
        entry.insert(include_closure(
            store,
            snapshot,
            &stored.language,
            stored.file_id,
        )?);
    }
    let Some(closure) = caches.include_closures.get(&stored.file_id) else {
        return Ok(hits);
    };
    Ok(hits
        .into_iter()
        .filter(|hit| {
            !hit.has_internal_linkage
                || hit.file_id == stored.file_id
                || closure.contains(&hit.file_id)
        })
        .collect())
}

/// Files the include chain of `file_id` reaches, transitively.
///
/// Two sources answer, because a sweep resolves references against one
/// snapshot taken before any of its own writeback: the import edges already
/// resolved in the graph, and the include statements still queued as pending
/// references, resolved here through the same path rule the import rule uses.
/// Without the second source a call and the include that makes it reachable
/// would resolve in the same sweep, and the call would be judged against a
/// unit that had not yet absorbed its header.
///
/// An include cycle keeps the walk finite: a file enters the set once, and the
/// file that started the walk is left out so the set answers "what does this
/// unit pull in" rather than "which units is it part of".
fn include_closure(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    language: &str,
    file_id: u32,
) -> crate::EngineResult<HashSet<u32>> {
    let mut reachable = HashSet::new();
    let mut frontier = vec![file_id];
    while let Some(current) = frontier.pop() {
        let mut targets = Vec::new();
        let path = snapshot.file_path(current).map(str::to_owned);
        for reference in store.import_references(current)? {
            if let Some(path) = path.as_deref()
                && let Some(target) =
                    import_target_file(snapshot, language, path, &reference.specifier)
            {
                targets.push(target);
            }
        }
        targets.extend(snapshot.imported_file_ids(current).iter().copied());
        for target in targets {
            if target != file_id && reachable.insert(target) {
                frontier.push(target);
            }
        }
    }
    Ok(reachable)
}

/// Narrows the candidates to the ones declared in a scope enclosing the
/// reference, when any are.
///
/// C and C++ search outward from the call: `ui::Paint` spelling `Widget` means
/// `ui::Widget` as soon as one exists, even when a `legacy::Widget` sits in the
/// same file and would win a whole-workspace tie-break. The scopes come from
/// the reference owner's qualified name, innermost first; an unscoped owner (a
/// file-level function) has only the global namespace and leaves the
/// candidates whole.
fn enclosing_scope_hits<'s>(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: Vec<&'s NameIndexEntry>,
) -> Vec<&'s NameIndexEntry> {
    if !matches!(stored.language.as_str(), "c" | "cpp") || hits.len() < 2 {
        return hits;
    }
    // A call that spells the scope it reaches (`Base::foo()`) states its own
    // binding: the caller's class is no evidence about where that scope is, and
    // narrowing to it would hide the member the call names.
    if spells_a_scope(stored) {
        return hits;
    }
    let Some(owner) = snapshot.entity_record(&stored.reference.owner_id) else {
        return hits;
    };
    let scopes = enclosing_scopes(&owner.qualified_name);
    if scopes.is_empty() {
        return hits;
    }
    let narrowed: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| {
            hit.qualified_name
                .rsplit_once("::")
                .is_some_and(|(scope, _)| scopes.contains(&scope))
        })
        .copied()
        .collect();
    if narrowed.is_empty() { hits } else { narrowed }
}

/// The scopes around a qualified name, innermost first: `a::b::f` yields
/// `["a::b", "a"]`.
fn enclosing_scopes(qualified_name: &str) -> Vec<&str> {
    let mut scopes = Vec::new();
    let mut current = qualified_name;
    while let Some((scope, _)) = current.rsplit_once("::") {
        scopes.push(scope);
        current = scope;
    }
    scopes
}

/// Whether the call site spells a declaring scope rather than a value:
/// `Base::foo()` versus `x.foo()`. The stored spelling is the callee as the
/// source wrote it, so a `::` before the member name marks the qualified form
/// (a leading `::` alone is the global qualifier of a bare name).
fn spells_a_scope(stored: &StoredPendingRef) -> bool {
    let Some(raw) = stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let callee = raw
        .split(['(', '<'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_start_matches("::");
    callee.contains("::")
}

/// Keeps only the candidates a call can reach.
///
/// A `Calls` reference names something invocable. A class, interface, enum,
/// module or alias is a type or a namespace, so a call that matches one has
/// matched a same-named declaration somewhere else: `struct PREFIX` for the
/// macro-shaped `PREFIX(scan)(x)`, or the class of a C++ `WithConstructor();`
/// statement that the grammar reads as a call expression although the language
/// declares a function. Construction is no exception: `new X()` and `X x;`
/// reach the constructor entity, which indexes as a function, so a type
/// candidate there is just as wrong.
///
/// A bare call (`relogin()`) spells no receiver, so a member cannot answer it.
/// A function-typed parameter shadows the package's method of the same name;
/// binding the method writes a call the code never makes. The scope is only
/// read as a type when the candidate's qualified name has an owner that the
/// index resolves to a class-like entity, which keeps nested functions
/// (`outer::work`) and module-scoped free functions (`ns::f`) callable.
///
/// Java is the exception: it has no package-level functions, so an
/// unqualified call is either an inherited member (implicit `this`) or a
/// statically imported one, and both are declared in a class. Excluding
/// members there would drop exactly the target those forms address, so the
/// bare-call rule stays off for Java and its class-scoped rules decide.
///
/// A construction site keeps type candidates: building a value is the one call
/// where naming a type is correct. The two forms differ. A call initializing a
/// value (`auto x = X(1)`, and every `new X()` in a language whose classes the
/// index does not mint constructors for) keeps them; C and C++ `new X()`
/// builds through a constructor entity, so a type candidate there is the class
/// the source never names.
///
/// Missing type evidence keeps the candidate: the gate never discards on
/// information the index does not carry.
fn callable_targets<'s>(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: Vec<&'s NameIndexEntry>,
    owner_is_type: &mut HashMap<String, bool>,
) -> Vec<&'s NameIndexEntry> {
    if stored.reference.ref_kind != GraphEdgeKind::Calls {
        return hits;
    }
    if let Some(site) = construction_site(stored) {
        let type_answers = site == "value" || !matches!(stored.language.as_str(), "c" | "cpp");
        if type_answers {
            return hits
                .into_iter()
                .filter(|hit| is_construction_target(hit))
                .collect();
        }
        // C and C++ `new X()` builds through the constructor the index mints
        // under `X::X`, so the class itself cannot answer the call. The
        // bare-call rule stays off too: it guards against borrowing a member of
        // the enclosing class, while a construction names its own type.
        return hits
            .into_iter()
            .filter(|hit| is_constructor_of(&stored.name_tail, &hit.qualified_name))
            .collect();
    }
    let bare = stored.reference.receiver_name.is_none() && stored.language != "java";
    hits.into_iter()
        .filter(|hit| is_callable_symbol(hit))
        .filter(|hit| !bare || !bare_call_borrows_a_member(snapshot, hit, owner_is_type))
        .collect()
}

/// Whether a candidate is the constructor a construction site names.
///
/// The index mints a constructor under its class's own name (`X::X`,
/// `ns::X::X`), so the last two segments repeat. The class itself keeps its
/// namespace as the owner and is not that entity.
fn is_constructor_of(name: &str, qualified_name: &str) -> bool {
    let Some((scope, tail)) = qualified_name.rsplit_once("::") else {
        return false;
    };
    tail == name && scope.rsplit("::").next() == Some(tail)
}

/// Whether the reference is a construction site that names the candidate's
/// class. The class owns its constructor, so such a site reaches it from any
/// scope, unlike a bare call to a function declared elsewhere in the file.
fn construction_names(stored: &StoredPendingRef, hit: &NameIndexEntry) -> bool {
    construction_site(stored).is_some()
        && matches!(stored.language.as_str(), "c" | "cpp")
        && is_constructor_of(&stored.name_tail, &hit.qualified_name)
}

/// The construction form extraction recorded for the reference: `"value"` for
/// a call initializing a value, `"new"` for a `new X()` site.
fn construction_site(stored: &StoredPendingRef) -> Option<&str> {
    stored
        .reference
        .metadata
        .get("constructs")
        .and_then(serde_json::Value::as_str)
}

/// Whether an entity can be what a construction site builds. A class is the
/// type itself; a module or an interface is never constructed, and an alias
/// names another entity rather than being one.
fn is_construction_target(hit: &NameIndexEntry) -> bool {
    !matches!(
        hit.symbol_type,
        Some(SymbolType::Alias | SymbolType::Interface | SymbolType::Module)
    )
}

/// Whether the entity a call names can be invoked. `None` means the index
/// recorded no kind, which is missing evidence rather than a negative one.
fn is_callable_symbol(hit: &NameIndexEntry) -> bool {
    !matches!(
        hit.symbol_type,
        Some(
            SymbolType::Alias
                | SymbolType::Class
                | SymbolType::Enum
                | SymbolType::Interface
                | SymbolType::Module
        )
    )
}

/// Whether `hit` is a member of a class-like scope, which a bare call cannot
/// address.
fn bare_call_borrows_a_member(
    snapshot: &ResolutionSnapshot,
    hit: &NameIndexEntry,
    owner_is_type: &mut HashMap<String, bool>,
) -> bool {
    let Some((scope, _)) = hit.qualified_name.rsplit_once("::") else {
        return false;
    };
    if scope.is_empty() {
        return false;
    }
    if let Some(is_type) = owner_is_type.get(scope) {
        return *is_type;
    }
    let is_type = scope_declares_a_type(snapshot, scope);
    owner_is_type.insert(scope.to_owned(), is_type);
    is_type
}

/// Whether one qualified scope has a class-like declaration in the index.
///
/// The name index is keyed by the last segment, so the lookup starts there and
/// keeps the candidate only when it spells the whole scope: a nested type is
/// addressed by its full `ns::Class` breadcrumb, and the bare tail would
/// otherwise match an unrelated top-level type of the same name.
fn scope_declares_a_type(snapshot: &ResolutionSnapshot, scope: &str) -> bool {
    let Some(tail) = scope.rsplit("::").next() else {
        return false;
    };
    snapshot.lookup_candidates(tail).is_some_and(|hits| {
        hits.iter().any(|hit| {
            hit.qualified_name == scope
                && matches!(
                    hit.symbol_type,
                    Some(SymbolType::Class | SymbolType::Enum | SymbolType::Interface)
                )
        })
    })
}

/// Whether a reference can address a candidate file's already-parsed formats.
/// The sweep reads each file's extension evidence once instead of per reference.
///
/// Missing evidence keeps the candidate: the gate never discards on
/// information it does not have. An unregistered extension is unknown
/// evidence just like a reference the walk could not tag with a language —
/// `unknown` is not a language of its own, so requiring an `unknown` candidate
/// to match it would drop every candidate spelling a real one.
fn language_compatible_formats(
    reference: Option<FileFormat>,
    formats: Option<&[FileFormat]>,
) -> bool {
    let (Some(reference), Some(formats)) = (reference, formats) else {
        return true;
    };
    if reference == FileFormat::Unknown {
        return true;
    }
    formats.iter().all(|format| *format == FileFormat::Unknown)
        || formats
            .iter()
            .any(|candidate| reference_compatible_with(reference, *candidate))
}

/// Drops the bodyless declarations a definition of the same symbol supersedes.
/// A C/C++ prototype and the definition carrying the same qualified name
/// describe one symbol, so the definition stands for both; only that spelling
/// pairs them, because a same-named entity under another qualified name — or
/// in another language — is a different symbol and must never hide the
/// declaration. A qualified name does not identify a C++ overload either, and
/// neither does an argument count: `void f(const char*);` and `void f(int) {}`
/// both take one argument, so hiding the declaration would leave the caller a
/// single candidate that the workspace-unique rule binds without evidence.
/// The pair therefore also needs agreement on the parameters themselves, and
/// when that evidence is missing both candidates stay, leaving the reference
/// ambiguous instead of pointing at the wrong implementation.
/// Call this after the language gate, never on the whole name index: shrinking
/// the shared set would let an unrelated language's definition erase the only
/// candidate a reference can bind.
fn prefer_definitions<T: Borrow<NameIndexEntry>>(hits: Vec<T>) -> Vec<T> {
    /// One definition's evidence: its counted parameters and the normalized
    /// parameter list that tells its overload apart.
    type Evidence = (Option<u32>, Option<String>);
    let mut defined: HashMap<String, Vec<Evidence>> = HashMap::new();
    for hit in &hits {
        let hit: &NameIndexEntry = hit.borrow();
        if !hit.is_declaration {
            defined
                .entry(hit.qualified_name.clone())
                .or_default()
                .push((hit.arity, hit.parameters.clone()));
        }
    }
    if defined.is_empty() {
        return hits;
    }
    hits.into_iter()
        .filter(|hit| {
            let hit: &NameIndexEntry = hit.borrow();
            !hit.is_declaration
                || !defined.get(&hit.qualified_name).is_some_and(|definitions| {
                    definitions.iter().any(|(arity, parameters)| {
                        same_symbol(
                            hit.arity,
                            hit.parameters.as_deref(),
                            *arity,
                            parameters.as_deref(),
                        )
                    })
                })
        })
        .collect()
}

/// Whether one qualified name's declaration and definition describe the same
/// symbol: compatible arities, and — for symbols that take parameters — the
/// same parameter list.
fn same_symbol(
    declaration_arity: Option<u32>,
    declaration_parameters: Option<&str>,
    definition_arity: Option<u32>,
    definition_parameters: Option<&str>,
) -> bool {
    if !compatible_arity(declaration_arity, definition_arity) {
        return false;
    }
    if declaration_arity.is_none() && definition_arity.is_none() {
        // A symbol that takes no parameters has no overload set to separate:
        // `class Foo;` and `class Foo {}` name one class.
        return true;
    }
    declaration_parameters
        .zip(definition_parameters)
        .is_some_and(|(declared, defined)| same_parameters(declared, defined))
}

/// Whether two parameter lists describe one overload. The stored lists carry
/// each parameter's type only — the name a declaration or definition spells is
/// dropped where the signature is normalized — so identity is exact equality:
/// `const char*` against `const char*` pairs, `unsigned long` against
/// `unsigned` and `const char*` against `int` do not.
fn same_parameters(declared: &str, defined: &str) -> bool {
    split_parameters(declared) == split_parameters(defined)
}

/// Splits a normalized parameter list at the commas that separate parameters,
/// leaving the ones nested in template arguments or function pointers.
fn split_parameters(list: &str) -> Vec<&str> {
    let mut parameters = Vec::new();
    let mut depth = 0usize;
    let mut begin = 0usize;
    for (offset, character) in list.char_indices() {
        match character {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parameters.push(list[begin..offset].trim());
                begin = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    parameters.push(list[begin..].trim());
    parameters
}

/// Settles a bare name among the candidates the calling file itself binds.
///
/// A bare name reads the lexical binding its file spells — a top-level
/// definition or one declared in an enclosing function — and that binding is
/// what the call or base clause means. Import, export and argument-count rules
/// answer the namespace a module publishes instead, so a name this file binds
/// is settled among this file's own candidates: one binding is the target,
/// and several leave the reference ambiguous rather than falling through to a
/// rule that describes a different scope — unless the call's argument count
/// picks one of the file's overloads. A receiver names a member, and an import
/// statement addresses a module, so neither reads a lexical binding.
fn resolve_lexical_binding(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    visible: &HashSet<&str>,
) -> Option<Outcome> {
    if stored.reference.receiver_name.is_some()
        || !matches!(
            stored.reference.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Extends | GraphEdgeKind::Implements
        )
    {
        return None;
    }
    // File membership is not visibility: a definition nested in another
    // function is a local there, and a call outside it reads its module or
    // import binding instead. Only the caller's own scope chain decides.
    let lexical: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.file_id == stored.file_id && shadows_import(snapshot, hit))
        .filter(|hit| declares_in(hit, visible))
        .copied()
        .collect();
    // One file may bind the name to several overloads (`send(int)` and
    // `send(int, int)`), and the call's argument count still names one of them:
    // the counts are read before the set is called ambiguous. Only a language
    // whose same-named definitions really are overloads has that reading — a
    // Python or JS script rebinds the name instead, so its later definition is
    // the only one the file has to offer and the count decides nothing.
    if lexical.len() > 1
        && matches!(stored.language.as_str(), "c" | "cpp")
        && let Some(expected) = stored.reference.arity
        && let Some(hit) = unique_arity_match(lexical.iter().copied(), expected)
    {
        return Some(Outcome::Resolved {
            resolution: Resolution {
                ref_id: stored.id,
                target_id: hit.entity_id.clone(),
                provenance: EdgeProvenance::ArityBound,
            },
            rule: Rule::ArityBound,
        });
    }
    match lexical.as_slice() {
        [] => None,
        [hit] => Some(Outcome::Resolved {
            resolution: Resolution {
                ref_id: stored.id,
                target_id: hit.entity_id.clone(),
                provenance: EdgeProvenance::FileScoped,
            },
            rule: Rule::Unique,
        }),
        _ if stored.language == "cpp" && stored.reference.ref_kind == GraphEdgeKind::Calls => {
            let nearest = lexical.iter().map(|hit| hit.qualified_name.len()).max()?;
            let nearest: Vec<_> = lexical
                .into_iter()
                .filter(|hit| hit.qualified_name.len() == nearest)
                .collect();
            Some(
                stored
                    .reference
                    .arity
                    .and_then(|arity| unique_arity_match(nearest.iter().copied(), arity))
                    .map_or_else(
                        || {
                            Outcome::Ambiguous(
                                nearest.iter().map(|hit| hit.entity_id.clone()).collect(),
                            )
                        },
                        |hit| Outcome::Resolved {
                            resolution: Resolution {
                                ref_id: stored.id,
                                target_id: hit.entity_id.clone(),
                                provenance: EdgeProvenance::ArityBound,
                            },
                            rule: Rule::ArityBound,
                        },
                    ),
            )
        }
        _ => Some(Outcome::Ambiguous(
            lexical.iter().map(|hit| hit.entity_id.clone()).collect(),
        )),
    }
}

/// Whether two arities can describe one symbol. A side without a counted
/// parameter list carries no evidence, so it neither confirms nor refutes the
/// pair; two counted lists must agree.
fn compatible_arity(left: Option<u32>, right: Option<u32>) -> bool {
    left.is_none() || right.is_none() || left == right
}

/// Resolves an import specifier onto the target file's endpoint
/// `f{file_id}`: TS/JS spell file-relative specifiers (`./x`, `../y`),
/// while Python spells dotted module paths that address the workspace root
/// (`from declare import x` addresses `declare.py` there). Bare package
/// names and alias roots stay pending: they address modules outside the
/// workspace snapshot.
fn resolve_import(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> Option<Resolution> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Imports {
        return None;
    }
    let path = match importing_paths.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(snapshot.file_path(stored.file_id).map(str::to_owned))
            .clone(),
    };
    let Some(path) = path else {
        // A file without a persisted path cannot anchor a relative specifier.
        return None;
    };
    let file_id = import_target_file(
        snapshot,
        &stored.language,
        &path,
        reference.ref_name.as_str(),
    )?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: format!("f{file_id}"),
        provenance: EdgeProvenance::ImportScoped,
    })
}

/// The file one import specifier addresses, relative to the importing file's
/// path, or `None` when the specifier names nothing the snapshot carries.
///
/// Shared by the import rule, the include-chain walk and the preprocessor
/// replay: an include is an import whose textual effect joins two translation
/// units, so both read a specifier the same way.
pub(super) fn import_target_file(
    snapshot: &ResolutionSnapshot,
    language: &str,
    importing_path: &str,
    specifier: &str,
) -> Option<u32> {
    let bases = match (language, specifier) {
        // A dotted Python specifier addresses the workspace root
        // (`from declare import x` reaches `declare.py`), except for the
        // leading-dot form, which climbs from the importing module's package.
        ("python", specifier) if specifier.starts_with('.') => {
            vec![python_relative_base(
                importing_dir(importing_path),
                specifier,
            )?]
        }
        ("python", specifier) => vec![join_specifier("", &specifier.replace('.', "/"))?],
        (_, specifier) if specifier.starts_with("./") || specifier.starts_with("../") => {
            let relative = join_specifier(importing_dir(importing_path), specifier)?;
            // A quoted `#include` is searched from the includer's directory
            // first and then from the build's include roots. Extraction keeps
            // the spelled form as the specifier, so the workspace root stands
            // in for those roots and the two searches are both tried.
            match join_specifier("", specifier) {
                Some(root) if matches!(language, "c" | "cpp") => vec![relative, root],
                _ => vec![relative],
            }
        }
        _ => return None,
    };
    bases
        .into_iter()
        .flat_map(|base| path_variants(&base, language))
        .find_map(|candidate| snapshot.file_id_by_path(&candidate))
}

/// Resolves an ambiguous bare-name reference through the file's imports:
/// when the imports pin the name to exactly one module and that module
/// declares it exactly once, the reference targets that definition.
/// Receiver-typed calls wait for receiver inference; the caller has already
/// set aside names a same-file lexical binding shadows, so every call that
/// reaches this rule reads its module's namespace.
fn resolve_import_bound(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    importing_files: &mut HashMap<u32, Vec<u32>>,
) -> Option<Resolution> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some()
        || !matches!(
            reference.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Extends | GraphEdgeKind::Implements
        )
    {
        return None;
    }
    let modules = match importing_files.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(snapshot.imported_file_ids(stored.file_id).to_vec())
            .clone(),
    };
    let scoped: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| modules.contains(&hit.file_id))
        .copied()
        .collect();
    if let [hit] = scoped.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: EdgeProvenance::ImportBound,
        });
    }
    None
}

/// The scopes a bare name inside `owner` can read: the module scope (the empty
/// prefix) plus every enclosing scope of the caller, its own included. A
/// reference the file node owns sees the module alone.
fn visible_scopes<'a>(snapshot: &'a ResolutionSnapshot, owner_id: &str) -> HashSet<&'a str> {
    let mut scopes = HashSet::from([""]);
    let Some(record) = snapshot.entity_record(owner_id) else {
        return scopes;
    };
    let mut scope = record.qualified_name.as_str();
    loop {
        scopes.insert(scope);
        match scope.rsplit_once("::") {
            Some((parent, _)) => scope = parent,
            None => break,
        }
    }
    scopes
}

/// Whether a candidate's declaring scope is one the caller can see. A
/// qualified name without an owner is a module binding, which every caller
/// in that module reads.
fn declares_in(hit: &NameIndexEntry, visible: &HashSet<&str>) -> bool {
    match hit.qualified_name.rsplit_once("::") {
        Some((owner, _)) => visible.contains(owner),
        None => true,
    }
}

/// Whether a same-file candidate is a lexical binding of the referenced
/// name, the namespace an import binding loses to.
///
/// A top-level definition lives in the module's own namespace, and one
/// declared inside a function or another module scope — Python's `outer::work`
/// is a local of `outer` — is a binding a bare call reads. A class member is
/// not: a method is reached through a receiver that a bare call never spells,
/// so it cannot shadow the import. The owner of the qualified name tells the
/// two apart, and any owner not proven to declare a type keeps the
/// conservative answer, because an unconfirmed member claim must not decide a
/// bind.
fn shadows_import(snapshot: &ResolutionSnapshot, hit: &NameIndexEntry) -> bool {
    let Some((owner, _)) = hit.qualified_name.rsplit_once("::") else {
        return true;
    };
    let tail = owner.rsplit("::").next().unwrap_or(owner);
    // Hidden candidates answer nothing: an owner in an unfinished file may be
    // another kind of scope, so the member claim stays unconfirmed.
    let Some(candidates) = snapshot.lookup_candidates(tail) else {
        return true;
    };
    let owners: Vec<&NameIndexEntry> = candidates
        .iter()
        .filter(|candidate| candidate.file_id == hit.file_id && candidate.qualified_name == owner)
        .collect();
    let member_scope = !owners.is_empty()
        && owners
            .iter()
            .all(|candidate| candidate.symbol_type.is_some_and(is_type_scope));
    !member_scope
}

/// Whether a scope of this symbol type declares members a receiver reaches,
/// rather than lexical bindings a bare name reads.
fn is_type_scope(symbol_type: SymbolType) -> bool {
    matches!(
        symbol_type,
        SymbolType::Class | SymbolType::Interface | SymbolType::Enum
    )
}

/// One `as` alias binding parsed from an import statement's raw text.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AliasBinding {
    /// Local name the importing file binds (`leftWork`, `left_work`).
    local: String,
    /// Name the import addresses inside the specifier's module (`work`),
    /// or `None` for module-namespace bindings (`import left as left_api`),
    /// which bare-name resolution cannot follow without a receiver type.
    imported: Option<String>,
    /// Module specifier the statement imports from.
    specifier: String,
}

/// Binds a bare call whose name is an `as` alias of an import: the alias
/// never names an indexed symbol itself, and the binding pins which module
/// the imported name addresses, so a unique same-module declaration of the
/// imported name is the call's target. Receiver-typed calls wait for
/// receiver inference and namespace bindings stay with the namespace rule;
/// specifiers addressing no snapshot module keep the general rules.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_alias_bound(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some() || reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(store, stored, caches)?;
    let matched: Vec<&AliasBinding> = file_bindings
        .iter()
        .filter(|binding| binding.imported.is_some() && binding.local == stored.name_tail)
        .collect();
    let [binding] = matched.as_slice() else {
        return Ok(None);
    };
    Ok(bind_in_module(
        snapshot,
        stored,
        binding.imported.as_deref().expect("imported binding"),
        binding,
        &mut caches.importing_paths,
        &mut caches.alias_targets,
    ))
}

/// Binds a call whose receiver names a module-namespace binding
/// (`import left as left_api`, then `left_api.work()`): the receiver is
/// the module object itself, so a unique declaration of the invoked member
/// inside that one module is the call's target. Namespace imports of no
/// snapshot module and members the module declares ambiguously keep the
/// reference with the general rules.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_namespace_bound(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(store, stored, caches)?;
    let matched: Vec<&AliasBinding> = file_bindings
        .iter()
        .filter(|binding| binding.imported.is_none() && binding.local == receiver)
        .collect();
    let [binding] = matched.as_slice() else {
        return Ok(None);
    };
    Ok(bind_in_module(
        snapshot,
        stored,
        &stored.name_tail,
        binding,
        &mut caches.importing_paths,
        &mut caches.alias_targets,
    ))
}

/// Cached alias bindings of the reference's own file.
///
/// # Errors
/// Propagates graph storage failures.
fn load_bindings(
    store: &IndexStore,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Vec<AliasBinding>> {
    load_bindings_in(
        store,
        stored.language.as_str(),
        stored.file_id,
        &mut caches.alias_bindings,
    )
}

/// [`load_bindings`] for one file, cached the same way.
///
/// # Errors
/// Propagates graph storage failures.
fn load_bindings_in(
    store: &IndexStore,
    language: &str,
    file_id: u32,
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
) -> crate::EngineResult<Vec<AliasBinding>> {
    Ok(match bindings.entry(file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(load_alias_bindings_in(store, language, file_id)?)
            .clone(),
    })
}

/// Binds the addressed member to the one declaration inside the binding's
/// module: the import pins the module, so a unique same-module hit is the
/// target. A specifier addressing no snapshot module and members the
/// module declares ambiguously keep the reference with the general rules.
fn bind_in_module(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    imported: &str,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> Option<Resolution> {
    let target_file = alias_target_file(snapshot, stored, binding, importing_paths, targets)?;
    let Some(raw_hits) = snapshot.lookup_candidates(imported) else {
        // Hidden candidates must not turn the binding into a seemingly
        // unique match against an unfinished snapshot.
        return None;
    };
    let scoped: Vec<&NameIndexEntry> = raw_hits
        .iter()
        .filter(|hit| hit.file_id == target_file)
        .collect();
    // A module may hold a prototype and the definition of one symbol; the
    // definition stands for both, so the uniqueness check below must not see
    // two candidates for it.
    let scoped = prefer_definitions(scoped);
    if let [hit] = scoped.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: called_target_id(snapshot, stored, hit),
            provenance: EdgeProvenance::ImportBound,
        });
    }
    None
}

/// The entity a call reaches once a rule has picked the declaration to bind.
/// A declaration that is another spelling of a value
/// (`const dispatch = send`, `export const dispatch = send`) forwards to the
/// value it names, so the edge lands on the function the alias stands for
/// instead of on the alias itself.
///
/// A member whose key a later property of the same object literal replaces
/// (`{ run() {}, run: right }`) forwards to the replacement: the method the
/// index recorded is not what the source's last word on that key reaches.
///
/// Non-call references keep the declaration they matched: an inheritance edge
/// names the class, which neither an alias nor an overwritten property speaks
/// for.
fn called_target_id(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hit: &NameIndexEntry,
) -> String {
    let is_call = stored.reference.ref_kind == GraphEdgeKind::Calls;
    let bare_call = is_call && stored.reference.receiver_name.is_none();
    if bare_call && let Some(target) = follow_alias(snapshot, hit) {
        return target.entity_id.clone();
    }
    if is_call && let Some(target) = follow_overwrite(snapshot, hit) {
        return target.entity_id.clone();
    }
    hit.entity_id.clone()
}

/// Follows the replacement a later property of the same key names inside the
/// object literal: `{ run() {}, run: right }` indexes the method as `api::run`,
/// yet the literal's last `run` holds `right`, so a call written through the
/// key reaches `right`.
///
/// Only a unique same-module declaration of the replacement name is followed;
/// when the module declares that name ambiguously, the indexed member is kept
/// rather than guessing which value the literal meant.
fn follow_overwrite<'a>(
    snapshot: &'a ResolutionSnapshot,
    hit: &NameIndexEntry,
) -> Option<&'a NameIndexEntry> {
    let replacement = hit.overwritten_by.as_deref()?;
    let raw_hits = snapshot.lookup_candidates(replacement)?;
    let scoped: Vec<&NameIndexEntry> = raw_hits
        .iter()
        .filter(|candidate| candidate.file_id == hit.file_id)
        .collect();
    match prefer_definitions(scoped).as_slice() {
        [target] => Some(*target),
        _ => None,
    }
}

/// Follows one link of a value alias inside the module that declares it: a
/// module exporting `const dispatch = send` makes `dispatch` another spelling
/// of `send`, so a call written through `dispatch` reaches `send`.
///
/// Only a unique same-module declaration of the aliased name is followed; when
/// the module declares that name ambiguously, the alias's own declaration is
/// kept rather than guessing which one the source meant.
fn follow_alias<'a>(
    snapshot: &'a ResolutionSnapshot,
    hit: &NameIndexEntry,
) -> Option<&'a NameIndexEntry> {
    let aliased = hit.alias_of.as_deref()?;
    let raw_hits = snapshot.lookup_candidates(aliased)?;
    let scoped: Vec<&NameIndexEntry> = raw_hits
        .iter()
        .filter(|candidate| candidate.file_id == hit.file_id)
        .collect();
    match prefer_definitions(scoped).as_slice() {
        [target] => Some(*target),
        _ => None,
    }
}

/// Extracts `(local, imported)` pairs from one stored import statement;
/// `None` marks a module-level or namespace binding.
type AliasPairs = fn(&str) -> Vec<(String, Option<String>)>;

/// Statement text with trailing line comments removed.
///
/// Tree-sitter's import nodes already exclude comments, so this only guards raw
/// text that still carries one: a comment spelling ` as ` or ` from ` would
/// otherwise leak into a binding's local name or cut the clause in the wrong
/// place.
fn without_line_comments<'a>(statement: &'a str, marker: &str) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;
    if !statement.contains(marker) {
        return Cow::Borrowed(statement);
    }
    Cow::Owned(
        statement
            .lines()
            .map(|line| line.split_once(marker).map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// One local name an import statement binds, with the specifier that names
/// its module.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportBinding {
    local: String,
    specifier: String,
}

/// Whether the bare name reaches a definition only because an import
/// statement of its own file no longer names anything.
///
/// The statement is the only evidence for what the name stands for: it binds
/// the local spelling and names the module. Once that module is gone the
/// import is stale, and a same-spelled definition elsewhere in the workspace
/// is a different symbol — uniqueness, exports and arities must not answer
/// for it. Keeping the reference pending lets a later import edit place it.
/// A module the workspace still holds keeps the general rules, which place it
/// by path where they can and by workspace uniqueness where a package root
/// hides it from them.
///
/// # Errors
/// Propagates graph storage failures.
fn import_is_stale(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<bool> {
    if !import_names_a_missing_module(store, snapshot, stored, caches)? {
        return Ok(false);
    }
    // A candidate inside a module this file does import still answers the
    // name: the stale statement is not the only binding evidence.
    Ok(!hits_reach_an_imported_module(
        snapshot, stored, hits, caches,
    ))
}

/// Whether a bare name an import statement binds names a module the
/// workspace no longer contains.
///
/// # Errors
/// Propagates graph storage failures.
fn import_names_a_missing_module(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<bool> {
    load_import_names(store, stored, caches)?;
    let bindings = caches
        .import_names
        .get(&stored.file_id)
        .map_or(&[][..], Vec::as_slice);
    let matched: Vec<&ImportBinding> = bindings
        .iter()
        .filter(|binding| binding.local == stored.name_tail)
        .collect();
    if matched.is_empty() {
        return Ok(false);
    }
    if matched
        .iter()
        .any(|binding| !module_missing(snapshot, stored, &binding.specifier))
    {
        return Ok(false);
    }
    Ok(true)
}

/// Whether the specifier addresses a module no indexed file holds.
///
/// Only module-path imports answer this: a Python dotted name is looked up as
/// a module stem anywhere in the workspace (a package root the path rules do
/// not know still counts), and a relative `./x` is looked up beside its
/// importer. Every other specifier — a bare package, an alias — names nothing
/// the workspace can be asked about.
fn module_missing(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    specifier: &str,
) -> bool {
    match stored.language.as_str() {
        "python" => {
            let module = specifier.rsplit('.').next().unwrap_or(specifier).trim();
            !module.is_empty() && !snapshot.has_python_module(module)
        }
        "typescript" | "javascript" => {
            if !specifier.starts_with('.') {
                return false;
            }
            let Some(importer) = snapshot.file_path(stored.file_id) else {
                return false;
            };
            let base = std::path::Path::new(importer).parent().map_or_else(
                || std::path::PathBuf::from(specifier),
                |dir| dir.join(specifier),
            );
            let base = base.to_string_lossy().replace('\\', "/");
            // `Path::join` keeps a `./` segment, and a specifier that walks
            // parents spells `src/services/../all`; the snapshot only holds
            // normalized paths, so the lookup must be spelled the same way.
            let base = normalize_path(&base);
            !snapshot.has_path_under(&base)
        }
        _ => false,
    }
}

/// One path spelling with empty and `.` segments dropped and `..` folded
/// into its parent, so two spellings of one file compare equal.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Whether one of the candidates lives in a module the reference's file
/// imports, or in the file itself.
fn hits_reach_an_imported_module(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caches: &mut RuleCaches,
) -> bool {
    let modules = imported_modules(snapshot, stored.file_id, caches);
    hits.iter()
        .any(|hit| hit.file_id == stored.file_id || modules.contains(&hit.file_id))
}

/// Imports of one file, read once per sweep.
fn imported_modules(
    snapshot: &ResolutionSnapshot,
    file_id: u32,
    caches: &mut RuleCaches,
) -> Vec<u32> {
    match caches.importing_files.entry(file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(snapshot.imported_file_ids(file_id).to_vec())
            .clone(),
    }
}

/// The local names one file's import statements bind, read once per sweep.
///
/// # Errors
/// Propagates graph storage failures.
fn load_import_names(
    store: &IndexStore,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<()> {
    if caches.import_names.contains_key(&stored.file_id) {
        return Ok(());
    }
    let mut bound = Vec::new();
    for reference in store.import_references(stored.file_id)? {
        let Some(raw_text) = reference.raw_text.as_deref() else {
            continue;
        };
        for local in import_bound_names(&stored.language, raw_text) {
            bound.push(ImportBinding {
                local,
                specifier: reference.specifier.clone(),
            });
        }
    }
    caches.import_names.insert(stored.file_id, bound);
    Ok(())
}

/// The names one import statement binds in the importing file: the local
/// spelling a call reads (`from m import a, b as c` binds `a` and `c`).
/// A `*` import binds nothing by name.
///
/// Only module-path languages are parsed: a Rust `use` and a Java `import`
/// name an item, and their rules answer the call before the fallbacks run.
fn import_bound_names(language: &str, statement: &str) -> Vec<String> {
    match language {
        "python" => python_bound_names(without_line_comments(statement, "#").trim()),
        "typescript" | "javascript" => {
            ts_bound_names(without_line_comments(statement, "//").trim())
        }
        _ => Vec::new(),
    }
}

/// `from m import a, b as c` binds `a` and `c`; `import a.b` binds `a`;
/// `import m as n` binds `n`.
fn python_bound_names(statement: &str) -> Vec<String> {
    if let Some(rest) = statement.strip_prefix("from ") {
        let Some((_, names)) = rest.split_once(" import ") else {
            return Vec::new();
        };
        let names = names.trim();
        let names = names
            .strip_prefix('(')
            .map_or(names, |inner| inner.strip_suffix(')').unwrap_or(inner));
        return names.split(',').filter_map(bound_local).collect();
    }
    let Some(rest) = statement.strip_prefix("import ") else {
        return Vec::new();
    };
    rest.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            match entry.split_once(" as ") {
                Some((_, local)) => bound_local(local),
                // `import a.b` binds the root package.
                None => bound_local(entry.split('.').next().unwrap_or(entry)),
            }
        })
        .collect()
}

/// `import d, { a, b as c } from './x'` binds `d`, `a` and `c`;
/// `import * as ns from './x'` binds `ns`.
fn ts_bound_names(statement: &str) -> Vec<String> {
    let Some(rest) = statement.strip_prefix("import ") else {
        return Vec::new();
    };
    let clause = rest.split_once(" from ").map_or(rest, |(clause, _)| clause);
    let clause = clause.trim();
    // `import type { T }` binds a type alone; a call never reads it, but the
    // entry still names a local spelling.
    let clause = clause.strip_prefix("type ").unwrap_or(clause);
    clause
        .replace(['{', '}'], " ")
        .split(',')
        .filter_map(bound_local)
        .filter(|local| local != "*")
        .collect()
}

/// The local name a bare import entry binds, or `None` for an entry that
/// names nothing by itself (empty, `*`).
fn bound_local(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() || entry == "*" {
        return None;
    }
    let local = entry
        .split_once(" as ")
        .map_or(entry, |(_, local)| local.trim());
    (!local.is_empty() && local != "*").then(|| local.to_owned())
}

/// Loads a file's import edges and parses their statement text into `as` alias
/// bindings. Only statements spelling an explicit `as` produce bindings:
/// unaliased imports keep flowing through the bare-name and import-bound
/// rules, which already resolve them.
///
/// The file need not be the reference's own: a property value is spelled in the
/// module that declares the object literal holding it.
///
/// # Errors
/// Propagates graph storage failures.
fn load_alias_bindings_in(
    store: &IndexStore,
    language: &str,
    file_id: u32,
) -> crate::EngineResult<Vec<AliasBinding>> {
    let pairs: AliasPairs = match language {
        "typescript" | "javascript" => ts_alias_pairs,
        "python" => python_alias_pairs,
        "rust" => rust_alias_pairs,
        _ => return Ok(Vec::new()),
    };
    let mut bindings = Vec::new();
    for reference in store.import_references(file_id)? {
        let Some(raw_text) = reference.raw_text.as_deref() else {
            continue;
        };
        for (local, imported) in pairs(raw_text) {
            bindings.push(AliasBinding {
                local,
                imported,
                specifier: reference.specifier.clone(),
            });
        }
    }
    Ok(bindings)
}

/// Parses an import statement into aliased `(local, imported)` pairs and
/// namespace bindings: `import { work as leftWork } from './left'` pairs a
/// local alias with the imported name, `import * as leftApi from './left'`
/// binds the module itself (`imported` stays `None`). Default imports and
/// unaliased names never spell a binding.
fn ts_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    if !statement.trim_start().starts_with("import ") {
        return Vec::new();
    }
    let statement = without_line_comments(statement, "//");
    let statement = statement.as_ref();
    let Some(clause_end) = statement.rfind(" from ") else {
        return Vec::new();
    };
    let clause = statement[..clause_end].trim();
    let Some(open) = clause.find('{') else {
        let Some(rest) = clause.strip_prefix("import ") else {
            return Vec::new();
        };
        let rest = rest.trim();
        let Some(as_index) = rest.find(" as ") else {
            return Vec::new();
        };
        if rest[..as_index].trim() != "*" {
            return Vec::new();
        }
        let local = rest[as_index + " as ".len()..].trim();
        if local.is_empty() {
            return Vec::new();
        }
        return vec![(local.to_owned(), None)];
    };
    let Some(close) = clause.rfind('}') else {
        return Vec::new();
    };
    if close < open {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    for entry in clause[open + 1..close].split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        // `default as x` addresses the module's default export, which no
        // named lookup can reach.
        if imported.is_empty() || local.is_empty() || imported == "default" {
            continue;
        }
        pairs.push((local.to_owned(), Some(imported.to_owned())));
    }
    pairs
}

/// Parses `from declare import func as foobar` statements into aliased
/// `(local, imported)` pairs, and `import left as left_api` statements into
/// namespace bindings whose imported half stays `None`. Unaliased forms
/// never spell an `as`, so they stay unparsed.
fn python_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    let statement = without_line_comments(statement, "#");
    let statement = statement.as_ref();
    let trimmed = statement.trim_start();
    // `from m import ...` spells statement-level names; a leading `import`
    // binds the module itself as a namespace.
    if !trimmed.starts_with("from ") {
        let Some(names) = trimmed.strip_prefix("import ") else {
            return Vec::new();
        };
        return python_namespace_pairs(names);
    }
    let Some(import_index) = statement.find(" import ") else {
        return Vec::new();
    };
    let names = statement[import_index + " import ".len()..].trim();
    let names = names.strip_prefix('(').map_or(names, |stripped| {
        stripped.strip_suffix(')').unwrap_or(stripped)
    });
    let mut pairs = Vec::new();
    for entry in names.split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        if imported.is_empty() || local.is_empty() {
            continue;
        }
        pairs.push((local.to_owned(), Some(imported.to_owned())));
    }
    pairs
}

/// Parses comma-separated `module as alias` entries of a plain `import`
/// statement into namespace bindings.
fn python_namespace_pairs(names: &str) -> Vec<(String, Option<String>)> {
    let mut pairs = Vec::new();
    for entry in names.trim().trim_end_matches(';').split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        if imported.is_empty() || local.is_empty() {
            continue;
        }
        pairs.push((local.to_owned(), None));
    }
    pairs
}

/// Parses `use left::work as left_work;` statements into aliased
/// `(local, imported)` pairs. Brace groups (`use a::{b as c}`) need
/// per-item module paths the statement text alone cannot pin, so they
/// stay unparsed.
fn rust_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    let statement = without_line_comments(statement, "//");
    let statement = statement.as_ref();
    if statement.contains('{') {
        return Vec::new();
    }
    let Some(as_index) = statement.find(" as ") else {
        return Vec::new();
    };
    let Some(path) = statement[..as_index].strip_prefix("use ") else {
        return Vec::new();
    };
    let path = path.trim();
    let local = statement[as_index + " as ".len()..]
        .trim()
        .trim_end_matches(';')
        .trim();
    let Some((module, imported)) = path.rsplit_once("::") else {
        return Vec::new();
    };
    if module.is_empty() || imported.is_empty() || local.is_empty() {
        return Vec::new();
    }
    vec![(local.to_owned(), Some(imported.to_owned()))]
}

/// Resolves an alias binding's module specifier onto the file it imports:
/// TS/JS specifiers are file-relative, Python dotted paths are root
/// relative, and Rust use paths name their module with the first segment
/// (a leading `crate::` addresses the workspace root). `None` keeps the
/// reference with the general rules.
///
/// The sweep shares this memo, so it keys on the normalized workspace-relative
/// base rather than the raw specifier: `./util` addresses a different module
/// in every directory.
fn alias_target_file(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> Option<u32> {
    alias_target_file_in(
        snapshot,
        stored.language.as_str(),
        stored.file_id,
        binding,
        importing_paths,
        targets,
    )
}

/// [`alias_target_file`] for a binding read from `file_id`, which need not be
/// the reference's own file: an object literal names its property values in
/// the module that declares it, not in the module that calls through it.
fn alias_target_file_in(
    snapshot: &ResolutionSnapshot,
    language: &str,
    file_id: u32,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> Option<u32> {
    let base = alias_target_base_in(snapshot, language, file_id, binding, importing_paths)?;
    match targets.entry((language.to_owned(), base.clone())) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => *entry.insert(alias_target_at(snapshot, language, &base)),
    }
}

/// Workspace-relative module path a binding addresses, before extension
/// probing: TS/JS specifiers join the importing file's directory, while
/// Python's dotted paths and Rust's leading module segment address the
/// workspace root. `None` for specifiers no rule can place.
fn alias_target_base_in(
    snapshot: &ResolutionSnapshot,
    language: &str,
    file_id: u32,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> Option<String> {
    let module: String = match language {
        "typescript" | "javascript" => {
            if binding.specifier != "."
                && !binding.specifier.starts_with("./")
                && !binding.specifier.starts_with("../")
            {
                return None;
            }
            if matches!(binding.specifier.as_str(), "." | "./") {
                "./index".to_owned()
            } else {
                binding.specifier.clone()
            }
        }
        "python" => {
            if binding.specifier.starts_with('.') {
                return None;
            }
            binding.specifier.replace('.', "/")
        }
        "rust" => {
            let crate_path = binding
                .specifier
                .strip_prefix("crate::")
                .unwrap_or(&binding.specifier);
            crate_path
                .split("::")
                .find(|segment| !segment.is_empty())?
                .to_owned()
        }
        _ => return None,
    };
    match language {
        "typescript" | "javascript" => {
            let path = match importing_paths.entry(file_id) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => entry
                    .insert(snapshot.file_path(file_id).map(str::to_owned))
                    .clone(),
            };
            join_specifier(importing_dir(&path?), &module)
        }
        // Python and Rust specifiers address the workspace root.
        _ => Some(module),
    }
}

/// Probes one normalized module base for the indexed file behind it: the
/// literal path, then per-language extensions and index files.
fn alias_target_at(snapshot: &ResolutionSnapshot, language: &str, base: &str) -> Option<u32> {
    let mut variants = path_variants(base, language);
    if language == "rust" {
        // Rust modules live in `<base>.rs` or `<base>/mod.rs`; the index
        // variant path_variants emits covers neither.
        variants.push(format!("{base}/mod.rs"));
    }
    variants
        .iter()
        .find_map(|candidate| snapshot.file_id_by_path(candidate))
}

/// Receivers naming the enclosing instance rather than a named local: `self`
/// (Rust, Python), `cls` (Python class methods), and `this` (C-family, Java,
/// JavaScript). Both receiver rules read this one list so a language can
/// never be taught a spelling in only one of them.
fn is_self_receiver(language: &str, receiver: &str) -> bool {
    match language {
        "python" => matches!(receiver, "self" | "cls"),
        _ => matches!(receiver, "self" | "this"),
    }
}

/// One class a member call is searched in.
///
/// The name is the spelling hit qualified names are built from, so it stays
/// the primary key: extraction joins the declaring breadcrumb with `::`, and
/// both the enclosing class and its bases are named by that string. The name
/// alone cannot tell two modules apart, though — `caller::Target` and
/// `decoy::Target` both spell `Target::reset` — so `declared_in` records the
/// file whose declaration of that name this scope means. Whatever the index
/// cannot place leaves the comparison at the name alone, which keeps the
/// rule's older, coarser evidence instead of dropping a candidate on
/// information it does not have.
#[derive(Clone)]
struct ClassScope {
    name: String,
    /// The file declaring the class. A class name is spelled the same in every
    /// module that declares it, so the file is the evidence that separates
    /// them; `None` when the index cannot place the name in one file.
    declared_in: Option<u32>,
    /// The class entity an inheritance walk starts from, when the index places
    /// exactly one. A language that indexes an `impl` block as a second entity
    /// of the same name leaves this `None` while `declared_in` still answers.
    entity: Option<String>,
}

impl ClassScope {
    /// The scope a class name means at a point that names `file_hint` as the
    /// file it is written in.
    fn at(snapshot: &ResolutionSnapshot, name: String, file_hint: Option<u32>) -> Self {
        Self {
            declared_in: class_declaration_file(snapshot, &name, file_hint),
            entity: class_entity_id(snapshot, &name),
            name,
        }
    }

    /// Whether both scopes address the same class. A declaring file decides
    /// whenever both are known; the name alone is the fallback for scopes the
    /// index cannot place.
    fn same_as(&self, other: &Self) -> bool {
        self.name == other.name
            && match (self.declared_in, other.declared_in) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    }
}

/// The candidates declaring a member inside `scope`, recognized by the
/// qualified name extraction builds from the declaring breadcrumb. A candidate
/// whose declaring class is a namesake declared in another file is not a member
/// of `scope` at all, however equal the two names look.
fn declarations_in<'a>(
    snapshot: &ResolutionSnapshot,
    hits: &[&'a NameIndexEntry],
    scope: &ClassScope,
) -> Vec<&'a NameIndexEntry> {
    hits.iter()
        .filter(|hit| {
            let Some((declaring, _)) = hit.qualified_name.rsplit_once("::") else {
                return false;
            };
            // A class template's name is one name however its arguments are
            // spelled: `omt_node<omtdata_t, true>::get_marked` is a member of
            // `omt_node`, the scope the bound type names.
            if type_head(declaring) != type_head(&scope.name) {
                return false;
            }
            match (
                scope.declared_in,
                class_declaration_file(snapshot, declaring, Some(hit.file_id)),
            ) {
                (Some(expected), Some(actual)) => actual == expected,
                _ => true,
            }
        })
        .copied()
        .collect()
}

/// The resolution a member rule writes: the declaring scope came from the
/// hierarchy, so the edge makes no workspace-wide claim.
fn member_resolution(stored: &StoredPendingRef, hit: &NameIndexEntry) -> Resolution {
    Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: EdgeProvenance::FileScoped,
    }
}

/// Binds a member call to the nearest declaration among `scopes`, which the
/// caller orders by precedence: the receiver's own class first, then its
/// bases by distance. The closest scope declaring the member decides, since
/// an override hides the declaration its base would contribute — the one
/// declaration there binds, while several (overloads this rule cannot
/// separate) leave the call to the rules below rather than binding an
/// ancestor the override replaced.
fn bind_nearest_member(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    scopes: &[ClassScope],
) -> Option<Resolution> {
    for scope in scopes {
        let found = declarations_in(snapshot, hits, scope);
        match found.as_slice() {
            [] => {}
            [hit] => return Some(member_resolution(stored, hit)),
            _ => return None,
        }
    }
    None
}

/// Binds a member call across the hierarchy `caller` heads, reading `scopes`
/// in the order the walk produced.
///
/// That order is distance from the caller, which reproduces the lookup order
/// of single inheritance: the caller, then its base, then the base's base. A
/// class with several bases leaves the order to the language — Python
/// linearizes them (C3), so `Child(A, B)` with `A(Grand)` searches
/// `Child, A, Grand, B` while this walk offers `Child, A, B, Grand` and would
/// bind `B.save` over `Grand.save`. Where the walk cannot reproduce that
/// order it declines to choose between the bases.
fn bind_hierarchy_member(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caller: Option<&ClassScope>,
    scopes: &[ClassScope],
    branched: bool,
) -> Option<Resolution> {
    if branched && stored.language == "python" {
        return bind_sole_declaration(snapshot, stored, hits, caller, scopes);
    }
    bind_nearest_member(snapshot, stored, hits, scopes)
}

/// Binds a member call whose language searches the hierarchy in an order this
/// walk cannot reproduce.
///
/// The caller heads every order a language can choose, and a member exactly
/// one base declares is reached by every order too, so both still bind. Only
/// several bases declaring it make the answer a function of the order, which
/// stays pending rather than binding the base the walk happens to reach first.
fn bind_sole_declaration(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caller: Option<&ClassScope>,
    scopes: &[ClassScope],
) -> Option<Resolution> {
    if let Some(caller) = caller {
        match declarations_in(snapshot, hits, caller).as_slice() {
            [] => {}
            [hit] => return Some(member_resolution(stored, hit)),
            _ => return None,
        }
    }
    let mut declaration: Option<&NameIndexEntry> = None;
    for scope in scopes {
        if caller.is_some_and(|caller| caller.same_as(scope)) {
            continue;
        }
        match declarations_in(snapshot, hits, scope).as_slice() {
            [] => {}
            [hit] => {
                if declaration.is_some() {
                    return None;
                }
                declaration = Some(hit);
            }
            _ => return None,
        }
    }
    declaration.map(|hit| member_resolution(stored, hit))
}

/// Resolves a receiver call that names the enclosing instance (`self`,
/// `this`, Python's `cls`) or the superclass (`super`): the receiver's type
/// is the class owning the call site, so the invoked member's declaring
/// scope must be that class or — through the class's extends/implements
/// edges — one of its bases. `super` skips the enclosing class and
/// addresses the direct bases only. A single-inheritance hierarchy is
/// searched nearest first, so an override wins over the base method it
/// replaces; one that declares several bases leaves the order to the
/// language, and only the conclusions every order shares still bind (see
/// `bind_hierarchy_member`). Declarations the deciding scope holds
/// ambiguously and receivers of classes without a persisted owner record
/// stay pending.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_receiver_class(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let receiver = match reference.receiver_name.as_deref() {
        Some(receiver) => receiver,
        None if stored.language == "java" => "this",
        None => return Ok(None),
    };
    let superclass = receiver == "super";
    if !superclass && !is_self_receiver(&stored.language, receiver) {
        return Ok(None);
    }
    let Some(owner) = snapshot.entity_record(&reference.owner_id) else {
        return Ok(None);
    };
    // The enclosing class is the owner's qualified-name scope; an owner
    // without one is the class itself (static initializers), while a
    // `super` receiver of such an owner has no base to address.
    let Some(enclosing) = (match owner.qualified_name.rsplit_once("::") {
        Some((scope, _)) => Some(scope.to_owned()),
        None if !superclass => Some(owner.qualified_name.clone()),
        None => None,
    }) else {
        return Ok(None);
    };
    if stored.language == "rust" {
        return resolve_rust_self(store, snapshot, stored, hits, &enclosing, caches);
    }
    // The call site is written inside this class, so its own file is the
    // evidence that tells the class apart from a namesake in another module.
    let root = ClassScope::at(snapshot, enclosing, Some(stored.file_id));
    let mut scopes = if superclass {
        Vec::new()
    } else {
        vec![root.clone()]
    };
    let branched = collect_base_scopes(
        store,
        snapshot,
        &root,
        &mut scopes,
        &mut HashSet::new(),
        // `super` addresses the direct bases only; a self receiver walks the
        // whole chain a fixed number of hops out.
        if superclass { 1 } else { 8 },
    )?;
    Ok(bind_hierarchy_member(
        snapshot,
        stored,
        hits,
        (!superclass).then_some(&root),
        &scopes,
        branched,
    ))
}

/// Rust impl blocks in different files may share a type through an import,
/// but matching the spelling alone would mix unrelated same-named structs.
fn resolve_rust_self(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    enclosing: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let identity = rust_types::identity(store, snapshot, stored.file_id, enclosing, caches)?;
    let mut matched = None;
    for hit in hits {
        let Some((scope, _)) = hit.qualified_name.rsplit_once("::") else {
            continue;
        };
        if rust_types::identity(store, snapshot, hit.file_id, scope, caches)? == identity {
            if matched.is_some() {
                return Ok(None);
            }
            matched = Some(hit);
        }
    }
    Ok(matched.map(|hit| member_resolution(stored, hit)))
}

/// Binds a receiver call through the owner's declared bindings: a local
/// whose creation or parameter line pins exactly one indexed type answers
/// the member calls that follow it. Bindings at or after the call line say
/// nothing about it — rebinding a name orders the evidence, so the latest
/// earlier binding wins. A type name resolving to zero or several entities
/// keeps the call for the remaining rules.
///
/// # Errors
/// Propagates graph storage failures.
fn resolve_receiver_typed(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    if matches!(
        stored.language.as_str(),
        "typescript" | "javascript" | "go" | "java"
    ) {
        return bindings::resolve(store, snapshot, stored, hits, caches);
    }
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    // The self family has its own hierarchy rule; typed bindings only
    // address named locals, parameters and members.
    if receiver == "super" || is_self_receiver(&stored.language, receiver) {
        return Ok(None);
    }
    let Some(bound_type) = receiver_type_of(store, snapshot, stored, receiver, caches)? else {
        return Ok(None);
    };
    // The binding names the class whose members answer the call, once the
    // spelling is read: a holder answers as what it holds, an iterator member
    // as the element it walks, and a member type a class states by name as what
    // that name denotes.
    let root = type_root_scope(store, snapshot, stored, &bound_type, caches)?;
    let Some(root) = root else {
        // A type the workspace never declares still names a member when the
        // workspace declares exactly one by that name.
        return Ok(resolve_undeclared_type_member(
            snapshot,
            stored,
            &bound_type,
            hits,
        ));
    };
    let mut scopes = vec![root.clone()];
    let branched =
        collect_base_scopes(store, snapshot, &root, &mut scopes, &mut HashSet::new(), 8)?;
    Ok(bind_hierarchy_member(
        snapshot,
        stored,
        hits,
        Some(&root),
        &scopes,
        branched,
    ))
}

/// A bare call a using-declaration of the owner's class brings into scope.
///
/// `using First<T>::Run;` states that a bare `Run()` written in the class's
/// bodies reaches `First::Run`, even when another base declares that member
/// too. The declaration belongs to the class, so it answers before the
/// namespaces around the class are searched — a namespace-scope function of
/// the same name is a different symbol.
///
/// # Errors
/// Propagates graph storage failures of the binding reads.
fn resolve_using_member(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls
        || reference.receiver_name.is_some()
        || !matches!(stored.language.as_str(), "c" | "cpp")
    {
        return Ok(None);
    }
    let Some(candidates) = snapshot.lookup_candidates(&stored.name_tail) else {
        return Ok(None);
    };
    for scope in owner_class_scopes(store, snapshot, stored)? {
        let Some(entity) = scope.entity.as_deref() else {
            continue;
        };
        let bindings = match caches.declared_bindings.entry(entity.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(store.declared_bindings(entity)?),
        };
        // A using-declaration binds the imported name to the spelling it
        // imports, which ends with that name; a member declaration binds it to
        // its type instead, which says nothing about where the name lives.
        let mut found: Vec<&NameIndexEntry> = Vec::new();
        for binding in bindings.iter().filter(|binding| {
            binding.name == stored.name_tail
                && binding.bound_type.rsplit("::").next() == Some(binding.name.as_str())
        }) {
            let target = type_head(&binding.bound_type);
            for hit in candidates {
                let spells_target = hit.qualified_name == target
                    || hit.qualified_name.ends_with(&format!("::{target}"));
                if spells_target
                    && is_callable_symbol(hit)
                    && !found.iter().any(|known| known.entity_id == hit.entity_id)
                {
                    found.push(hit);
                }
            }
        }
        match found.as_slice() {
            [] => {}
            [hit] => return Ok(Some(member_resolution(stored, hit))),
            // Several spellings of one brought-in name leave the call to the
            // rules below rather than binding one of them.
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// A member call through a type the project never declares.
///
/// A generated class (`unittest::TestAllExtensions` from a `.proto`) has no
/// declaration in the workspace, so no hierarchy answers the member. The one
/// member the workspace declares by that name still answers it: the generated
/// class carries the runtime base's members, and the runtime is what the
/// workspace declares. Several same-named members elsewhere leave the call
/// pending, since nothing here says which of them the type has.
fn resolve_undeclared_type_member(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    bound_type: &str,
    hits: &[&NameIndexEntry],
) -> Option<Resolution> {
    if !matches!(stored.language.as_str(), "c" | "cpp") {
        return None;
    }
    let head = type_head(&pointee_type(bound_type));
    let tail = head.rsplit("::").next()?;
    // The index answers `Some([])` for a name nothing declares; `None` is a
    // snapshot that still hides some of it, which is not evidence of absence.
    if tail.is_empty()
        || snapshot
            .lookup_candidates(tail)
            .is_some_and(|known| !known.is_empty())
    {
        return None;
    }
    match hits {
        [hit] => Some(member_resolution(stored, hit)),
        _ => None,
    }
}

/// The declaring scope of the type a binding names.
///
/// The index holds a class under its own qualified name (`leveldb::Slice`) and,
/// when the class has a constructor, once more under the class's name inside it
/// (`leveldb::Slice::Slice`). Both spellings share one declaring scope, which
/// is what the member rule searches; a name that repeats the class's own
/// segment is the nested spelling, anything else is the class itself.
fn type_declaring_scope(candidate: &NameIndexEntry, bound_type: &str) -> String {
    let type_name = bound_type.rsplit("::").next().unwrap_or(bound_type);
    let qualified = &candidate.qualified_name;
    let Some((scope, tail)) = qualified.rsplit_once("::") else {
        return qualified.clone();
    };
    if tail != type_name {
        return qualified.clone();
    }
    match scope.rsplit_once("::") {
        Some((_, parent)) if parent == type_name => scope.to_owned(),
        None if scope == type_name => scope.to_owned(),
        _ => qualified.clone(),
    }
}

/// The receiver type the call site's name holds.
///
/// The calling body answers first: a local or parameter declares the name and
/// its line orders the evidence, so the latest declaration at or before the call
/// wins, and a loop's own variable stops applying at the end of the loop. A name
/// the body never declares is a member the enclosing class states in its own
/// body — a header, another file, a base class — and member declarations carry no
/// line order the call site could be compared against, so the member answer is
/// not filtered by line.
///
/// A receiver is not always one name. `statuses[0]`, `kv.second` and
/// `(*it).second` read through what the step before them holds, so the receiver
/// is evaluated one step at a time and the type of the last step answers.
///
/// # Errors
/// Propagates graph storage failures.
fn receiver_type_of(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<String>> {
    receiver_path_type(store, snapshot, stored, receiver, caches, 0)
}

/// The type one receiver path denotes, a step at a time.
///
/// The extractor records two steps that are not a single name: `element:(path)`
/// for a subscript or a loop's element, and `<base>.<member>` for a member read.
/// Everything else is the name itself, which the owner's body or the enclosing
/// class states.
///
/// # Errors
/// Propagates graph storage failures.
fn receiver_path_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
    depth: usize,
) -> crate::EngineResult<Option<String>> {
    if depth > MAX_TYPE_STEPS {
        return Ok(None);
    }
    if let Some((marker, inner)) = step_marker(receiver) {
        let Some(inner_type) =
            receiver_path_type(store, snapshot, stored, inner, caches, depth + 1)?
        else {
            return Ok(None);
        };
        return Ok(step_type(marker, &inner_type));
    }
    if let Some(bound) = declared_receiver_type(store, snapshot, stored, receiver, caches)? {
        // A loop or a placeholder declaration binds what the range or path it
        // was taken from holds, which is only known once that is read.
        if let Some((marker, inner)) = step_marker(&bound) {
            let Some(inner_type) =
                receiver_path_type(store, snapshot, stored, inner, caches, depth + 1)?
            else {
                return Ok(None);
            };
            return Ok(step_type(marker, &inner_type));
        }
        // The spelling keeps the declarator's decorations: a pointer answers a
        // subscript with what it points at, and the step that reads the value
        // decides what the name itself denotes.
        return Ok(Some(bound));
    }
    let Some((base, member)) = receiver.rsplit_once('.') else {
        return Ok(None);
    };
    let Some(base_type) = receiver_path_type(store, snapshot, stored, base, caches, depth + 1)?
    else {
        return Ok(None);
    };
    member_of_type(store, snapshot, stored, &base_type, member, caches, depth)
}

/// The type the call site's own body or enclosing class states for one name.
///
/// # Errors
/// Propagates graph storage failures.
fn declared_receiver_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<String>> {
    let reference = &stored.reference;
    let local = {
        let bindings = match caches.declared_bindings.entry(reference.owner_id.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(store.declared_bindings(&reference.owner_id)?),
        };
        bindings
            .iter()
            .filter(|binding| binding.name == receiver && binding_holds_at(binding, reference.line))
            .map(|binding| binding.bound_type.clone())
            .next_back() // bindings are line-ordered; the latest earlier one shadows
    };
    if local.is_some() {
        return Ok(local);
    }
    if let Some(binding) = translation_unit_binding(store, snapshot, stored, receiver, caches)? {
        return Ok(Some(binding));
    }
    member_type_of(store, snapshot, stored, receiver, caches)
}

/// The type the reference's translation unit states for one name outside every
/// function (`extern MemTableRep::Iterator* iter;`).
///
/// Such a declaration belongs to the unit rather than to one body, so every
/// body that sees it reads the same type. Textual inclusion is what makes a
/// unit: the header states the declaration, the `.cc` that includes it writes
/// the calls. Two files stating one name differently leave the name undecided.
///
/// # Errors
/// Propagates graph storage failures of the binding and include-chain reads.
fn translation_unit_binding(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<String>> {
    let mut types = translation_unit_bindings(store, snapshot, stored, receiver, caches)?;
    match types.len() {
        1 => Ok(types.pop()),
        _ => Ok(None),
    }
}

/// Every type the reference's translation unit states for one name outside
/// every function, in file order and without repeats.
///
/// # Errors
/// Propagates graph storage failures of the binding and include-chain reads.
fn translation_unit_bindings(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Vec<String>> {
    let mut files = vec![stored.file_id];
    if matches!(stored.language.as_str(), "c" | "cpp") {
        if let Entry::Vacant(entry) = caches.include_closures.entry(stored.file_id) {
            entry.insert(include_closure(
                store,
                snapshot,
                &stored.language,
                stored.file_id,
            )?);
        }
        if let Some(closure) = caches.include_closures.get(&stored.file_id) {
            let mut included: Vec<u32> = closure.iter().copied().collect();
            // The walk answers a set; the readings stay in file order so the
            // same workspace always resolves a name the same way.
            included.sort_unstable();
            files.extend(included);
        }
    }
    let mut found: Vec<String> = Vec::new();
    for file in files {
        let owner = format!("f{file}");
        let bindings = match caches.declared_bindings.entry(owner) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(store.declared_bindings(&format!("f{file}"))?),
        };
        // Lines order the declarations of one file; a header the unit includes
        // states its names for the whole unit, whatever line it sits on.
        let same_file = file == stored.file_id;
        for binding in bindings.iter().filter(|binding| {
            binding.name == receiver
                && (!same_file || binding_holds_at(binding, stored.reference.line))
        }) {
            if !found.contains(&binding.bound_type) {
                found.push(binding.bound_type.clone());
            }
        }
    }
    Ok(found)
}

/// The candidates a bare type spelling names once the unit's own scope is read.
///
/// `using google::protobuf::Descriptor;` states what `Descriptor` means where
/// the call is written, so a namesake another file declares is a different
/// class. Only a spelling that names another scope says that: a declaration
/// (`Widget w;`) states a value, not a meaning for the type's name.
///
/// # Errors
/// Propagates graph storage failures.
fn unit_spelled_hits<'a>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    head: &str,
    hits: Vec<&'a NameIndexEntry>,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Vec<&'a NameIndexEntry>> {
    if hits.len() < 2 {
        return Ok(hits);
    }
    // `using namespace rocksdb;` puts that namespace's own declarations into
    // the unit's lookup, so `Iterator` there is the namespace's class rather
    // than a class nested inside another one of its types.
    let mut visible = used_namespace_hits(store, snapshot, stored, head, &hits, caches)?;
    if !visible.is_empty() {
        return Ok(visible);
    }
    let Some(aliased) = translation_unit_binding(store, snapshot, stored, head, caches)? else {
        return Ok(hits);
    };
    if !aliased.contains("::") || aliased.rsplit("::").next() != Some(head) {
        return Ok(hits);
    }
    visible = spelled_under(&hits, &aliased);
    Ok(if visible.is_empty() { hits } else { visible })
}

/// The binding name extraction records for a `using namespace` directive.
const USING_NAMESPACE_BINDING: &str = "[[cpp_using_namespace]]";

/// The candidates a namespace the unit opens with `using namespace` declares
/// by that name.
///
/// # Errors
/// Propagates graph storage failures of the binding and include-chain reads.
fn used_namespace_hits<'a>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    head: &str,
    hits: &[&'a NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Vec<&'a NameIndexEntry>> {
    let spaces =
        translation_unit_bindings(store, snapshot, stored, USING_NAMESPACE_BINDING, caches)?;
    Ok(hits
        .iter()
        .filter(|hit| {
            spaces
                .iter()
                .any(|space| hit.qualified_name == format!("{space}::{head}"))
        })
        .copied()
        .collect())
}

/// Whether a declaration is still in effect at the call line.
///
/// The declaration has to come earlier (`line <= call`) and, when a loop scopes
/// it, the call has to be inside that loop: `for (Engine* car : engines)` states
/// `car` for its body alone, so a call after the loop reads the name's other
/// declaration instead.
fn binding_holds_at(binding: &DeclaredBinding, line: usize) -> bool {
    let Ok(declared) = usize::try_from(binding.line) else {
        return false;
    };
    if declared > line {
        return false;
    }
    match binding.until {
        Some(until) => usize::try_from(until).is_ok_and(|last| line <= last),
        None => true,
    }
}

/// The class scopes a body reads unqualified names from, in the order the
/// language searches them.
///
/// The owner's qualified name names its scopes innermost first
/// (`ns::Cls::method` → `Cls`, then `ns`), and every class is followed by its
/// bases, nearest first: a member or a nested type a base declares is reached
/// through the class's own inheritance edges.
///
/// # Errors
/// Propagates graph storage failures.
fn owner_class_scopes(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
) -> crate::EngineResult<Vec<ClassScope>> {
    let mut scopes = Vec::new();
    let Some(owner) = snapshot.entity_record(&stored.reference.owner_id) else {
        return Ok(scopes);
    };
    for enclosing in enclosing_scopes(&owner.qualified_name) {
        let Some(entity) = class_entity_in(snapshot, enclosing, Some(stored.file_id)) else {
            continue;
        };
        let root = ClassScope {
            declared_in: class_declaration_file(snapshot, enclosing, Some(stored.file_id)),
            entity: Some(entity),
            name: enclosing.to_owned(),
        };
        scopes.push(root.clone());
        collect_base_scopes(store, snapshot, &root, &mut scopes, &mut HashSet::new(), 8)?;
    }
    Ok(scopes)
}

/// The type the enclosing class (or one of its bases) gives a member name.
///
/// The first scope that declares the member the method calls through answers
/// it; a member a base declares is reached through the class's inheritance
/// edges, nearest base first.
///
/// # Errors
/// Propagates graph storage failures.
fn member_type_of(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    receiver: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<String>> {
    for scope in owner_class_scopes(store, snapshot, stored)? {
        let Some(entity) = scope.entity.as_deref() else {
            continue;
        };
        let bindings = match caches.declared_bindings.entry(entity.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(store.declared_bindings(entity)?),
        };
        // Several declarations of one member name inside one class are a
        // rebinding (`int x; double x;` is not C++, but macros can produce
        // it); the last declaration is the one in effect.
        if let Some(bound) = bindings
            .iter()
            .filter(|binding| binding.name == receiver)
            .map(|binding| binding.bound_type.clone())
            .next_back()
        {
            return Ok(Some(bound));
        }
    }
    Ok(None)
}

/// The candidates a nested type of the enclosing class (or of one of its
/// bases) spells.
///
/// A C++ body reads an unqualified name from its own class, then its bases,
/// and only then from the enclosing namespaces: inside a class deriving from
/// `MemTableRep`, a member spelled `Iterator` is `MemTableRep::Iterator` rather
/// than a namespace-scope namesake.
fn class_scope_hits<'a>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    head: &str,
    hits: &[&'a NameIndexEntry],
) -> crate::EngineResult<Option<Vec<&'a NameIndexEntry>>> {
    if !matches!(stored.language.as_str(), "c" | "cpp") {
        return Ok(None);
    }
    for scope in owner_class_scopes(store, snapshot, stored)? {
        let nested = format!("{}::{head}", scope.name);
        let found: Vec<&NameIndexEntry> = hits
            .iter()
            .filter(|hit| hit.qualified_name == nested)
            .copied()
            .collect();
        match found.len() {
            0 => {}
            1 => return Ok(Some(found)),
            // Two classes spelled alike in two files leave the name undecided.
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// The candidates whose qualified name ends with the binding's own spelling:
/// `MemTableRep::Iterator` selects `rocksdb::MemTableRep::Iterator` and leaves
/// the global `rocksdb::Iterator` out.
fn spelled_under<'a>(hits: &[&'a NameIndexEntry], bound_type: &str) -> Vec<&'a NameIndexEntry> {
    let suffix = format!("::{bound_type}");
    hits.iter()
        .filter(|hit| hit.qualified_name == bound_type || hit.qualified_name.ends_with(&suffix))
        .copied()
        .collect()
}

/// The candidates a spelling qualified from the global namespace names:
/// `::upb::Arena` selects `upb::Arena` alone, while `hpb::upb::Arena` is a
/// class of another scope that merely repeats the tail.
fn global_spelled_under<'a>(
    hits: &[&'a NameIndexEntry],
    bound_type: &str,
) -> Vec<&'a NameIndexEntry> {
    hits.iter()
        .filter(|hit| hit.qualified_name == bound_type)
        .copied()
        .collect()
}

/// How many steps one receiver path or type spelling may take before the
/// evidence stops being a spelling and becomes a guess (`A<B>::C::D` chains and
/// member types that name each other).
const MAX_TYPE_STEPS: usize = 8;

/// Words that qualify a spelled type without naming another type.
const TYPE_QUALIFIER_WORDS: &[&str] = &["const", "volatile", "typename", "struct", "class", "enum"];

/// Holding templates whose argument is the type the calls written through them
/// reach: `std::unique_ptr<Widget>` answers `w->Draw()` with `Widget`'s members.
///
/// A container is not a holder. `std::vector<Widget>` answers calls with its own
/// members, so treating its element as the receiver would bind `ws.size()` to
/// `Widget::size`.
const HOLDER_TEMPLATES: &[&str] = &[
    "unique_ptr",
    "shared_ptr",
    "weak_ptr",
    "optional",
    "StatusOr",
    "NoDestructor",
];

/// The names a container states for the position it walks. They are lowercase
/// because a nested class named `Iterator` (`SkipList::Iterator`) is a class in
/// its own right, not a position in another type.
const ELEMENT_MEMBER_NAMES: &[&str] = &[
    "iterator",
    "const_iterator",
    "reverse_iterator",
    "const_reverse_iterator",
    "value_type",
    "reference",
    "const_reference",
];

/// One step the extractor records instead of a spelling: the name a binding
/// declares is not a type, but what a container, a subscript, a position or a
/// path yields.
#[derive(Clone, Copy)]
enum StepMarker {
    /// The element a loop walks (`for (auto& kv : buffers)`).
    Element,
    /// The value a subscript reads (`buffers[k]`).
    Index,
    /// The element an iterator taken from a range walks (`auto it = v.begin()`).
    Position,
    /// Whatever the path on the right of a placeholder declaration holds
    /// (`auto& v = it->second`).
    TypeOf,
}

/// The step a binding spells, with the path it steps from.
fn step_marker(text: &str) -> Option<(StepMarker, &str)> {
    let markers = [
        ("element:(", StepMarker::Element),
        ("index:(", StepMarker::Index),
        ("position:(", StepMarker::Position),
        ("type_of:(", StepMarker::TypeOf),
    ];
    markers.into_iter().find_map(|(prefix, marker)| {
        text.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(')'))
            .map(|inner| (marker, inner))
    })
}

/// The type a step answers for the type it steps from.
fn step_type(marker: StepMarker, inner_type: &str) -> Option<String> {
    match marker {
        StepMarker::Element => element_type(inner_type),
        StepMarker::Index => index_type(inner_type),
        StepMarker::Position => position_element_type(inner_type),
        StepMarker::TypeOf => Some(unwrap_binding_type(inner_type)),
    }
}

/// The class a spelled type names, as the member rule searches it.
///
/// The spelling states a type the way the source wrote it, so it is read before
/// it is looked up: a holder answers as the type it holds, a pointer or an array
/// as what it addresses, an iterator member as the element it walks, and a
/// member type a class states by name (`BaseHyperClockCache<Table>::Shard`) as
/// what that name denotes in the class.
///
/// # Errors
/// Propagates graph storage failures.
fn type_root_scope(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    type_text: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<ClassScope>> {
    type_root_scope_in(store, snapshot, stored, type_text, caches, 0)
}

/// The class a spelled type names, after `depth` readings of the spelling.
///
/// # Errors
/// Propagates graph storage failures.
fn type_root_scope_in(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    type_text: &str,
    caches: &mut RuleCaches,
    depth: usize,
) -> crate::EngineResult<Option<ClassScope>> {
    if depth > MAX_TYPE_STEPS {
        return Ok(None);
    }
    let spelled = pointee_type(type_text);
    let head = type_head(&spelled);
    // A spelling that starts at the global namespace (`::upb::Arena`) names one
    // class wherever the call stands: `hpb::upb::Arena` repeats the tail but is
    // a different class, so only the exact name answers it.
    let global = spelled.trim_start().starts_with("::");
    let Some(tail) = head.rsplit("::").next() else {
        return Ok(None);
    };
    if tail.is_empty() {
        return Ok(None);
    }
    // The name index keys entities by their own name, so a qualified binding
    // (`MemTableRep::Iterator`) is looked up by its tail and then narrowed by
    // the enclosing class it spells. A binding the declaration spelled with a
    // scope (`absl::Status`) names one class, and only that class answers: a
    // project type whose tail happens to match (`upb::Status`) is a different
    // class, so an unmatched spelling leaves the call pending rather than
    // falling back to a namesake. A bare binding instead means what that name
    // means where the call is written: inside `namespace ui`, `Widget` is
    // `ui::Widget` even when another file's namespace declares a namesake.
    let candidates = match snapshot
        .lookup_candidates(tail)
        .map(|hits| hits.iter().collect::<Vec<&NameIndexEntry>>())
    {
        Some(hits) if global => Some(global_spelled_under(&hits, &head)),
        Some(hits) if head.contains("::") => Some(spelled_under(&hits, &head)),
        Some(hits) => {
            // A class the call is written in answers the name before the
            // namespaces around it do; the enclosing-scope reading applies
            // only when neither the class nor its bases declares it.
            let hits = match class_scope_hits(store, snapshot, stored, &head, &hits)? {
                Some(nested) => nested,
                None => enclosing_scope_hits(snapshot, stored, hits),
            };
            Some(unit_spelled_hits(
                store, snapshot, stored, &head, hits, caches,
            )?)
        }
        None => None,
    };
    if let Some(candidates) = candidates
        && !candidates.is_empty()
    {
        // An alias or typedef states the type it names (`using Line = Row`),
        // and the declaration's own binding carries that spelling: a call
        // written through the alias reaches the named type's members, which the
        // alias itself declares none of.
        if let [candidate] = candidates.as_slice()
            && candidate.symbol_type == Some(SymbolType::Alias)
            && !head.contains("::")
            && let Some(aliased) = self_named_binding(store, caches, candidate)?
            && aliased != type_text
        {
            return type_root_scope_in(store, snapshot, stored, &aliased, caches, depth + 1);
        }
        // Constructors (`X::X`) and impl blocks index under the type's name
        // inside the type's declaring scope, so the binding identifies the type
        // by the one scope its candidates share; candidates spelling several
        // distinct scopes keep the type ambiguous.
        let mut shared_scopes = candidates
            .iter()
            .map(|candidate| type_declaring_scope(candidate, &head));
        let Some(type_scope) = shared_scopes.next() else {
            return Ok(None);
        };
        if shared_scopes.any(|scope| scope != type_scope) {
            return Ok(None);
        }
        // The member's declaring scope is the bound type's own, then its
        // ancestors nearest first, mirroring the self-receiver hierarchy.
        // A bound type declared in one file is placed there; candidates spanning
        // several files leave the declaring file to the index.
        let single_file = {
            let mut files = candidates.iter().map(|candidate| candidate.file_id);
            files
                .next()
                .filter(|first| files.all(|file| file == *first))
        };
        return Ok(Some(ClassScope::at(snapshot, type_scope, single_file)));
    }
    // `A<T>::B` where `B` is a type `A` states by name: the alias answers as
    // what it names, so the spelling is read one level in.
    let Some((scope, _)) = head.rsplit_once("::") else {
        return Ok(None);
    };
    let Some(member_type) =
        member_of_type(store, snapshot, stored, scope, tail, caches, depth + 1)?
    else {
        return Ok(None);
    };
    type_root_scope_in(store, snapshot, stored, &member_type, caches, depth + 1)
}

/// The type a spelling's own declaration binds it to, when that declaration is
/// an alias rather than a class (`using Line = Row`, `typedef Row Line`).
///
/// # Errors
/// Propagates graph storage failures.
fn self_named_binding(
    store: &IndexStore,
    caches: &mut RuleCaches,
    candidate: &NameIndexEntry,
) -> crate::EngineResult<Option<String>> {
    let bindings = match caches.declared_bindings.entry(candidate.entity_id.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(store.declared_bindings(&candidate.entity_id)?),
    };
    Ok(bindings
        .iter()
        .filter(|binding| candidate.qualified_name.ends_with(&binding.name))
        .map(|binding| binding.bound_type.clone())
        .next_back())
}

/// The type whose members answer a call written through a value of this type.
///
/// A holder answers as what it holds, a pointer or an array as what it
/// addresses, and an iterator member as the element it walks:
/// `std::deque<Slice>::const_iterator` answers `it->data()` with `Slice`.
fn pointee_type(text: &str) -> String {
    let mut text = unwrap_binding_type(text);
    if let Some(base) = text.strip_suffix("[]") {
        text = unwrap_binding_type(base);
    }
    if let Some(element) = iterator_element(&text) {
        text = unwrap_binding_type(&element);
    }
    text
}

/// The type a spelled value holds, with the spelling's decoration removed.
///
/// Qualifiers (`const`, `typename`), pointer and reference markers, and holders
/// all describe how a value is stored rather than what its members are, so each
/// is read through: `const std::unique_ptr<Widget>&` is a `Widget`.
fn unwrap_binding_type(text: &str) -> String {
    let mut current = text.trim().to_owned();
    for _ in 0..MAX_TYPE_STEPS {
        let mut trimmed = current.as_str();
        loop {
            let trimmed_now = trimmed.trim();
            if let Some((word, rest)) = trimmed_now.split_once(char::is_whitespace)
                && TYPE_QUALIFIER_WORDS.contains(&word)
            {
                trimmed = rest;
                continue;
            }
            if let Some((rest, word)) = trimmed_now.rsplit_once(char::is_whitespace)
                && TYPE_QUALIFIER_WORDS.contains(&word)
            {
                trimmed = rest;
                continue;
            }
            trimmed = trimmed_now;
            break;
        }
        // A pointer or reference marker sits on either end: `Node*` in a
        // template argument, `*Widget` in a declarator's abstract spelling.
        let trimmed = trimmed.trim_matches(['*', '&']).trim();
        let next = holder_argument(trimmed).unwrap_or_else(|| trimmed.to_owned());
        if next == current {
            return next;
        }
        current = next;
    }
    current
}

/// The type a holding template argument names: `std::unique_ptr<Widget>` holds a
/// `Widget`.
fn holder_argument(text: &str) -> Option<String> {
    let segments = type_segments(text);
    let (head, args) = segments.last()?;
    let name = head.rsplit("::").next()?;
    if !HOLDER_TEMPLATES.contains(&name) || !text.trim_end().ends_with('>') {
        return None;
    }
    args.first().cloned()
}

/// The type one element of a container, array or iterator names.
fn element_type(text: &str) -> Option<String> {
    element_type_spelling(text).map(|element| unwrap_binding_type(&element))
}

/// The element as the container spells it, before decoration is read through:
/// a container of owners (`std::vector<std::shared_ptr<T>>`) states an element
/// that is the holder itself.
fn element_type_spelling(text: &str) -> Option<String> {
    let text = unwrap_binding_type(text);
    // `Status[2]` states its element count in the declarator; the binding
    // records the levels as `[]`, one per subscript the value answers.
    if let Some(base) = text.strip_suffix("[]") {
        return Some(base.to_owned());
    }
    if let Some(element) = iterator_element(&text) {
        return Some(element);
    }
    let segments = type_segments(&text);
    let (head, args) = segments.last()?;
    if args.is_empty() {
        return None;
    }
    if is_map_name(head) && args.len() >= 2 {
        return Some(format!("pair<{}, {}>", args[0], args[1]));
    }
    Some(args[0].clone())
}

/// The value a subscript reads.
///
/// A map's subscript answers the value it pairs with the key (`buffers[k]` is a
/// `Buffer`, not the pair a loop over the same map walks), a container or an
/// array its element, and a pointer what its declarator points at. A class that
/// states its own `operator[]` answers a type only its return type could name,
/// so a spelling with none of those shapes stays unanswered.
fn index_type(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(base) = text.strip_suffix("[]") {
        return Some(base.to_owned());
    }
    let bare = unwrap_binding_type(text);
    if let Some(element) = iterator_element(&bare) {
        return Some(element);
    }
    let segments = type_segments(&bare);
    let (head, args) = segments.last()?;
    if is_map_name(head) && args.len() >= 2 {
        return Some(args[1].clone());
    }
    if args.is_empty() {
        return text.strip_suffix('*').map(unwrap_binding_type);
    }
    Some(args[0].clone())
}

/// The element a position walks.
///
/// `it->member()` reaches the element's own members, so the element answers as
/// a value. When that value is itself a holder (`std::vector<std::shared_ptr<T>>`
/// walks a `std::shared_ptr<T>`), the call reaches the holder's members — but a
/// call written as `(*it)->member()` reaches the held type's instead, and the
/// extractor records only the receiver's name, not which of the two spellings
/// the source wrote. A position over owners therefore stays unanswered rather
/// than binding the held type's namesakes.
fn position_element_type(text: &str) -> Option<String> {
    let element = element_type_spelling(text)?;
    if holder_argument(&element).is_some() {
        return None;
    }
    Some(unwrap_binding_type(&element))
}

/// The element an iterator, `value_type` or `reference` member walks.
///
/// `std::deque<Slice>::const_iterator` walks `Slice`, and
/// `RepeatedPtrField<Location>::iterator` walks the `Location` the container
/// states, whatever the iterator class is called.
fn iterator_element(text: &str) -> Option<String> {
    let segments = type_segments(text);
    let (tail, tail_args) = segments.last()?;
    if !tail_args.is_empty() || !ELEMENT_MEMBER_NAMES.contains(&tail.as_str()) {
        return None;
    }
    let (container, args) = segments[..segments.len().checked_sub(1)?]
        .iter()
        .rev()
        .find(|(_, args)| !args.is_empty())?;
    if is_map_name(container) && args.len() >= 2 {
        return Some(format!("pair<{}, {}>", args[0], args[1]));
    }
    args.first().cloned()
}

/// The two types a pair-shaped value holds: a `pair<A, B>` states them, a map
/// pairs each key with its value, and an iterator over either names the pair its
/// element is.
fn pair_elements(text: &str) -> Option<(String, String)> {
    let text = unwrap_binding_type(text);
    if iterator_element(&text).is_some() {
        return pair_elements(&iterator_element(&text)?);
    }
    let segments = type_segments(&text);
    let (head, args) = segments.last()?;
    if args.len() < 2 || !(head == "pair" || is_map_name(head)) {
        return None;
    }
    Some((args[0].clone(), args[1].clone()))
}

/// Whether a template name is a map, whose element pairs a key with a value.
fn is_map_name(name: &str) -> bool {
    matches!(
        name,
        "map" | "multimap" | "unordered_map" | "unordered_multimap"
    )
}

/// The type a member of the named type holds.
///
/// A pair-shaped type answers `first` and `second` from what it pairs. Anything
/// else is a class the spelling names, and the member is what its own body (or a
/// base's) states for that name.
///
/// # Errors
/// Propagates graph storage failures.
fn member_of_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    type_text: &str,
    member: &str,
    caches: &mut RuleCaches,
    depth: usize,
) -> crate::EngineResult<Option<String>> {
    if depth > MAX_TYPE_STEPS {
        return Ok(None);
    }
    if let Some((first, second)) = pair_elements(type_text) {
        match member {
            "first" => return Ok(Some(unwrap_binding_type(&first))),
            "second" => return Ok(Some(unwrap_binding_type(&second))),
            _ => {}
        }
    }
    let Some(root) = type_root_scope_in(store, snapshot, stored, type_text, caches, depth + 1)?
    else {
        return Ok(None);
    };
    let mut scopes = vec![root.clone()];
    collect_base_scopes(store, snapshot, &root, &mut scopes, &mut HashSet::new(), 8)?;
    for scope in &scopes {
        let Some(entity) = scope.entity.as_deref() else {
            continue;
        };
        let bindings = match caches.declared_bindings.entry(entity.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(store.declared_bindings(entity)?),
        };
        if let Some(bound) = bindings
            .iter()
            .filter(|binding| binding.name == member)
            .map(|binding| binding.bound_type.clone())
            .next_back()
        {
            return Ok(Some(unwrap_binding_type(&bound)));
        }
    }
    Ok(None)
}

/// The qualified head a spelled type names, with the template arguments
/// dropped: `std::deque<Slice>::const_iterator` heads `std::deque::const_iterator`.
fn type_head(text: &str) -> String {
    type_segments(text)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

/// Splits a spelled type into its `::` segments, each with the template
/// arguments it applies: `std::deque<Slice>::const_iterator` yields
/// `[("std", []), ("deque", ["Slice"]), ("const_iterator", [])]`.
///
/// Angle brackets nest (`std::map<int, std::pair<A, B>>`), and a comma separates
/// arguments only at the outermost level of one application.
fn type_segments(text: &str) -> Vec<(String, Vec<String>)> {
    let mut segments: Vec<(String, Vec<String>)> = Vec::new();
    let mut name = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut argument = String::new();
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                depth += 1;
                if depth > 1 {
                    argument.push(ch);
                }
            }
            '>' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    push_argument(&mut args, &mut argument);
                } else {
                    argument.push(ch);
                }
            }
            ',' if depth == 1 => push_argument(&mut args, &mut argument),
            ':' if depth == 0 && chars.peek() == Some(&':') => {
                chars.next();
                segments.push((name.trim().to_owned(), std::mem::take(&mut args)));
                name.clear();
            }
            _ => {
                if depth == 0 {
                    name.push(ch);
                } else {
                    argument.push(ch);
                }
            }
        }
    }
    push_argument(&mut args, &mut argument);
    segments.push((name.trim().to_owned(), args));
    segments
}

/// Records one template argument when the text between separators names one.
fn push_argument(args: &mut Vec<String>, argument: &mut String) {
    let trimmed = argument.trim();
    if !trimmed.is_empty() {
        args.push(trimmed.to_owned());
    }
    argument.clear();
}

/// Binds a member call whose receiver names an object literal: the literal's
/// own property table states which value each key holds, so `api.run()` reaches
/// the function `run` was last bound to rather than the method of the same
/// name an earlier property described (`{ run() {}, run: right }`).
///
/// The receiver must name exactly one declaration in the workspace, and that
/// declaration must state the key. A receiver the workspace declares more than
/// once keeps the general rules: two unrelated `api` objects state different
/// property tables, and picking either would be a guess.
fn resolve_object_member(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    if receiver == "super" || is_self_receiver(&stored.language, receiver) {
        return Ok(None);
    }
    let holder = match unique_receiver(snapshot, receiver) {
        Some(holder) => holder,
        // The receiver spells an `as` import: the object whose table this
        // call reads is the one that module exports, not a same-named
        // declaration in this file.
        None => match imported_binding_target(
            store,
            snapshot,
            stored,
            stored.file_id,
            receiver,
            caches,
        )? {
            Some(holder) => holder,
            None => return Ok(None),
        },
    };
    // The last property of a key wins, mirroring the language's own object
    // literal semantics.
    let Some(member) = holder
        .object_members
        .iter()
        .rev()
        .find(|member| member.name == stored.name_tail)
    else {
        return Ok(None);
    };
    // A property value that is itself an `as` import names the exporting
    // module's declaration: `{ run: renamed }` holds `renamed`'s module
    // binding, not a nested declaration that happens to spell the name. The
    // value is spelled in the module that declares the literal.
    if let Some(target) = imported_binding_target(
        store,
        snapshot,
        stored,
        holder.file_id,
        &member.target,
        caches,
    )? {
        return Ok(Some(Resolution {
            ref_id: stored.id,
            target_id: target.entity_id.clone(),
            provenance: EdgeProvenance::ImportBound,
        }));
    }
    let Some(targets) = snapshot.lookup_candidates(&member.target) else {
        return Ok(None);
    };
    let scoped: Vec<&NameIndexEntry> = targets
        .iter()
        .filter(|candidate| candidate.file_id == holder.file_id)
        .collect();
    Ok(match prefer_definitions(scoped).as_slice() {
        [target] => Some(Resolution {
            ref_id: stored.id,
            target_id: target.entity_id.clone(),
            provenance: EdgeProvenance::FileScoped,
        }),
        _ => None,
    })
}

/// The object member rule's outcome, or `Deferred` when the object literal's
/// property table says nothing about the called key.
///
/// # Errors
/// Propagates graph storage failures.
fn object_member_outcome(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Outcome> {
    Ok(
        match resolve_object_member(store, snapshot, stored, caches)? {
            Some(resolution) => Outcome::Resolved {
                resolution,
                rule: Rule::ObjectMember,
            },
            None => Outcome::Deferred,
        },
    )
}

/// The one entity a receiver name places across the workspace: a same-name
/// declaration in another file is a different object, so an ambiguous or
/// missing set leaves the caller no table to read.
fn unique_receiver<'a>(
    snapshot: &'a ResolutionSnapshot,
    receiver: &str,
) -> Option<&'a NameIndexEntry> {
    let receivers = snapshot.lookup_candidates(receiver)?;
    let preferred = prefer_definitions(receivers.iter().collect());
    match preferred.as_slice() {
        [holder] => Some(*holder),
        _ => None,
    }
}

/// The declaration an `as` import binds one local name to inside its module.
///
/// `import { actual as renamed } from './target'` makes `renamed` this file's
/// spelling of `target.ts`'s `actual`; every other spelling of that name is a
/// different declaration, however near it is written. Only a unique same-module
/// declaration is followed, and a name no `as` clause binds is `None`.
///
/// # Errors
/// Propagates graph storage failures.
fn imported_binding_target<'a>(
    store: &IndexStore,
    snapshot: &'a ResolutionSnapshot,
    stored: &StoredPendingRef,
    file_id: u32,
    local: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<&'a NameIndexEntry>> {
    let file_bindings = load_bindings_in(
        store,
        stored.language.as_str(),
        file_id,
        &mut caches.alias_bindings,
    )?;
    let matched: Vec<&AliasBinding> = file_bindings
        .iter()
        .filter(|binding| binding.imported.is_some() && binding.local == local)
        .collect();
    let [binding] = matched.as_slice() else {
        return Ok(None);
    };
    let Some(target_file) = alias_target_file_in(
        snapshot,
        stored.language.as_str(),
        file_id,
        binding,
        &mut caches.importing_paths,
        &mut caches.alias_targets,
    ) else {
        return Ok(None);
    };
    let Some(imported) = binding.imported.as_deref() else {
        return Ok(None);
    };
    let Some(raw_hits) = snapshot.lookup_candidates(imported) else {
        return Ok(None);
    };
    let scoped: Vec<&NameIndexEntry> = raw_hits
        .iter()
        .filter(|hit| hit.file_id == target_file)
        .collect();
    Ok(match prefer_definitions(scoped).as_slice() {
        [hit] => Some(*hit),
        _ => None,
    })
}

/// Appends every base class reachable from `root` through extends/implements
/// edges, nearest first: the direct bases before their own bases, so reading
/// `scopes` in order walks the hierarchy by distance from `root` — the order
/// member lookup needs to let an override shadow the declarations of the bases
/// it replaces. `remaining` bounds the chain depth (one hop for `super`
/// receivers, a fixed limit for the transitive `self` chain) and `visited`
/// guards against inheritance cycles; a root whose class the index cannot
/// place ends the walk for that branch conservatively.
///
/// Returns whether one class on the way declares several bases, the condition
/// under which distance is no longer the language's own lookup order and the
/// caller must not read `scopes` as a precedence order.
///
/// # Errors
/// Propagates graph storage failures.
fn collect_base_scopes(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    root: &ClassScope,
    scopes: &mut Vec<ClassScope>,
    visited: &mut HashSet<String>,
    remaining: usize,
) -> crate::EngineResult<bool> {
    // A base is reached through its own entity, so every scope this walk adds
    // starts from that exact entity; only the root may fall back to the name.
    let mut frontier = vec![root.clone()];
    visited.insert(root.entity.clone().unwrap_or_else(|| root.name.clone()));
    let mut branched = false;
    for _ in 0..remaining {
        let mut bases = Vec::new();
        for scope in &frontier {
            let class_id = scope
                .entity
                .clone()
                .or_else(|| class_entity_id(snapshot, &scope.name));
            let Some(class_id) = class_id else {
                continue;
            };
            let (resolved, pending) = store.inheritance_bases(&class_id)?;
            // Clauses, not resolved bases: a base the index cannot place still
            // occupies a position in the order the language searches.
            branched |= resolved.len() + pending.len() > 1;
            let mut declared = resolved;
            for name in pending {
                // A base spelled as an application (`Impl<T...>`) is looked up
                // by the class name: the arguments say which specialization,
                // not which class.
                let lookup = type_head(&name);
                // Hidden candidates must not turn an ambiguous base name into
                // a seemingly unique ancestor.
                let candidates = prefer_definitions(
                    snapshot
                        .lookup_candidates(&lookup)
                        .unwrap_or_default()
                        .iter()
                        .collect::<Vec<_>>(),
                );
                if let [hit] = candidates.as_slice() {
                    declared.push(hit.entity_id.clone());
                }
            }
            bases.extend(declared);
        }
        let mut next = Vec::new();
        for base_id in bases {
            let Some(base) = snapshot.entity_record(&base_id) else {
                continue;
            };
            let scope = ClassScope {
                entity: Some(base_id),
                ..ClassScope::at(snapshot, base.qualified_name.clone(), None)
            };
            let key = scope.entity.clone().unwrap_or_else(|| scope.name.clone());
            if visited.insert(key) {
                scopes.push(scope.clone());
                next.push(scope);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(branched)
}

/// The entity whose qualified name spells `class_scope`, addressed through
/// its last `::` segment in the name index.
///
/// A type declaration outranks a scope that carries its name over (`impl
/// Target` in Rust, a C++ constructor `X::X`), and a definition outranks a
/// prototype: both name one class, so the hierarchy resolves through either.
/// A scope no entity spells exactly, or several that do, keeps the hierarchy
/// unknown.
///
/// `file_hint` places a class two modules declare under one name: the class a
/// method belongs to is declared beside it, which is the evidence the name
/// alone cannot carry. Without a hint the name has to be workspace-unique.
fn class_entity_in(
    snapshot: &ResolutionSnapshot,
    class_scope: &str,
    file_hint: Option<u32>,
) -> Option<String> {
    let tail = class_scope.rsplit("::").next()?;
    let hits = snapshot.lookup_candidates(tail)?;
    let spelled: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == class_scope)
        .collect();
    let declared: Vec<&NameIndexEntry> = spelled
        .iter()
        .copied()
        .filter(|hit| hit.declares_type)
        .collect();
    let matched = if declared.is_empty() {
        spelled
    } else {
        declared
    };
    let in_file: Vec<&NameIndexEntry> = matched
        .iter()
        .copied()
        .filter(|hit| file_hint == Some(hit.file_id))
        .collect();
    let matched = if in_file.is_empty() { matched } else { in_file };
    let matched = prefer_definitions(matched);
    if let [hit] = matched.as_slice() {
        return Some(hit.entity_id.clone());
    }
    None
}

/// `class_entity_in` for a scope no single file hints at.
fn class_entity_id(snapshot: &ResolutionSnapshot, class_scope: &str) -> Option<String> {
    class_entity_in(snapshot, class_scope, None)
}

/// The file that declares the class `class_scope` for a reference written in
/// `file_hint`.
///
/// A file is the identity a class name lacks: every module that declares a
/// `Target` spells it `Target`, and an `impl` block adds a second entity of
/// that name inside the same file, so neither the name nor a unique entity
/// separates two modules. The hint decides whenever that file declares the
/// name at all — a member and the class it belongs to are declared together,
/// with out-of-line definitions being the exception the fallback covers. With
/// no declaration at the hint, a single declaring file workspace-wide still
/// answers; several leave the class unplaced, and the caller compares names.
fn class_declaration_file(
    snapshot: &ResolutionSnapshot,
    class_scope: &str,
    file_hint: Option<u32>,
) -> Option<u32> {
    let tail = class_scope.rsplit("::").next()?;
    let hits = snapshot.lookup_candidates(tail)?;
    let spelled: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == class_scope)
        .collect();
    // Only the declarations that introduce the name locate the class. A scope
    // that carries the name over from the type it belongs to (Rust's `impl
    // Target`) sits wherever its implementation is written, which says nothing
    // about where the class is declared; a name no entity declares as a type
    // falls back to every entity spelling it.
    let declaring: HashSet<u32> = {
        let types: Vec<u32> = spelled
            .iter()
            .filter(|hit| hit.declares_type)
            .map(|hit| hit.file_id)
            .collect();
        if types.is_empty() {
            spelled.iter().map(|hit| hit.file_id).collect()
        } else {
            types.into_iter().collect()
        }
    };
    if let Some(hint) = file_hint.filter(|hint| declaring.contains(hint)) {
        return Some(hint);
    }
    if declaring.len() == 1 {
        return declaring.into_iter().next();
    }
    None
}

/// Binds a qualified call (`Class::method()`) to the entity whose
/// `qualified_name` spells the member under the receiver's scope. Extraction
/// builds that name from the declaring breadcrumb joined with `::`, which
/// fixes what a receiver can spell: either the whole receiver (`ns::Class`
/// indexes a type nested in `ns`) or only its last `::` segment (`Class`
/// indexes a top-level type). The first spelling keeps `crate::ns::Class`
/// verbatim and so matches nothing, while the second drops `crate` and every
/// enclosing module — `crate::Class::method()` therefore binds a top-level
/// type, and `crate::ns::Class::method()` binds neither the nested nor the
/// top-level one and stays pending. Because the qualified name carries the
/// declaring scope, a unique match resolves even when the short member name
/// is ambiguous workspace-wide. Duplicate `qualified_name` declarations and
/// instance-style dotted receivers, which never spell a `::`-scoped path,
/// stay pending.
fn resolve_qualified_name(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
) -> Option<Resolution> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return None;
    }
    let receiver = reference.receiver_name.as_deref()?;
    // The receiver may spell the full declaring path (`ns::Waldo::find()`) or
    // only its innermost segment (`Waldo::find()`); accept either spelling,
    // and bind only when exactly one entity answers.
    let scope = receiver.rsplit("::").next().unwrap_or(receiver);
    let expected_full = format!("{receiver}::{}", stored.name_tail);
    let expected_tail = format!("{scope}::{}", stored.name_tail);
    let matches: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == expected_full || hit.qualified_name == expected_tail)
        .copied()
        .collect();
    if let [hit] = matches.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: called_target_id(snapshot, stored, hit),
            provenance: EdgeProvenance::QualifiedNameMatch,
        });
    }
    None
}

/// Binds a Java import specifier to the one candidate it names.
///
/// Java imports address entities, not modules: `import p.Widget;` names the
/// type `Widget`, while `import static p.Util.work;` names the member `work`
/// of class `Util`. A class's indexed `qualified_name` starts at the class
/// itself, so the name part of the specifier matches as a dotted suffix on a
/// segment boundary: `p.Util.work` names `Util::work` and `p.Outer.Inner`
/// names `Outer::Inner`, never `Other::work`. On-demand forms (`p.*`,
/// `p.Util.*`) name no single target and stay pending.
///
/// The package the specifier states must also agree with the package the
/// declaring file records: a specifier for an external package would otherwise
/// accept the workspace's only class of that name, since a Java name carries
/// no package. Without that agreement — including when the declaring file's
/// package is unknown — the reference stays pending instead of binding an
/// unrelated file into this file's import scope.
fn resolve_java_import(stored: &StoredPendingRef, hits: &[&NameIndexEntry]) -> Option<Resolution> {
    let specifier = stored.reference.ref_name.as_str();
    let mut matched = hits
        .iter()
        .filter(|hit| java_import_match(hit.package.as_deref(), specifier, &hit.qualified_name));
    let hit = matched.next()?;
    if matched.next().is_some() {
        return None;
    }
    Some(Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: EdgeProvenance::ImportBound,
    })
}

/// Whether a Java import specifier names `qualified_name`, declared in
/// `package`: the name as a dotted suffix past a package segment, and the
/// package the specifier states equal to the one the file declares. A Java
/// name carries no package, so the declared package is the only evidence that
/// `java.util.List` is not the workspace's `my.List`; uniqueness inside the
/// workspace cannot stand in for it, and a file whose package is unknown keeps
/// the import pending.
fn java_import_match(package: Option<&str>, specifier: &str, qualified_name: &str) -> bool {
    let Some(stated) = specifier_package(specifier, qualified_name) else {
        return false;
    };
    package == Some(stated)
}

/// The package a specifier states before the name it spells: `p.Util.work`
/// states `p` for `Util::work`, and a specifier spelling only the name states
/// the default package. `None` when the specifier does not end with the dotted
/// name on a segment boundary.
fn specifier_package<'a>(specifier: &'a str, qualified_name: &str) -> Option<&'a str> {
    let name = qualified_name.replace("::", ".");
    let prefix = specifier.strip_suffix(&name)?;
    if prefix.is_empty() {
        return Some("");
    }
    prefix.strip_suffix('.')
}

/// Reports import references whose specifier can never address a workspace
/// module: TS/JS bare package specifiers (anything not starting with `./`
/// or `../`, reduced to its first path segment) and Python's bare top-level
/// module imports (`import re`). A specifier whose leading segment matches
/// a workspace file or directory — a monorepo package, a top-level module —
/// stays in the queue for path-based rules instead.
///
/// Scoped specifiers (`@scope/pkg`) and path aliases (`@/x`) always stay
/// queued: a workspace's own package name or `paths` mapping can route them
/// inside the workspace while no file or directory spells the scope, so
/// existence cannot be proven either way. Holding them is safe because a
/// module specifier never reaches the name index: only the path rules can
/// place it, and `classify` defers it when they cannot. Languages whose
/// imports name an entity instead — Rust's `use`, Java's `import` — are
/// answered from the name index, so they must never be failed here.
fn is_unresolvable_import(
    snapshot: &ResolutionSnapshot,
    reference: &StoredPendingRef,
    module_paths: &mut HashMap<(String, String), bool>,
) -> bool {
    if reference.reference.ref_kind != GraphEdgeKind::Imports {
        return false;
    }
    let specifier = reference.reference.ref_name.as_str();
    let root = match reference.language.as_str() {
        // Angle-bracket includes address the standard library and `-I`
        // include paths, which a workspace snapshot can never spell — the
        // extraction layer only emits this form for `#include <x>`. Quoted
        // forms arrive normalized to `./x` and flow through the relative
        // path below.
        "c" | "cpp" if specifier.starts_with('<') => return true,
        "typescript" | "javascript"
            if !specifier.starts_with("./") && !specifier.starts_with("../") =>
        {
            if specifier.starts_with('@') {
                return false;
            }
            specifier
                .split('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or(specifier)
        }
        "python" if !specifier.starts_with('.') && !specifier.contains('.') => specifier,
        _ => return false,
    };
    let key = (reference.language.clone(), root.to_owned());
    let exists = match module_paths.entry(key) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => {
            let exists = snapshot.path_has_module(root, extensions_for(&reference.language));
            entry.insert(exists);
            exists
        }
    };
    !exists
}

/// Narrows an ambiguous TS/JS reference to the one workspace-wide exported
/// declaration: cross-file lookups only reach exported symbols, so when
/// every candidate is declared in another file and exactly one is exported,
/// the rest are unreachable shadows. A same-file candidate (free local
/// access) or several exported declarations keeps the ambiguity path.
fn resolve_export_bound(stored: &StoredPendingRef, hits: &[&NameIndexEntry]) -> Option<Resolution> {
    if !matches!(stored.language.as_str(), "typescript" | "javascript")
        || !matches!(
            stored.reference.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Extends | GraphEdgeKind::Implements
        )
    {
        return None;
    }
    let exported: Vec<&NameIndexEntry> =
        hits.iter().filter(|hit| hit.is_exported).copied().collect();
    if exported.len() == 1 && hits.iter().all(|hit| hit.file_id != stored.file_id) {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: exported[0].entity_id.clone(),
            provenance: EdgeProvenance::ExportBound,
        });
    }
    None
}

/// Resolves a receiver call through the declaring scope its receiver spells
/// together with the call's argument count: `A::run(1)` names the scope, so
/// that scope's own overloads are the only candidates and their counts
/// separate them.
///
/// The receiver narrows candidates by its whole spelling: `ns::Waldo` and a
/// top-level `Waldo` may both declare `find`, and keeping only the last
/// segment would hand the call to the namespace's type' namesake. An argument
/// count says nothing across scopes — two classes may declare a `match` taking
/// one argument — so a receiver that reproduces no candidate's scope narrows
/// nothing and leaves the reference to the caller's guard.
fn resolve_receiver_arity(
    stored: &StoredPendingRef,
    hits: &[&NameIndexEntry],
) -> Option<Resolution> {
    let receiver = stored.reference.receiver_name.as_deref()?;
    let expected = stored.reference.arity?;
    let hit = unique_arity_match(
        hits.iter()
            .filter(|hit| {
                hit.qualified_name
                    .rsplit_once("::")
                    .is_some_and(|(owner, _)| owner == receiver)
            })
            .copied(),
        expected,
    )?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: EdgeProvenance::ArityBound,
    })
}

/// Narrows an ambiguous bare name to the one candidate whose declared
/// parameter count equals the call's argument count: among candidates that
/// all declare their arity, a single match is the invoked overload.
/// Candidates with unknown arity cannot rule themselves out, so their
/// presence keeps the reference pending.
fn resolve_arity_bound(stored: &StoredPendingRef, hits: &[&NameIndexEntry]) -> Option<Resolution> {
    let expected = stored.reference.arity?;
    let hit = unique_arity_match(hits.iter().copied(), expected)?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: EdgeProvenance::ArityBound,
    })
}

/// The one candidate declaring exactly `expected` parameters, or `None` when
/// several counts agree or any candidate's count is unknown.
fn unique_arity_match<'entry>(
    candidates: impl Iterator<Item = &'entry NameIndexEntry>,
    expected: usize,
) -> Option<&'entry NameIndexEntry> {
    let mut matched = None;
    for hit in candidates {
        match hit.arity {
            Some(arity) if arity as usize == expected => {
                if matched.is_some() {
                    return None;
                }
                matched = Some(hit);
            }
            Some(_) => {}
            None => return None,
        }
    }
    matched
}

/// Directory prefix of a forward-slash workspace path; the workspace root
/// itself is the empty string.
fn importing_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// Joins a relative specifier onto the importing directory, folding `.` and
/// `..` segments. Returns `None` when the specifier names no file (`./`) or
/// the joined path escapes the root.
fn join_specifier(dir: &str, specifier: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in dir.split('/').filter(|segment| !segment.is_empty()) {
        segments.push(segment);
    }
    let mut named = false;
    for segment in specifier.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                segments.pop()?;
            }
            _ => {
                segments.push(segment);
                named = true;
            }
        }
    }
    if !named {
        return None;
    }
    Some(segments.join("/")).filter(|joined| !joined.is_empty())
}

/// Resolves a Python relative specifier against the importing module's
/// package: one leading dot is that package itself, and every further dot
/// climbs one parent directory. The dotted tail then joins the climbed
/// directory like an absolute module path, so `..json` imported from
/// `pkg/json/tag.py` addresses `pkg/json` and lands on its `__init__.py`
/// variant, while `...util` from `pkg/a/b.py` addresses the workspace-root
/// `util` module.
fn python_relative_base(dir: &str, specifier: &str) -> Option<String> {
    let dots = specifier
        .chars()
        .take_while(|character| *character == '.')
        .count();
    let mut segments: Vec<&str> = dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    // The first dot names the importing package; each extra dot ascends one.
    for _ in 1..dots {
        segments.pop()?;
    }
    let tail = specifier[dots..].replace('.', "/");
    let tail = tail.trim_matches('/');
    let base = match (segments.is_empty(), tail.is_empty()) {
        (true, true) => return None,
        (true, false) => tail.to_owned(),
        (false, true) => segments.join("/"),
        (false, false) => format!("{}/{}", segments.join("/"), tail),
    };
    Some(base)
}

/// Path candidates for one specifier: the literal base first (explicit
/// extensions win), then the module file per-language extensions, then the
/// directory entry the language resolves a directory to — the TypeScript
/// family's `index.<ext>`, a Python package's `__init__.py`.
///
/// Python orders the last two the other way round: its importer checks a
/// directory package's `__init__` before a same-named module file, so
/// `pkg/__init__.py` has to precede `pkg.py` for `from pkg import work` to
/// reach the package both files spell.
fn path_variants(base: &str, language: &str) -> Vec<String> {
    let extensions = extensions_for(language);
    let module_files = || {
        extensions
            .iter()
            .map(move |extension| format!("{base}{extension}"))
    };
    let mut variants = vec![base.to_owned()];
    if let Some(entry) = index_entry_for(language) {
        variants.push(format!("{base}/{entry}"));
        variants.extend(module_files());
    } else {
        variants.extend(module_files());
        variants.extend(
            extensions
                .iter()
                .map(|extension| format!("{base}/index{extension}")),
        );
    }
    variants
}

/// The file a directory stands for, for languages whose package entry is not
/// spelled `index.<ext>`.
fn index_entry_for(language: &str) -> Option<&'static str> {
    match language {
        "python" => Some("__init__.py"),
        _ => None,
    }
}

/// Extension preference per reference language, mirroring the TypeScript
/// extension table: source order over compiled siblings.
fn extensions_for(language: &str) -> &'static [&'static str] {
    match language {
        "typescript" => &[".ts", ".tsx", ".js", ".jsx"],
        "javascript" => &[".js", ".jsx"],
        "python" => &[".py"],
        "rust" => &[".rs"],
        "go" => &[".go"],
        "java" => &[".java"],
        "c" => &[".c", ".h"],
        "cpp" => &[".cpp", ".cc", ".cxx", ".hpp", ".h"],
        _ => &[],
    }
}

#[cfg(test)]
mod alias_pair_tests {
    use super::{python_alias_pairs, rust_alias_pairs, ts_alias_pairs};

    fn pairs(items: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        items
            .iter()
            .map(|(local, imported)| ((*local).to_owned(), imported.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn ts_reads_multiline_named_type_and_namespace_imports() {
        assert_eq!(
            ts_alias_pairs("import {\n  work as leftWork,\n  other as second,\n} from './left'"),
            pairs(&[("leftWork", Some("work")), ("second", Some("other"))])
        );
        assert_eq!(
            ts_alias_pairs("import type { Work as W } from './types'"),
            pairs(&[("W", Some("Work"))])
        );
        assert_eq!(
            ts_alias_pairs("import * as leftApi from './left'"),
            pairs(&[("leftApi", None)])
        );
        // A default import spells no named binding, and neither does an
        // aliased default export, which no named lookup can reach.
        assert!(ts_alias_pairs("import work from './left'").is_empty());
        assert!(ts_alias_pairs("import { default as thing } from './m'").is_empty());
    }

    #[test]
    fn python_reads_parenthesized_comma_and_plain_imports() {
        assert_eq!(
            python_alias_pairs(
                "from pkg import (\n    work as leftWork,\n    other,\n    second as s,\n)"
            ),
            pairs(&[("leftWork", Some("work")), ("s", Some("second"))])
        );
        assert_eq!(
            python_alias_pairs("from pkg.sub import a, b as c"),
            pairs(&[("c", Some("b"))])
        );
        // A leading `import` binds modules, so every alias names a namespace.
        assert_eq!(
            python_alias_pairs("import left.module as lm, other.mod as om"),
            pairs(&[("lm", None), ("om", None)])
        );
    }

    #[test]
    fn rust_reads_path_aliases_and_skips_brace_groups() {
        assert_eq!(
            rust_alias_pairs("use crate::util::work as left_work;"),
            pairs(&[("left_work", Some("work"))])
        );
        // A brace group needs per-item module paths the text alone cannot pin.
        assert!(rust_alias_pairs("use crate::util::{work as left_work};").is_empty());
        assert!(rust_alias_pairs("use crate::util::work;").is_empty());
    }

    #[test]
    fn trailing_comments_never_spell_a_binding() {
        // The comment names ` from ` and ` as `; neither may cut the clause or
        // extend a local name.
        assert_eq!(
            ts_alias_pairs("import { work as w } from './left' // moved from './right'"),
            pairs(&[("w", Some("work"))])
        );
        assert_eq!(
            python_alias_pairs(
                "from pkg import (\n    work as w,  # re-export\n    other as o,\n)"
            ),
            pairs(&[("w", Some("work")), ("o", Some("other"))])
        );
        assert_eq!(
            python_alias_pairs("from pkg import work as w  # re-export"),
            pairs(&[("w", Some("work"))])
        );
        assert_eq!(
            rust_alias_pairs("use crate::util::work as w; // alias"),
            pairs(&[("w", Some("work"))])
        );
    }
}

#[cfg(test)]
mod java_import_tests {
    use super::java_import_match;

    #[test]
    fn a_specifier_states_the_package_before_the_name() {
        assert!(java_import_match(
            Some("com.acme"),
            "com.acme.Util.work",
            "Util::work"
        ));
        assert!(java_import_match(
            Some("com.acme"),
            "com.acme.Outer.Inner",
            "Outer::Inner"
        ));
        assert!(java_import_match(Some(""), "Util", "Util"));
        // A name that does not start on a segment boundary spells another name.
        assert!(!java_import_match(
            Some("com.acme"),
            "com.acme.MyUtil",
            "Util"
        ));
    }

    #[test]
    fn only_the_declared_package_confirms_the_specifier() {
        // The workspace's only `List` belongs to `my`, not to `java.util`.
        assert!(!java_import_match(Some("my"), "java.util.List", "List"));
        // A directory that happens to end in the package name is no evidence:
        // a file under `.../example/java/util` may declare `example.java.util`.
        assert!(!java_import_match(
            Some("example.java.util"),
            "java.util.List",
            "List"
        ));
        // A class in a named package is not in the default package.
        assert!(!java_import_match(Some("com.acme"), "Util", "Util"));
        // A file whose package is unknown carries no evidence.
        assert!(!java_import_match(None, "com.acme.Util", "Util"));
    }
}

#[cfg(test)]
mod overload_evidence_tests {
    use super::same_parameters;

    #[test]
    fn normalized_parameter_types_pair() {
        // The stored lists carry types only, so the name a parameter spells
        // never separates one overload.
        assert!(same_parameters("int", "int"));
        assert!(same_parameters("const char*", "const char*"));
        assert!(same_parameters(
            "const std::string&, int",
            "const std::string&, int"
        ));
    }

    #[test]
    fn different_types_and_counts_never_pair() {
        assert!(!same_parameters("unsigned", "unsigned long"));
        assert!(!same_parameters("unsigned long", "unsigned"));
        assert!(!same_parameters("const char*", "int"));
        assert!(!same_parameters("int, int", "int"));
    }
}

#[cfg(test)]
mod language_gate_tests {
    use super::{formats_for_path, language_compatible_formats};
    use crate::domain::FileFormat;
    use std::path::Path;

    #[test]
    fn unknown_reference_language_keeps_every_candidate() {
        // `unknown` marks a reference the walk could not tag with a language,
        // not a language whose candidates must spell `unknown` too.
        assert!(language_compatible_formats(
            Some(FileFormat::Unknown),
            Some(&formats_for_path(Path::new("src/lib.rs")))
        ));
        assert!(language_compatible_formats(Some(FileFormat::Unknown), None));
    }

    #[test]
    fn missing_evidence_on_either_side_keeps_the_candidate() {
        assert!(language_compatible_formats(
            None,
            Some(&formats_for_path(Path::new("src/lib.py")))
        ));
        assert!(language_compatible_formats(Some(FileFormat::Rust), None));
        // An unregistered extension is unknown evidence, not a mismatch.
        assert!(language_compatible_formats(
            Some(FileFormat::Rust),
            Some(&formats_for_path(Path::new("src/data.zzz")))
        ));
    }

    #[test]
    fn known_languages_gate_on_the_shared_namespace() {
        assert!(!language_compatible_formats(
            Some(FileFormat::Rust),
            Some(&formats_for_path(Path::new("src/lib.py")))
        ));
        assert!(language_compatible_formats(
            Some(FileFormat::C),
            Some(&formats_for_path(Path::new("src/api.cpp")))
        ));
        assert!(language_compatible_formats(
            Some(FileFormat::TypeScript),
            Some(&formats_for_path(Path::new("src/main.js")))
        ));
    }
}
