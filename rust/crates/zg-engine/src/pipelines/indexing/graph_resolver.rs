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
//! static member their specifier spells, export/arity evidence narrows the
//! remaining ambiguities, external import specifiers terminate as failed,
//! and [`IndexStore::apply_resolutions`] writes each batch atomically.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use super::pipeline::throw_if_cancelled;
use crate::domain::{
    EdgeProvenance, FileFormat, GraphEdgeKind, SymbolType, formats_for_path,
    reference_compatible_with,
};
use crate::storage::graph::{NameIndexEntry, Resolution, StoredPendingRef};
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
    let mut caches = RuleCaches::default();
    let mut cursor = 0;
    loop {
        let page = store.list_pending_refs(PENDING_PAGE, cursor)?;
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
                Outcome::Ambiguous(candidates) => batch.candidates.push((stored.id, candidates)),
            }
        }
        // Write back at the page boundary: everything decided so far is
        // durable, so a cancelled or failing later page leaves its references
        // pending without discarding the work already done.
        batch.write_back(store, &mut sweep)?;
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
        sweep: &mut ResolutionSweep,
    ) -> crate::EngineResult<()> {
        if !self.resolutions.is_empty() {
            let stats = store.apply_resolutions(&self.resolutions)?;
            sweep.resolved += stats.resolved;
            sweep.stale += stats.stale;
        }
        if !self.failures.is_empty() {
            sweep.failed += store.apply_failures(&self.failures)?;
        }
        if !self.candidates.is_empty() {
            sweep.candidates_recorded += store.record_candidates(&self.candidates)?;
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
    importing_paths: HashMap<u32, Option<String>>,
    candidate_paths: HashMap<u32, Option<String>>,
    importing_files: HashMap<u32, Vec<u32>>,
    module_paths: HashMap<(String, String), bool>,
    alias_bindings: HashMap<u32, Vec<AliasBinding>>,
    alias_targets: HashMap<(String, String), Option<u32>>,
}

impl ResolutionSweep {
    fn record(&mut self, rule: Rule) {
        match rule {
            Rule::Import | Rule::Unique => {}
            Rule::ImportBound => self.import_bound += 1,
            Rule::Receiver => self.file_scoped += 1,
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
    if let Some(resolution) = resolve_alias_bound(
        store,
        snapshot,
        stored,
        &mut caches.alias_bindings,
        &mut caches.importing_paths,
        &mut caches.alias_targets,
    )? {
        // An `as` alias never names an indexed symbol itself, so the bare-name
        // lookup below would only park the call; the alias binding pins its
        // imported name's module directly.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::ImportBound,
        });
    }
    if let Some(resolution) = resolve_namespace_bound(
        store,
        snapshot,
        stored,
        &mut caches.alias_bindings,
        &mut caches.importing_paths,
        &mut caches.alias_targets,
    )? {
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

/// Runs the name-index rules: receivers, qualified names, uniqueness, and the
/// import/export/arity bounds that narrow an ambiguous set.
fn classify_names(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Outcome> {
    let hits = {
        let Some(raw) = snapshot.lookup_candidates(&stored.name_tail) else {
            // Hidden candidates must not turn an ambiguous name into
            // a seemingly unique match against an unfinished snapshot.
            return Ok(Outcome::Deferred);
        };
        // The language gate runs first: preference for definitions is only
        // meaningful among candidates that can host this reference.
        let compatible = filter_compatible_hits(snapshot, stored, raw, &mut caches.candidate_paths);
        prefer_definitions(compatible)
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
    if let Some(resolution) = resolve_receiver_class(store, snapshot, stored, &hits)? {
        // The receiver names the enclosing instance or superclass, so the
        // member's declaring scope is pinned to the owner's class hierarchy
        // before workspace-wide ambiguity is considered.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Receiver,
        });
    }
    if let Some(resolution) = resolve_receiver_typed(store, snapshot, stored, &hits)? {
        // The receiver names a local whose declared type pins the member's
        // declaring scope before workspace-wide ambiguity is considered.
        return Ok(Outcome::Resolved {
            resolution,
            rule: Rule::Receiver,
        });
    }
    if let Some(resolution) = resolve_qualified_name(stored, &hits) {
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
    if let [hit] = hits.as_slice() {
        if hit.file_id != stored.file_id {
            return Ok(Outcome::Resolved {
                resolution: Resolution {
                    ref_id: stored.id,
                    target_id: hit.entity_id.clone(),
                    provenance: EdgeProvenance::WorkspaceUnique,
                },
                rule: Rule::Unique,
            });
        }
        // Bare name, same file, single hit: the in-file symbol table skips
        // declaration entities, so a pending bare call can only mean this
        // one indexed location.
        return Ok(Outcome::Resolved {
            resolution: Resolution {
                ref_id: stored.id,
                target_id: hit.entity_id.clone(),
                provenance: EdgeProvenance::FileScoped,
            },
            rule: Rule::Unique,
        });
    }
    if hits.len() > 1 {
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
            hits.into_iter().map(|hit| hit.entity_id).collect(),
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
fn filter_compatible_hits(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    candidate_paths: &mut HashMap<u32, Option<String>>,
) -> Vec<NameIndexEntry> {
    let mut filtered = Vec::with_capacity(hits.len());
    for hit in hits {
        let path = match candidate_paths.entry(hit.file_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry
                .insert(snapshot.file_path(hit.file_id).map(str::to_owned))
                .clone(),
        };
        if language_compatible(FileFormat::parse(&stored.language), path.as_deref()) {
            filtered.push(hit.clone());
        }
    }
    filtered
}

/// Whether a reference whose language is `reference` can address a candidate
/// file at `path`.
///
/// Missing evidence keeps the candidate: the gate never discards on
/// information it does not have. An unregistered extension is unknown
/// evidence just like a reference the walk could not tag with a language —
/// `unknown` is not a language of its own, so requiring an `unknown` candidate
/// to match it would drop every candidate spelling a real one.
fn language_compatible(reference: Option<FileFormat>, path: Option<&str>) -> bool {
    let (Some(reference), Some(path)) = (reference, path) else {
        return true;
    };
    if reference == FileFormat::Unknown {
        return true;
    }
    let formats = formats_for_path(Path::new(path));
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

/// Whether two parameter lists describe one overload. A definition names its
/// parameters while a declaration often does not, so a parameter matches when
/// its tokens are equal or one side carries a single extra trailing identifier
/// (`int` against `int value`, `const char*` against `const char* path`).
/// Everything else stays unproven: `unsigned long` against `unsigned` and
/// `const char*` against `int` are different parameters, and a comparison that
/// cannot tell them apart would merge unrelated overloads.
fn same_parameters(declared: &str, defined: &str) -> bool {
    let declared = split_parameters(declared);
    let defined = split_parameters(defined);
    declared.len() == defined.len()
        && declared
            .iter()
            .zip(defined.iter())
            .all(|(left, right)| same_parameter(left, right))
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

fn same_parameter(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left: Vec<&str> = left.split_whitespace().collect();
    let right: Vec<&str> = right.split_whitespace().collect();
    let (longer, shorter) = if left.len() > right.len() {
        (left, right)
    } else {
        (right, left)
    };
    longer.len() == shorter.len() + 1
        && longer[..shorter.len()] == shorter[..]
        && is_parameter_name(longer[shorter.len()])
}

/// Whether the token a parameter adds to its type can be that parameter's name.
/// `int` against `int value` and `const char*` against `const char* path` differ
/// by one, but `unsigned` against `unsigned long` differ by one too, and reading
/// `long` as a name would merge two different parameters.
fn is_parameter_name(token: &str) -> bool {
    let mut characters = token.chars();
    characters
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_')
        && !matches!(
            token,
            "auto"
                | "bool"
                | "char"
                | "char8_t"
                | "char16_t"
                | "char32_t"
                | "class"
                | "const"
                | "double"
                | "enum"
                | "float"
                | "int"
                | "long"
                | "register"
                | "restrict"
                | "short"
                | "signed"
                | "size_t"
                | "struct"
                | "typename"
                | "union"
                | "unsigned"
                | "void"
                | "volatile"
                | "wchar_t"
        )
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
    let base = match (stored.language.as_str(), reference.ref_name.as_str()) {
        // A dotted Python specifier addresses the workspace root
        // (`from declare import x` reaches `declare.py`), except for the
        // leading-dot form, which climbs from the importing module's package.
        ("python", specifier) if specifier.starts_with('.') => {
            python_relative_base(importing_dir(&path), specifier)?
        }
        ("python", specifier) => join_specifier("", &specifier.replace('.', "/"))?,
        (_, specifier) if specifier.starts_with("./") || specifier.starts_with("../") => {
            join_specifier(importing_dir(&path), specifier)?
        }
        _ => return None,
    };
    let file_id = path_variants(&base, &stored.language)
        .into_iter()
        .find_map(|candidate| snapshot.file_id_by_path(&candidate))?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: format!("f{file_id}"),
        provenance: EdgeProvenance::ImportScoped,
    })
}

/// Resolves an ambiguous bare-name reference through the file's imports:
/// when the imports pin the name to exactly one module and that module
/// declares it exactly once, the reference targets that definition.
/// Receiver-typed calls wait for receiver inference, and a same-file lexical
/// binding of the bare name shadows any import binding, so both stay with
/// the general ambiguity path.
///
/// A lexical binding is a top-level definition or one declared inside a
/// function or module scope; a class member is not, because a bare call never
/// spells the receiver that reaches it.
fn resolve_import_bound(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    importing_files: &mut HashMap<u32, Vec<u32>>,
) -> Option<Resolution> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some()
        || !matches!(
            reference.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Extends | GraphEdgeKind::Implements
        )
        || hits
            .iter()
            .any(|hit| hit.file_id == stored.file_id && shadows_import(snapshot, hit))
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
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some() || reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(store, stored, bindings)?;
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
        importing_paths,
        targets,
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
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(store, stored, bindings)?;
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
        importing_paths,
        targets,
    ))
}

/// Cached alias bindings of the reference's own file.
///
/// # Errors
/// Propagates graph storage failures.
fn load_bindings(
    store: &IndexStore,
    stored: &StoredPendingRef,
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
) -> crate::EngineResult<Vec<AliasBinding>> {
    Ok(match bindings.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry.insert(load_alias_bindings(store, stored)?).clone(),
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
            target_id: hit.entity_id.clone(),
            provenance: EdgeProvenance::ImportBound,
        });
    }
    None
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

/// Loads the file's import edges and parses their statement text into `as`
/// alias bindings. Only statements spelling an explicit `as` produce
/// bindings: unaliased imports keep flowing through the bare-name and
/// import-bound rules, which already resolve them.
///
/// # Errors
/// Propagates graph storage failures.
fn load_alias_bindings(
    store: &IndexStore,
    stored: &StoredPendingRef,
) -> crate::EngineResult<Vec<AliasBinding>> {
    let pairs: AliasPairs = match stored.language.as_str() {
        "typescript" | "javascript" => ts_alias_pairs,
        "python" => python_alias_pairs,
        "rust" => rust_alias_pairs,
        _ => return Ok(Vec::new()),
    };
    let mut bindings = Vec::new();
    for reference in store.import_references(stored.file_id)? {
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
    let language = stored.language.as_str();
    let base = alias_target_base(snapshot, stored, binding, importing_paths)?;
    match targets.entry((language.to_owned(), base.clone())) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => *entry.insert(alias_target_at(snapshot, language, &base)),
    }
}

/// Workspace-relative module path a binding addresses, before extension
/// probing: TS/JS specifiers join the importing file's directory, while
/// Python's dotted paths and Rust's leading module segment address the
/// workspace root. `None` for specifiers no rule can place.
fn alias_target_base(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> Option<String> {
    let language = stored.language.as_str();
    let module: String = match language {
        "typescript" | "javascript" => {
            if !binding.specifier.starts_with("./") && !binding.specifier.starts_with("../") {
                return None;
            }
            binding.specifier.clone()
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
            let path = match importing_paths.entry(stored.file_id) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => entry
                    .insert(snapshot.file_path(stored.file_id).map(str::to_owned))
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

/// The candidates declaring a member inside `scope`, recognized by the
/// qualified name extraction builds from the declaring breadcrumb.
fn declarations_in<'a>(hits: &'a [NameIndexEntry], scope: &str) -> Vec<&'a NameIndexEntry> {
    hits.iter()
        .filter(|hit| {
            hit.qualified_name
                .rsplit_once("::")
                .is_some_and(|(candidate, _)| candidate == scope)
        })
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
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    scopes: &[String],
) -> Option<Resolution> {
    for scope in scopes {
        match declarations_in(hits, scope).as_slice() {
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
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    caller: Option<&str>,
    scopes: &[String],
    branched: bool,
) -> Option<Resolution> {
    if branched && stored.language == "python" {
        return bind_sole_declaration(stored, hits, caller, scopes);
    }
    bind_nearest_member(stored, hits, scopes)
}

/// Binds a member call whose language searches the hierarchy in an order this
/// walk cannot reproduce.
///
/// The caller heads every order a language can choose, and a member exactly
/// one base declares is reached by every order too, so both still bind. Only
/// several bases declaring it make the answer a function of the order, which
/// stays pending rather than binding the base the walk happens to reach first.
fn bind_sole_declaration(
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    caller: Option<&str>,
    scopes: &[String],
) -> Option<Resolution> {
    if let Some(caller) = caller {
        match declarations_in(hits, caller).as_slice() {
            [] => {}
            [hit] => return Some(member_resolution(stored, hit)),
            _ => return None,
        }
    }
    let mut declaration: Option<&NameIndexEntry> = None;
    for scope in scopes {
        if Some(scope.as_str()) == caller {
            continue;
        }
        match declarations_in(hits, scope).as_slice() {
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
    hits: &[NameIndexEntry],
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
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
    let caller = (!superclass).then_some(enclosing.as_str());
    let mut scopes = Vec::new();
    if !superclass {
        scopes.push(enclosing.clone());
    }
    let branched = collect_base_scopes(
        store,
        snapshot,
        &enclosing,
        &mut scopes,
        &mut HashSet::new(),
        // `super` addresses the direct bases only; a self receiver walks the
        // whole chain a fixed number of hops out.
        if superclass { 1 } else { 8 },
    )?;
    Ok(bind_hierarchy_member(
        stored, hits, caller, &scopes, branched,
    ))
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
    hits: &[NameIndexEntry],
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    // The self family has its own hierarchy rule; typed bindings only
    // address named locals and parameters.
    if receiver == "super" || is_self_receiver(&stored.language, receiver) {
        return Ok(None);
    }
    let bindings = store.declared_bindings(&reference.owner_id)?;
    let bound_type = bindings
        .into_iter()
        .filter(|binding| {
            binding.name == receiver
                && usize::try_from(binding.line).is_ok_and(|line| line <= reference.line)
        })
        .map(|binding| binding.bound_type)
        .next_back(); // bindings are line-ordered; the latest earlier one shadows
    let Some(bound_type) = bound_type else {
        return Ok(None);
    };
    let Some(candidates) = snapshot.lookup_candidates(&bound_type) else {
        // Hidden type candidates keep this rule out of the sweep instead of
        // matching against an unfinished snapshot.
        return Ok(None);
    };
    // Constructors (`X::X`) and impl blocks index under the type's name
    // inside the type's declaring scope, so the binding identifies the type
    // by the one scope its candidates share; candidates spelling several
    // distinct scopes keep the type ambiguous.
    let mut shared_scopes = candidates.iter().map(|candidate| {
        candidate.qualified_name.rsplit_once("::").map_or_else(
            || candidate.qualified_name.clone(),
            |(scope, _)| scope.to_owned(),
        )
    });
    let Some(type_scope) = shared_scopes.next() else {
        return Ok(None);
    };
    if shared_scopes.any(|scope| scope != type_scope) {
        return Ok(None);
    }
    // The member's declaring scope is the bound type's own, then its
    // ancestors nearest first, mirroring the self-receiver hierarchy.
    let mut scopes = vec![type_scope.clone()];
    let branched = collect_base_scopes(
        store,
        snapshot,
        &type_scope,
        &mut scopes,
        &mut HashSet::new(),
        8,
    )?;
    Ok(bind_hierarchy_member(
        stored,
        hits,
        Some(type_scope.as_str()),
        &scopes,
        branched,
    ))
}

/// Appends the qualified names of every base class reachable from
/// `class_scope` through extends/implements edges, nearest first: the direct
/// bases before their own bases, so reading `scopes` in order walks the
/// hierarchy by distance from `class_scope` — the order member lookup needs
/// to let an override shadow the declarations of the bases it replaces.
/// `remaining` bounds the chain depth (one hop for `super` receivers, a
/// fixed limit for the transitive `self` chain) and `visited` guards against
/// inheritance cycles; a class whose entities spell its scope ambiguously
/// ends that branch conservatively.
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
    class_scope: &str,
    scopes: &mut Vec<String>,
    visited: &mut HashSet<String>,
    remaining: usize,
) -> crate::EngineResult<bool> {
    visited.insert(class_scope.to_owned());
    let mut frontier = vec![class_scope.to_owned()];
    let mut branched = false;
    for _ in 0..remaining {
        let mut bases = Vec::new();
        for scope in &frontier {
            let Some(class_id) = class_entity_id(snapshot, scope) else {
                continue;
            };
            let (resolved, pending) = store.inheritance_bases(&class_id)?;
            // Clauses, not resolved bases: a base the index cannot place still
            // occupies a position in the order the language searches.
            branched |= resolved.len() + pending.len() > 1;
            let mut declared = resolved;
            for name in pending {
                // Hidden candidates must not turn an ambiguous base name into
                // a seemingly unique ancestor.
                let candidates = prefer_definitions(
                    snapshot
                        .lookup_candidates(&name)
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
            let scope = base.qualified_name.clone();
            if visited.insert(scope.clone()) {
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
/// its last `::` segment in the name index. A scope no entity spells
/// exactly, or several that do, keeps the hierarchy unknown.
fn class_entity_id(snapshot: &ResolutionSnapshot, class_scope: &str) -> Option<String> {
    let tail = class_scope.rsplit("::").next()?;
    let hits = snapshot.lookup_candidates(tail)?;
    let matched: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == class_scope)
        .collect();
    // A prototype and the definition of one class share the qualified name;
    // they name one symbol, so the hierarchy resolves through either.
    let matched = prefer_definitions(matched);
    if let [hit] = matched.as_slice() {
        return Some(hit.entity_id.clone());
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
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
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
        .collect();
    if let [hit] = matches.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
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
fn resolve_java_import(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
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
fn resolve_export_bound(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
    if !matches!(stored.language.as_str(), "typescript" | "javascript")
        || !matches!(
            stored.reference.ref_kind,
            GraphEdgeKind::Calls | GraphEdgeKind::Extends | GraphEdgeKind::Implements
        )
    {
        return None;
    }
    let exported: Vec<&NameIndexEntry> = hits.iter().filter(|hit| hit.is_exported).collect();
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
/// An argument count says nothing across scopes — two classes may declare a
/// `match` taking one argument — so a receiver that reproduces no candidate's
/// scope narrows nothing and leaves the reference to the caller's guard.
fn resolve_receiver_arity(
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
) -> Option<Resolution> {
    let receiver = stored.reference.receiver_name.as_deref()?;
    let expected = stored.reference.arity?;
    let scope = receiver.rsplit("::").next().unwrap_or(receiver);
    let hit = unique_arity_match(
        hits.iter().filter(|hit| {
            hit.qualified_name
                .rsplit_once("::")
                .is_some_and(|(owner, _)| owner == scope)
        }),
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
fn resolve_arity_bound(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
    let expected = stored.reference.arity?;
    let hit = unique_arity_match(hits.iter(), expected)?;
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
    fn a_parameter_name_does_not_separate_one_overload() {
        assert!(same_parameters("int", "int value"));
        assert!(same_parameters("const char*", "const char* path"));
        assert!(same_parameters(
            "const std::string&, int",
            "const std::string& name, int"
        ));
    }

    #[test]
    fn a_type_word_is_no_parameter_name() {
        // `unsigned` and `unsigned long` are different parameters, so reading
        // `long` as a name would merge them.
        assert!(!same_parameters("unsigned", "unsigned long"));
        assert!(!same_parameters("unsigned long", "unsigned"));
    }

    #[test]
    fn different_types_and_counts_never_pair() {
        assert!(!same_parameters("const char*", "int"));
        assert!(!same_parameters("int, int", "int"));
    }
}

#[cfg(test)]
mod language_gate_tests {
    use super::language_compatible;
    use crate::domain::FileFormat;

    #[test]
    fn unknown_reference_language_keeps_every_candidate() {
        // `unknown` marks a reference the walk could not tag with a language,
        // not a language whose candidates must spell `unknown` too.
        assert!(language_compatible(
            Some(FileFormat::Unknown),
            Some("src/lib.rs")
        ));
        assert!(language_compatible(Some(FileFormat::Unknown), None));
    }

    #[test]
    fn missing_evidence_on_either_side_keeps_the_candidate() {
        assert!(language_compatible(None, Some("src/lib.py")));
        assert!(language_compatible(Some(FileFormat::Rust), None));
        // An unregistered extension is unknown evidence, not a mismatch.
        assert!(language_compatible(
            Some(FileFormat::Rust),
            Some("src/data.zzz")
        ));
    }

    #[test]
    fn known_languages_gate_on_the_shared_namespace() {
        assert!(!language_compatible(
            Some(FileFormat::Rust),
            Some("src/lib.py")
        ));
        assert!(language_compatible(
            Some(FileFormat::C),
            Some("src/api.cpp")
        ));
        assert!(language_compatible(
            Some(FileFormat::TypeScript),
            Some("src/main.js")
        ));
    }
}
