//! Cross-file resolution of pending references against the workspace snapshot.
//!
//! The resolver consumes the graph storage's pending queue and edge evidence,
//! plus one [`ResolutionSnapshot`] of the canonical zvec index, so it needs no
//! extraction state: [`IndexStore::list_pending_refs`] pages the backlog,
//! relative import specifiers resolve through [`ResolutionSnapshot::file_id_by_path`],
//! [`ResolutionSnapshot::lookup_candidates`] narrows name candidates, ambiguous
//! bare names resolve through [`ResolutionSnapshot::imported_file_ids`] when
//! the file's imports pin them to one module, `self`/`this` receiver calls
//! bind to the one same-file declaration of the invoked member, qualified
//! `Class::method` receivers bind to the entity whose `qualified_name`
//! spells the same path, export/arity evidence narrows the remaining
//! ambiguities, external import specifiers terminate as failed, and
//! [`IndexStore::apply_resolutions`] writes each batch atomically.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

use crate::domain::{
    EdgeProvenance, FileFormat, GraphEdgeKind, formats_for_path, reference_compatible_with,
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
    /// Receiver-qualified calls bound to a same-file declaration this sweep.
    pub file_scoped: usize,
    /// Qualified references bound through `qualified_name` this sweep.
    pub preferred_file: usize,
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
/// `self`/`this` receiver calls onto the one same-file declaration, and
/// qualified `Class::method` receivers onto the entity declaring that
/// exact `qualified_name`. Ambiguities surviving these rules narrow through
/// export visibility and declared arity when the evidence picks exactly one
/// candidate, and import specifiers that address modules outside the
/// workspace terminate as failed.
///
/// Matching stays conservative. Same-file name hits stay pending — partition
/// already resolved unambiguous local names, so a survivor in the same file
/// is a receiver-typed call that needs type inference, except where the
/// receiver names the enclosing instance (`self`/`this`) and the file declares
/// the member exactly once — and names that stay ambiguous after every
/// evidence rule record their candidate set for consumers.
///
/// The entire read/resolve/writeback cycle runs under the caller's graph
/// storage lock, matching the serialization contract of `apply_resolutions`.
///
/// # Errors
/// Propagates storage failures. Completed writeback batches remain committed;
/// references not yet written stay pending for a later sweep.
#[expect(
    clippy::too_many_lines,
    reason = "One sweep walks the rule chain in order; splitting it would hide the precedence"
)]
pub(crate) fn resolve_pending(store: &IndexStore) -> crate::EngineResult<ResolutionSweep> {
    let snapshot = store.resolution_snapshot()?;
    let mut sweep = ResolutionSweep::default();
    let mut resolutions = Vec::new();
    let mut failures: Vec<i64> = Vec::new();
    let mut candidate_sets: Vec<(i64, Vec<String>)> = Vec::new();
    let mut importing_paths: HashMap<u32, Option<String>> = HashMap::new();
    let mut candidate_paths: HashMap<u32, Option<String>> = HashMap::new();
    let mut importing_files: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut module_paths: HashMap<(String, String), bool> = HashMap::new();
    let mut alias_bindings: HashMap<u32, Vec<AliasBinding>> = HashMap::new();
    let mut alias_targets: HashMap<(String, String), Option<u32>> = HashMap::new();
    let mut cursor = 0;
    loop {
        let page = store.list_pending_refs(1000, cursor)?;
        sweep.scanned += page.refs.len();
        for stored in &page.refs {
            if stored.reference.ref_kind == GraphEdgeKind::Declares {
                // Binding evidence for `resolve_receiver_typed`, not a call
                // site: the bound variable name must never reach the
                // name-index lookup below. Declares edges stay pending in
                // the table for every later sweep to re-read.
                continue;
            }
            if is_unresolvable_import(&snapshot, stored, &mut module_paths) {
                failures.push(stored.id);
                continue;
            }
            if let Some(resolution) = resolve_import(&snapshot, stored, &mut importing_paths) {
                // The batch applies after the loop, so fold this sweep's fresh
                // import targets into the in-memory scope for bare names.
                if let Some(target_file) = resolution
                    .target_id
                    .strip_prefix('f')
                    .and_then(|digits| digits.parse::<u32>().ok())
                {
                    let modules = match importing_files.entry(stored.file_id) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            entry.insert(snapshot.imported_file_ids(stored.file_id).to_vec())
                        }
                    };
                    if !modules.contains(&target_file) {
                        modules.push(target_file);
                    }
                }
                resolutions.push(resolution);
                continue;
            }
            if let Some(resolution) = resolve_alias_bound(
                store,
                &snapshot,
                stored,
                &mut alias_bindings,
                &mut importing_paths,
                &mut alias_targets,
            )? {
                // An `as` alias never names an indexed symbol itself, so the
                // bare-name lookup below would only park the call; the alias
                // binding pins its imported name's module directly.
                resolutions.push(resolution);
                sweep.import_bound += 1;
                continue;
            }
            if let Some(resolution) = resolve_namespace_bound(
                store,
                &snapshot,
                stored,
                &mut alias_bindings,
                &mut importing_paths,
                &mut alias_targets,
            )? {
                // The receiver names a module object, so the bare-name lookup
                // below would only see workspace-wide ambiguity; the binding
                // scopes the member to its one module.
                resolutions.push(resolution);
                sweep.import_bound += 1;
                continue;
            }
            let hits = {
                let Some(raw) = snapshot.lookup_candidates(&stored.name_tail) else {
                    // Hidden candidates must not turn an ambiguous name into
                    // a seemingly unique match against an unfinished snapshot.
                    continue;
                };
                filter_compatible_hits(&snapshot, stored, raw, &mut candidate_paths)
            };
            if let Some(resolution) = resolve_receiver_local(stored, &hits) {
                resolutions.push(resolution);
                sweep.file_scoped += 1;
                continue;
            }
            if let Some(resolution) = resolve_receiver_class(store, &snapshot, stored, &hits)? {
                // The receiver names the enclosing instance or superclass, so
                // the member's declaring scope is pinned to the owner's class
                // hierarchy before workspace-wide ambiguity is considered.
                resolutions.push(resolution);
                sweep.file_scoped += 1;
                continue;
            }
            if let Some(resolution) = resolve_receiver_typed(store, &snapshot, stored, &hits)? {
                // The receiver names a local whose declared type pins the
                // member's declaring scope before workspace-wide ambiguity
                // is considered.
                resolutions.push(resolution);
                sweep.file_scoped += 1;
                continue;
            }
            if let Some(resolution) = resolve_qualified_name(stored, &hits) {
                resolutions.push(resolution);
                sweep.preferred_file += 1;
                continue;
            }
            if let [hit] = hits.as_slice() {
                if hit.file_id != stored.file_id {
                    resolutions.push(Resolution {
                        ref_id: stored.id,
                        target_id: hit.entity_id.clone(),
                        provenance: EdgeProvenance::WorkspaceUnique,
                    });
                } else if stored.reference.receiver_name.is_none() {
                    // Bare name, same file, single hit: the in-file symbol table
                    // skips declaration entities, so a pending bare call can only
                    // mean this one indexed location. Receiver-qualified calls
                    // still stay pending — a same-file receiver bind would be a
                    // type-inference guess.
                    resolutions.push(Resolution {
                        ref_id: stored.id,
                        target_id: hit.entity_id.clone(),
                        provenance: EdgeProvenance::FileScoped,
                    });
                }
            } else if hits.len() > 1 {
                if let Some(resolution) =
                    resolve_import_bound(&snapshot, stored, &hits, &mut importing_files)
                {
                    resolutions.push(resolution);
                    sweep.import_bound += 1;
                    continue;
                }
                if let Some(resolution) = resolve_export_bound(stored, &hits) {
                    resolutions.push(resolution);
                    sweep.export_bound += 1;
                    continue;
                }
                if let Some(resolution) = resolve_arity_bound(stored, &hits) {
                    resolutions.push(resolution);
                    sweep.arity_bound += 1;
                    continue;
                }
                candidate_sets.push((
                    stored.id,
                    hits.into_iter().map(|hit| hit.entity_id).collect(),
                ));
            }
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = next;
    }
    let stats = store.apply_resolutions(&resolutions)?;
    sweep.resolved = stats.resolved;
    sweep.stale = stats.stale;
    sweep.failed = store.apply_failures(&failures)?;
    if !candidate_sets.is_empty() {
        sweep.candidates_recorded = store.record_candidates(&candidate_sets)?;
    }
    Ok(sweep)
}

/// Drops name-index candidates whose file language cannot host the
/// reference. Every downstream rule — same-file, qualified, workspace
/// uniqueness, import/export/arity bounds, and the recorded candidate
/// sets — then reasons over same-language evidence only. The gate never
/// discards on missing information: unknown reference languages, missing
/// candidate paths, and unregistered extensions all keep their entries.
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
        let keep = match (FileFormat::parse(&stored.language), path.as_deref()) {
            (Some(reference), Some(path)) if !formats_for_path(path).is_empty() => {
                formats_for_path(path)
                    .iter()
                    .any(|candidate| reference_compatible_with(reference, *candidate))
            }
            _ => true,
        };
        if keep {
            filtered.push(hit.clone());
        }
    }
    filtered
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
    let module = match stored.language.as_str() {
        "python" if !reference.ref_name.starts_with('.') => reference.ref_name.replace('.', "/"),
        _ if reference.ref_name.starts_with("./") || reference.ref_name.starts_with("../") => {
            reference.ref_name.clone()
        }
        _ => return None,
    };
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
    let base = match stored.language.as_str() {
        "python" => join_specifier("", &module),
        _ => join_specifier(importing_dir(&path), &module),
    };
    let base = base?;
    for candidate in path_variants(&base, extensions_for(&stored.language)) {
        if let Some(file_id) = snapshot.file_id_by_path(&candidate) {
            return Some(Resolution {
                ref_id: stored.id,
                target_id: format!("f{file_id}"),
                provenance: EdgeProvenance::ImportScoped,
            });
        }
    }
    None
}

/// Resolves an ambiguous bare-name reference through the file's imports:
/// when the imports pin the name to exactly one module and that module
/// declares it exactly once, the reference targets that definition.
/// Receiver-typed calls wait for receiver inference, and a local definition
/// shadows any import binding, so both stay with the general ambiguity path.
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
        || hits.iter().any(|hit| hit.file_id == stored.file_id)
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
fn alias_target_file(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> Option<u32> {
    let language = stored.language.as_str();
    match targets.entry((language.to_owned(), binding.specifier.clone())) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => *entry.insert(alias_target_file_uncached(
            snapshot,
            stored,
            binding,
            importing_paths,
        )),
    }
}

/// Uncached specifier resolution behind [`alias_target_file`].
fn alias_target_file_uncached(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> Option<u32> {
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
            let first = crate_path.split("::").find(|segment| !segment.is_empty())?;
            first.to_owned()
        }
        _ => return None,
    };
    let base = match language {
        "typescript" | "javascript" => {
            let path = match importing_paths.entry(stored.file_id) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => entry
                    .insert(snapshot.file_path(stored.file_id).map(str::to_owned))
                    .clone(),
            };
            let path = path?;
            join_specifier(importing_dir(&path), &module)?
        }
        // Python and Rust specifiers address the workspace root.
        _ => module,
    };
    let mut variants = path_variants(&base, extensions_for(language));
    if language == "rust" {
        // Rust modules live in `<base>.rs` or `<base>/mod.rs`; the index
        // variant path_variants emits covers neither.
        variants.push(format!("{base}/mod.rs"));
    }
    for candidate in variants {
        if let Some(file_id) = snapshot.file_id_by_path(&candidate) {
            return Some(file_id);
        }
    }
    None
}

/// Binds a `self.`/`this.` receiver call to the one entity declaring the
/// invoked member in the same file: the receiver names the enclosing
/// instance, so a unique same-file declaration is that member's definition.
/// Members inherited from base classes and members declared more than once
/// in the file stay pending.
fn resolve_receiver_local(
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
) -> Option<Resolution> {
    if !matches!(
        stored.reference.receiver_name.as_deref(),
        Some("self" | "this")
    ) {
        return None;
    }
    let local: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.file_id == stored.file_id)
        .collect();
    if let [hit] = local.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: EdgeProvenance::FileScoped,
        });
    }
    None
}

/// Resolves a receiver call that names the enclosing instance (`self`,
/// `this`, Python's `me`) or the superclass (`super`): the receiver's type
/// is the class owning the call site, so the invoked member's declaring
/// scope must be that class or — through the class's extends/implements
/// edges — one of its bases. `super` skips the enclosing class and
/// addresses the direct bases only. A unique scope-matched declaration
/// binds; members the hierarchy declares ambiguously and receivers of
/// classes without a persisted owner record stay pending.
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
    let self_names: &[&str] = if stored.language == "python" {
        &["self", "me"]
    } else {
        &["self", "this"]
    };
    if !superclass && !self_names.contains(&receiver) {
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
    let mut scopes = Vec::new();
    if superclass {
        collect_base_scopes(
            store,
            snapshot,
            &enclosing,
            &mut scopes,
            &mut HashSet::new(),
            1,
        )?;
    } else {
        scopes.push(enclosing.clone());
        collect_base_scopes(
            store,
            snapshot,
            &enclosing,
            &mut scopes,
            &mut HashSet::new(),
            8,
        )?;
    }
    let matched: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| {
            hit.qualified_name
                .rsplit_once("::")
                .is_some_and(|(scope, _)| scopes.iter().any(|candidate| candidate == scope))
        })
        .collect();
    if let [hit] = matched.as_slice() {
        return Ok(Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: EdgeProvenance::FileScoped,
        }));
    }
    Ok(None)
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
    // The self/this family has its own hierarchy rule; typed bindings only
    // address named locals and parameters.
    if matches!(receiver, "self" | "this" | "me" | "super") {
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
    // The member's declaring scope is the bound type's own or any ancestor
    // the inheritance chain adds, mirroring the self-receiver hierarchy.
    let mut scopes = vec![type_scope.clone()];
    collect_base_scopes(
        store,
        snapshot,
        &type_scope,
        &mut scopes,
        &mut HashSet::new(),
        8,
    )?;
    let matched: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| {
            hit.qualified_name
                .rsplit_once("::")
                .is_some_and(|(scope, _)| scopes.iter().any(|candidate| candidate == scope))
        })
        .collect();
    if let [hit] = matched.as_slice() {
        return Ok(Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: EdgeProvenance::FileScoped,
        }));
    }
    Ok(None)
}

/// Appends the qualified names of every base class reachable from
/// `class_scope` through extends/implements edges. `remaining` bounds the
/// chain depth (one hop for `super` receivers, a fixed limit for the
/// transitive `self` chain) and `visited` guards against inheritance
/// cycles; a class whose entities spell its scope ambiguously ends the
/// walk conservatively.
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
) -> crate::EngineResult<()> {
    if remaining == 0 || !visited.insert(class_scope.to_owned()) {
        return Ok(());
    }
    let Some(class_id) = class_entity_id(snapshot, class_scope) else {
        return Ok(());
    };
    let (resolved, pending) = store.inheritance_bases(&class_id)?;
    let mut bases = resolved;
    for name in pending {
        // Hidden candidates must not turn an ambiguous base name into a
        // seemingly unique ancestor.
        if let [hit] = snapshot.lookup_candidates(&name).unwrap_or_default() {
            bases.push(hit.entity_id.clone());
        }
    }
    for base_id in bases {
        let Some(base) = snapshot.entity_record(&base_id) else {
            continue;
        };
        scopes.push(base.qualified_name.clone());
        collect_base_scopes(
            store,
            snapshot,
            &base.qualified_name,
            scopes,
            visited,
            remaining - 1,
        )?;
    }
    Ok(())
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
    if let [hit] = matched.as_slice() {
        return Some(hit.entity_id.clone());
    }
    None
}

/// Binds a qualified call (`Class::method()`, including receivers spelled as
/// full `crate::path::Class` paths reduced to their last `::` segment) to the
/// entity whose `qualified_name` spells exactly that scope and member. The
/// qualified name carries the declaring scope, so a unique match resolves
/// even when the short member name is ambiguous workspace-wide. Duplicate
/// `qualified_name` declarations and instance-style dotted receivers, which
/// never spell a `::`-scoped path, stay pending.
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

/// Reports import references whose specifier can never address a workspace
/// module: TS/JS bare package specifiers (anything not starting with `./`
/// or `../`, reduced to its first path segment) and Python's bare top-level
/// module imports (`import re`). A specifier whose leading segment matches
/// a workspace file or directory — a monorepo package, a top-level module —
/// stays in the queue for path-based rules instead of failing.
///
/// Scoped specifiers (`@scope/pkg`) and path aliases (`@/x`) always stay
/// queued: a workspace's own package name or `paths` mapping can route them
/// inside the workspace while no file or directory spells the scope, so
/// existence cannot be proven either way. They are safe to hold — their
/// name tails contain `/`, which entity names never do, so the uniqueness
/// rule cannot mis-bind them.
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

/// Narrows an ambiguous reference to the one candidate whose declared
/// parameter count equals the call's argument count: among candidates that
/// all declare their arity, a single match is the invoked overload.
/// Candidates with unknown arity cannot rule themselves out, so their
/// presence keeps the reference pending.
fn resolve_arity_bound(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
    let expected = stored.reference.arity?;
    let mut matched = None;
    for hit in hits {
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
    let hit = matched?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: EdgeProvenance::ArityBound,
    })
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

/// Path candidates for one specifier: the literal base first (explicit
/// extensions win), then per-language extensions, then directory index files.
fn path_variants(base: &str, extensions: &[&str]) -> Vec<String> {
    let mut variants = vec![base.to_owned()];
    variants.extend(
        extensions
            .iter()
            .map(|extension| format!("{base}{extension}")),
    );
    variants.extend(
        extensions
            .iter()
            .map(|extension| format!("{base}/index{extension}")),
    );
    variants
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
