//! Resolve values and receiver types from lexical and module evidence.
//!
//! Value lookup walks the nearest declaration outward, then follows copied
//! aliases at their assignment position. Unknown bindings stop fallback so a
//! shadowed name cannot accidentally select a workspace-wide candidate.
//! Receiver lookup materializes factory returns/awaited types and keeps the
//! declaring file attached while selecting a type and its members.
//!
//! File initializers use source order; deferred function bodies use the module
//! binding view. Imported bindings use the exporting file's view, never the
//! importing file's coordinates. Recursive alias/import walks are bounded and
//! unresolved or ambiguous evidence remains unresolved.

#[cfg(test)]
mod tests;
use super::{
    AliasBinding, Entry, HashSet, IndexStore, NameIndexEntry, Outcome, Resolution,
    ResolutionSnapshot, Rule, RuleCaches, StoredPendingRef, SymbolType, alias_target_file,
    bind_hierarchy_member, collect_base_scopes, declares_in, importing_dir, is_type_scope,
    member_resolution, shadows_import, visible_scopes,
};

#[derive(Clone)]
struct Binding {
    ty: String,
    file_id: u32,
}

fn declaration_at(
    store: &IndexStore,
    caches: &mut RuleCaches,
    owner: &str,
    name: &str,
    position: (usize, usize),
) -> crate::EngineResult<Option<super::DeclaredBinding>> {
    let bindings = match caches.declared_bindings.entry(owner.to_owned()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(store.declared_bindings(owner)?),
    };
    let active = |binding: &&super::DeclaredBinding| {
        usize::try_from(binding.line).is_ok_and(|at| (at, binding.column as usize) <= position)
            && binding.scope_end.is_none_or(|end| position < end)
    };
    let binding = bindings
        .iter()
        .rev()
        .filter(active)
        .find(|binding| binding.name == name);
    // A newer assignment to any receiver prefix replaces its old properties.
    // Properties collected from the new object occur after that assignment.
    Ok(binding
        .filter(|binding| {
            !name.contains('.')
                || !bindings.iter().filter(active).any(|receiver| {
                    name.strip_prefix(&receiver.name)
                        .is_some_and(|suffix| suffix.starts_with('.'))
                        && (receiver.line, receiver.column) > (binding.line, binding.column)
                })
        })
        .cloned())
}

fn declared(
    store: &IndexStore,
    caches: &mut RuleCaches,
    owner: &str,
    name: &str,
    position: (usize, usize),
) -> crate::EngineResult<Option<String>> {
    Ok(declaration_at(store, caches, owner, name, position)?.map(|binding| binding.bound_type))
}

fn lexical_binding(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    name: &str,
) -> crate::EngineResult<Option<Binding>> {
    if let Some(ty) = declared(
        store,
        caches,
        &stored.reference.owner_id,
        name,
        (stored.reference.line, stored.reference.column),
    )? {
        return Ok(Some(Binding {
            ty,
            file_id: stored.file_id,
        }));
    }
    if let Some(binding) = enclosing_binding(store, snapshot, stored, caches, name)? {
        return Ok(Some(binding));
    }
    if let Some(ty) = declared(
        store,
        caches,
        &format!("f{}", stored.file_id),
        name,
        file_position(snapshot, stored),
    )? {
        return Ok(Some(Binding {
            ty,
            file_id: stored.file_id,
        }));
    }
    let visible = visible_scopes(snapshot, &stored.reference.owner_id);
    let mut variables: Vec<_> = snapshot
        .lookup_candidates(name)
        .unwrap_or_default()
        .iter()
        .filter(|hit| {
            hit.file_id == stored.file_id
                && declares_in(hit, &visible)
                && hit.symbol_type == Some(SymbolType::Value)
        })
        .collect();
    variables.sort_by_key(|hit| std::cmp::Reverse(hit.qualified_name.len()));
    for variable in variables {
        if let Some(ty) = declared(
            store,
            caches,
            &variable.entity_id,
            name,
            file_position(snapshot, stored),
        )? {
            return Ok(Some(Binding {
                ty,
                file_id: variable.file_id,
            }));
        }
    }
    if let Some((file, original)) =
        named_import(store, snapshot, stored, caches, stored.file_id, name, false)?
        && let Some(ty) = declared(
            store,
            caches,
            &format!("f{file}"),
            &original,
            (usize::MAX, usize::MAX),
        )?
    {
        return Ok(Some(Binding { ty, file_id: file }));
    }
    Ok(None)
}

fn enclosing_binding(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    name: &str,
) -> crate::EngineResult<Option<Binding>> {
    let Some(owner) = snapshot.entity_record(&stored.reference.owner_id) else {
        return Ok(None);
    };
    let mut scope = owner.qualified_name.as_str();
    while let Some((parent, _)) = scope.rsplit_once("::") {
        let short = parent.rsplit("::").next().unwrap_or(parent);
        for candidate in snapshot.lookup_candidates(short).unwrap_or_default() {
            if candidate.file_id == stored.file_id
                && candidate.qualified_name == parent
                && let Some(ty) = declared(
                    store,
                    caches,
                    &candidate.entity_id,
                    name,
                    if stored.language == "java" && candidate.symbol_type.is_some_and(is_type_scope)
                    {
                        (usize::MAX, usize::MAX)
                    } else {
                        (stored.reference.line, stored.reference.column)
                    },
                )?
            {
                return Ok(Some(Binding {
                    ty,
                    file_id: stored.file_id,
                }));
            }
        }
        scope = parent;
    }
    Ok(None)
}

/// Select a type in the binding's file/import scope, rather than combining
/// identically named classes from different modules.
fn type_definition(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    binding: &Binding,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let Some(binding) = materialize_type(store, snapshot, stored, caches, binding)? else {
        return Ok(None);
    };
    let mut name = binding.ty.as_str();
    if matches!(stored.language.as_str(), "typescript" | "javascript") {
        name = name.split('<').next().unwrap_or(name);
        if let Some(hit) = symbol_at(store, snapshot, stored, caches, binding.file_id, name, 8)? {
            return Ok(hit.symbol_type.is_some_and(is_type_scope).then_some(hit));
        }
        if let Some(hit) = fallback_type(
            store,
            snapshot,
            caches,
            binding.file_id,
            name,
            &stored.language,
        )? {
            return Ok(Some(hit));
        }
        return script_type(store, snapshot, stored, caches, binding.file_id, name);
    }
    if stored.language == "java" {
        let mut binding = binding;
        for _ in 0..8 {
            if let Some(hit) = java_type_definition(store, snapshot, stored, caches, &binding)? {
                return Ok(Some(hit));
            }
            let Some(bound) = lexical_binding(
                store,
                snapshot,
                stored,
                caches,
                &format!("[[type:{}]]", binding.ty),
            )?
            else {
                break;
            };
            if bound.ty == binding.ty {
                break;
            }
            binding = bound;
        }
        return Ok(None);
    }
    if stored.language == "go"
        && let Some((package, short)) = name.rsplit_once('.')
    {
        let Some(directory) =
            go_import_directory(store, snapshot, caches, binding.file_id, package)?
        else {
            return Ok(None);
        };
        let candidates: Vec<_> = snapshot
            .lookup_candidates(short)
            .unwrap_or_default()
            .iter()
            .filter(|hit| {
                hit.symbol_type
                    .is_some_and(|kind| is_type_scope(kind) || kind == SymbolType::Alias)
                    && snapshot
                        .file_path(hit.file_id)
                        .is_some_and(|path| importing_dir(path) == directory)
            })
            .collect();
        return Ok(match candidates.as_slice() {
            [hit] => Some((*hit).clone()),
            _ => None,
        });
    }
    name = name.trim();
    let candidates: Vec<_> = snapshot
        .lookup_candidates(name)
        .unwrap_or_default()
        .iter()
        .filter(|hit| {
            hit.symbol_type
                .is_some_and(|kind| is_type_scope(kind) || kind == SymbolType::Alias)
                && snapshot
                    .file_path(hit.file_id)
                    .zip(snapshot.file_path(binding.file_id))
                    .is_some_and(|(left, right)| importing_dir(left) == importing_dir(right))
        })
        .collect();
    Ok(match candidates.as_slice() {
        [hit] => Some((*hit).clone()),
        _ => None,
    })
}

/// Java types are scoped by their declaration/import and package. An explicit
/// external import must not fall back to an unrelated same-named local class.
fn java_type_definition(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    binding: &Binding,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let name = binding.ty.replace('.', "::");
    let short = name.rsplit("::").next().unwrap_or(&name);
    let candidates: Vec<_> = snapshot
        .lookup_candidates(short)
        .unwrap_or_default()
        .iter()
        .filter(|hit| hit.symbol_type.is_some_and(is_type_scope))
        .collect();
    let local: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|hit| hit.file_id == binding.file_id && hit.qualified_name == name)
        .collect();
    if !local.is_empty() {
        return Ok(match local.as_slice() {
            [hit] => Some((*hit).clone()),
            _ => None,
        });
    }
    let imports = caches.imports(store, binding.file_id)?;
    let explicit: Vec<_> = imports
        .iter()
        .filter(|import| import.specifier.rsplit('.').next() == Some(short))
        .collect();
    let package = snapshot
        .entity_record(&stored.reference.owner_id)
        .and_then(|owner| snapshot.lookup_candidates(&owner.name))
        .and_then(|hits| {
            hits.iter()
                .find(|hit| hit.entity_id == stored.reference.owner_id)
        })
        .and_then(|hit| hit.package.as_deref());
    let matches: Vec<_> = candidates
        .into_iter()
        .filter(|hit| {
            if binding.ty.contains('.') {
                super::java_import_match(hit.package.as_deref(), &binding.ty, &hit.qualified_name)
            } else if !explicit.is_empty() {
                explicit.iter().any(|import| {
                    super::java_import_match(
                        hit.package.as_deref(),
                        &import.specifier,
                        &hit.qualified_name,
                    )
                })
            } else {
                binding.file_id == stored.file_id
                    && package.is_some()
                    && hit.package.as_deref() == package
                    && hit.qualified_name == name
            }
        })
        .collect();
    Ok(match matches.as_slice() {
        [hit] => Some((*hit).clone()),
        _ => None,
    })
}

/// Construction never falls through to ordinary name/member lookup: a class
/// or a same-named method cannot stand in for an ambiguous explicit constructor.
pub(super) fn resolve_java_constructor(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if stored.language != "java" || stored.reference.ref_kind != super::GraphEdgeKind::Calls {
        return Ok(None);
    }
    let raw = stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str);
    let receiver = stored.reference.receiver_name.as_deref();
    let chained = matches!(
        (receiver, raw),
        (Some("this"), Some("this")) | (Some("super"), Some("super"))
    );
    if !chained && !raw.is_some_and(|text| text.starts_with("new ")) {
        return Ok(None);
    }
    let ty = if chained {
        snapshot
            .entity_record(&stored.reference.owner_id)
            .and_then(|owner| owner.qualified_name.rsplit_once("::"))
            .map(|(scope, _)| scope)
    } else {
        receiver
    };
    let Some(ty) = ty else {
        return Ok(Some(Outcome::Deferred));
    };
    let Some(mut owner) = java_type_definition(
        store,
        snapshot,
        stored,
        caches,
        &Binding {
            ty: ty.to_owned(),
            file_id: stored.file_id,
        },
    )?
    else {
        return Ok(Some(Outcome::Deferred));
    };
    if receiver == Some("super") {
        let Some(base) = java_superclass(store, snapshot, stored, caches, &owner)? else {
            return Ok(Some(Outcome::Deferred));
        };
        owner = base;
    }
    if owner.symbol_type != Some(SymbolType::Class) {
        return Ok(Some(Outcome::Deferred));
    }
    let name = owner
        .qualified_name
        .rsplit([':', '.'])
        .next()
        .unwrap_or(&owner.qualified_name);
    let qualified = format!("{}::{name}", owner.qualified_name);
    let constructors: Vec<_> = snapshot
        .lookup_candidates(name)
        .unwrap_or_default()
        .iter()
        .filter(|hit| {
            hit.file_id == owner.file_id && hit.qualified_name == qualified && hit.is_constructor
        })
        .collect();
    let target = if constructors.is_empty() && stored.reference.arity == Some(0) {
        // With no explicit constructor, the zero-argument default is represented
        // by the class. Never inherit a constructor from a more distant base.
        Some(&owner)
    } else {
        stored
            .reference
            .arity
            .and_then(|arity| super::unique_arity_match(constructors.into_iter(), arity))
    };
    Ok(Some(target.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: member_resolution(stored, hit),
            rule: Rule::Receiver,
        }
    })))
}

fn java_superclass(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    owner: &NameIndexEntry,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let (resolved, pending) = store.inheritance_bases(&owner.entity_id)?;
    let mut bases = Vec::new();
    for id in resolved {
        if let Some(record) = snapshot.entity_record(&id)
            && let Some(hit) = snapshot
                .lookup_candidates(&record.name)
                .unwrap_or_default()
                .iter()
                .find(|hit| hit.entity_id == id && hit.symbol_type == Some(SymbolType::Class))
        {
            bases.push(hit.clone());
        }
    }
    for name in pending {
        if let Some(hit) = java_type_definition(
            store,
            snapshot,
            stored,
            caches,
            &Binding {
                ty: name,
                file_id: owner.file_id,
            },
        )? && hit.symbol_type == Some(SymbolType::Class)
            && !bases.iter().any(|base| base.entity_id == hit.entity_id)
        {
            bases.push(hit);
        }
    }
    Ok(match bases.as_slice() {
        [base] => Some(base.clone()),
        _ => None,
    })
}

/// Configured path aliases and workspace packages can supply an exported type.
/// Package lookup requires a unique type within the named workspace package.
/// Unimported TypeScript types must come from a global script declaration.
fn fallback_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
    language: &str,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let (local, member) = name
        .split_once('.')
        .map_or((name, None), |(local, member)| (local, Some(member)));
    let mut imported = None;
    let mut specifier = None;
    for import in javascript_imports(store, caches, file)?.iter() {
        if import.reexport {
            continue;
        }
        let Some(original) = import.names.get(local).cloned() else {
            continue;
        };
        // An explicit relative path remains authoritative even if its target
        // is absent. Never substitute a same-named type from another module.
        if matches!(import.specifier.as_str(), "." | "..")
            || import.specifier.starts_with("./")
            || import.specifier.starts_with("../")
        {
            return Ok(None);
        }
        let original = match (original.as_str(), member) {
            ("*", Some(member)) => member.to_owned(),
            ("*" | "default", _) | (_, Some(_)) => return Ok(None),
            _ => original,
        };
        if imported.replace(original).is_some() {
            return Ok(None);
        }
        specifier = Some(import.specifier.clone());
    }
    if imported.is_none() && language != "typescript" {
        return Ok(None);
    }
    let name = imported.as_deref().unwrap_or(name);
    let mut selected = None;
    for hit in snapshot.lookup_candidates(name).unwrap_or_default() {
        if hit.qualified_name != name || !hit.symbol_type.is_some_and(is_type_scope) {
            continue;
        }
        let path = snapshot.file_path(hit.file_id).unwrap_or_default();
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str());
        if !matches!(
            extension,
            Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
        ) {
            continue;
        }
        if let Some(specifier) = &specifier {
            if !hit.is_exported || !type_import_matches(snapshot, caches, file, hit, specifier) {
                continue;
            }
        } else if !matches!(extension, Some("ts" | "tsx"))
            || declared(
                store,
                caches,
                &format!("f{}", hit.file_id),
                "[[typescript_scope]]",
                END_OF_SCOPE,
            )?
            .as_deref()
                != Some("script")
        {
            continue;
        }
        if selected.replace(hit.clone()).is_some() {
            return Ok(None);
        }
    }
    Ok(selected)
}

fn type_import_matches(
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
    candidate: &NameIndexEntry,
    specifier: &str,
) -> bool {
    let Some(path) = snapshot.file_path(file) else {
        return false;
    };
    for directory in std::path::Path::new(path)
        .parent()
        .into_iter()
        .flat_map(std::path::Path::ancestors)
    {
        let directory = directory.to_string_lossy().replace('\\', "/");
        let config_path = format!("{directory}/tsconfig.json")
            .trim_start_matches('/')
            .to_owned();
        let config = match super::module_config::typescript(caches, &config_path) {
            Ok(Some(config)) => config,
            Ok(None) => continue,
            Err(()) => return false,
        };
        // Exact mappings win, then the matching wildcard with longest prefix.
        let mapping = config
            .paths
            .iter()
            .flat_map(|paths| paths.iter())
            .filter_map(|(pattern, targets)| {
                if pattern == specifier {
                    return Some((usize::MAX, "", targets));
                }
                let (prefix, suffix) = pattern.split_once('*')?;
                let capture = specifier.strip_prefix(prefix)?.strip_suffix(suffix)?;
                Some((prefix.len(), capture, targets))
            })
            .max_by_key(|(priority, _, _)| *priority);
        if let Some((_, capture, targets)) = mapping {
            let base = config
                .base_url
                .as_deref()
                .unwrap_or(&config.paths_directory);
            for target in targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
            {
                let target = target.replace('*', capture);
                if let Some(base) = super::join_specifier(base, &target)
                    && let Some(target_file) = super::alias_target_at(snapshot, "typescript", &base)
                {
                    return target_file == candidate.file_id;
                }
            }
            return false;
        }
        // A nearer tsconfig is authoritative, including its explicit inheritance.
        break;
    }
    let Some(path) = snapshot.file_path(candidate.file_id) else {
        return false;
    };
    for directory in std::path::Path::new(path)
        .parent()
        .into_iter()
        .flat_map(std::path::Path::ancestors)
    {
        let config_path = directory
            .join("package.json")
            .to_string_lossy()
            .replace('\\', "/");
        match super::module_config::read(caches, &config_path) {
            Ok(None) => {}
            Ok(Some(config)) => return config["name"].as_str() == Some(specifier),
            Err(()) => return false,
        }
    }
    false
}

/// Classic JavaScript scripts share top-level declarations. Modules and
/// ambiguous global definitions do not participate in this fallback.
fn script_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if stored.language != "javascript"
        || !is_script(store, snapshot, caches, file)?
        || lexical_binding(store, snapshot, stored, caches, name)?.is_some()
    {
        return Ok(None);
    }
    let mut target = None;
    for hit in snapshot.lookup_candidates(name).unwrap_or_default() {
        if hit.qualified_name == name && is_script(store, snapshot, caches, hit.file_id)? {
            // A conflicting global value also prevents a unique class match.
            if target.is_some() || !hit.symbol_type.is_some_and(is_type_scope) {
                return Ok(None);
            }
            target = Some(hit.clone());
        }
    }
    Ok(target)
}

fn is_script(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
) -> crate::EngineResult<bool> {
    // Explicit module extensions remain modules without import/export syntax.
    if !snapshot.file_path(file).is_some_and(|path| {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext == "js" || ext == "jsx")
    }) {
        return Ok(false);
    }
    Ok(declared(
        store,
        caches,
        &format!("f{file}"),
        "[[javascript_scope]]",
        (usize::MAX, usize::MAX),
    )?
    .as_deref()
        == Some("script"))
}

fn field_binding(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    owner: &NameIndexEntry,
    field: &str,
) -> crate::EngineResult<Option<Binding>> {
    if let Some(ty) = declared(
        store,
        caches,
        &owner.entity_id,
        field,
        (usize::MAX, usize::MAX),
    )? {
        return Ok(Some(Binding {
            ty,
            file_id: owner.file_id,
        }));
    }
    for candidate in snapshot.lookup_candidates(field).unwrap_or_default() {
        if candidate.file_id == owner.file_id
            && candidate.qualified_name == format!("{}::{field}", owner.qualified_name)
            && let Some(ty) = declared(
                store,
                caches,
                &candidate.entity_id,
                field,
                (usize::MAX, usize::MAX),
            )?
        {
            return Ok(Some(Binding {
                ty,
                file_id: candidate.file_id,
            }));
        }
    }
    for constructor in snapshot
        .lookup_candidates("constructor")
        .unwrap_or_default()
    {
        if constructor.file_id == owner.file_id
            && constructor.qualified_name == format!("{}::constructor", owner.qualified_name)
            && let Some(ty) = declared(
                store,
                caches,
                &constructor.entity_id,
                &format!("this.{field}"),
                (usize::MAX, usize::MAX),
            )?
        {
            return Ok(Some(Binding {
                ty,
                file_id: constructor.file_id,
            }));
        }
    }
    Ok(None)
}

pub(super) fn resolve(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
    if stored.reference.ref_kind != super::GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(receiver) = stored.reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    let mut parts = receiver.split('.').peekable();
    let Some(root) = parts.next() else {
        return Ok(None);
    };
    if stored.language == "go"
        && !receiver.contains('.')
        && declared(
            store,
            caches,
            &stored.reference.owner_id,
            receiver,
            (stored.reference.line, stored.reference.column),
        )?
        .is_none()
        && let Some(directory) =
            go_import_directory(store, snapshot, caches, stored.file_id, receiver)?
    {
        let candidates: Vec<_> = hits
            .iter()
            .filter(|hit| {
                hit.qualified_name == stored.name_tail
                    && snapshot
                        .file_path(hit.file_id)
                        .is_some_and(|path| importing_dir(path) == directory)
            })
            .collect();
        return Ok(match candidates.as_slice() {
            [hit] => Some(member_resolution(stored, hit)),
            _ => None,
        });
    }
    let binding = if receiver.starts_with("ReturnType<typeof ") {
        Binding {
            ty: receiver.to_owned(),
            file_id: stored.file_id,
        }
    } else if root == "this" {
        let Some(owner) = snapshot.entity_record(&stored.reference.owner_id) else {
            return Ok(None);
        };
        let Some((scope, _)) = owner.qualified_name.rsplit_once("::") else {
            return Ok(None);
        };
        Binding {
            ty: scope.to_owned(),
            file_id: stored.file_id,
        }
    } else {
        match lexical_binding(store, snapshot, stored, caches, root)? {
            Some(binding) => binding,
            None if stored.language == "java" => Binding {
                ty: root.to_owned(),
                file_id: stored.file_id,
            },
            None => return Ok(None),
        }
    };
    let Some(mut owner) = type_definition(store, snapshot, stored, caches, &binding)? else {
        return Ok(None);
    };
    while let Some(field) = parts.next() {
        if receiver.starts_with("ReturnType<typeof ") {
            break;
        }
        let Some(binding) = field_binding(store, snapshot, caches, &owner, field)? else {
            return Ok(None);
        };
        if let Some(object) = binding.ty.strip_prefix("typeof ")
            && parts.peek().is_none()
            && let Some(hit) = symbol_at(
                store,
                snapshot,
                stored,
                caches,
                binding.file_id,
                &format!("{object}::{}", stored.name_tail),
                8,
            )?
        {
            return Ok(Some(member_resolution(stored, &hit)));
        }
        let Some(next) = type_definition(store, snapshot, stored, caches, &binding)? else {
            return Ok(None);
        };
        owner = next;
    }
    resolve_member(store, snapshot, stored, hits, &owner)
}

fn resolve_member(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    owner: &NameIndexEntry,
) -> crate::EngineResult<Option<Resolution>> {
    let own: Vec<_> = hits
        .iter()
        .filter(|hit| {
            (hit.file_id == owner.file_id
                || stored.language == "go"
                    && snapshot
                        .file_path(hit.file_id)
                        .zip(snapshot.file_path(owner.file_id))
                        .is_some_and(|(left, right)| importing_dir(left) == importing_dir(right)))
                && hit
                    .qualified_name
                    .rsplit_once("::")
                    .is_some_and(|(scope, _)| scope == owner.qualified_name)
        })
        .collect();
    if let [hit] = own.as_slice() {
        return Ok(Some(member_resolution(stored, hit)));
    }
    if !own.is_empty() {
        return Ok(None);
    }
    if stored.language == "go" {
        return Ok(None);
    }
    let mut scopes = vec![owner.qualified_name.clone()];
    let branched = collect_base_scopes(
        store,
        snapshot,
        &owner.qualified_name,
        &mut scopes,
        &mut HashSet::new(),
        8,
    )?;
    let hierarchy_hits: Vec<_> = hits
        .iter()
        .filter(|hit| {
            hit.file_id == owner.file_id
                || hit
                    .qualified_name
                    .rsplit_once("::")
                    .is_none_or(|(scope, _)| scope != owner.qualified_name)
        })
        .cloned()
        .collect();
    Ok(bind_hierarchy_member(
        stored,
        &hierarchy_hits,
        Some(&owner.qualified_name),
        &scopes,
        branched,
    ))
}

fn named_import(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
    include_reexports: bool,
) -> crate::EngineResult<Option<(u32, String)>> {
    for import in javascript_imports(store, caches, file)?.iter() {
        if !include_reexports && import.reexport {
            continue;
        }
        let Some(original) = import.names.get(name).cloned() else {
            continue;
        };
        let mut context = stored.clone();
        context.file_id = file;
        let binding = AliasBinding {
            local: name.to_owned(),
            imported: Some(original.clone()),
            specifier: import.specifier.clone(),
        };
        return Ok(alias_target_file(
            snapshot,
            &context,
            &binding,
            &mut caches.importing_paths,
            &mut caches.alias_targets,
        )
        .map(|target| (target, original)));
    }
    Ok(None)
}

// Import statements are parsed once per file/sweep. The stored source text
// remains the evidence; formatting and comments do not define binding names.
pub(super) struct JavascriptImport {
    specifier: String,
    reexport: bool,
    names: std::collections::HashMap<String, String>,
}

fn javascript_imports(
    store: &IndexStore,
    caches: &mut RuleCaches,
    file: u32,
) -> crate::EngineResult<std::sync::Arc<[JavascriptImport]>> {
    if let Some(imports) = caches.javascript_imports.get(&file) {
        return Ok(std::sync::Arc::clone(imports));
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .map_err(|error| crate::EngineError::internal(format!("load import grammar: {error}")))?;
    let mut result = Vec::new();
    for import in caches.imports(store, file)?.iter() {
        let Some(raw) = import.raw_text.as_deref() else {
            continue;
        };
        let keyword = raw
            .trim_start()
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
            .next()
            .unwrap_or_default();
        // CommonJS evidence stores a variable_declarator, without const/let/var.
        let source = if matches!(keyword, "import" | "export" | "const" | "let" | "var") {
            raw.to_owned()
        } else {
            format!("const {raw};")
        };
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        if tree.root_node().has_error() {
            continue;
        }
        let mut names = std::collections::HashMap::new();
        collect_import_names(tree.root_node(), source.as_bytes(), &mut names);
        result.push(JavascriptImport {
            specifier: import.specifier.clone(),
            reexport: keyword == "export",
            names,
        });
    }
    let result: std::sync::Arc<[JavascriptImport]> = result.into();
    caches
        .javascript_imports
        .insert(file, std::sync::Arc::clone(&result));
    Ok(result)
}

fn collect_import_names(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    names: &mut std::collections::HashMap<String, String>,
) {
    let text = |node: tree_sitter::Node<'_>| {
        node.utf8_text(source)
            .unwrap_or_default()
            .trim_matches(['\'', '"'])
            .to_owned()
    };
    match node.kind() {
        "import_specifier" | "export_specifier" => {
            if let Some(original) = node.child_by_field_name("name") {
                let local = node.child_by_field_name("alias").unwrap_or(original);
                names.insert(text(local), text(original));
            }
            return;
        }
        "namespace_import" | "import_require_clause" => {
            if let Some(local) = node.named_child(0) {
                names.insert(
                    text(local),
                    if node.kind() == "import_require_clause" {
                        "default"
                    } else {
                        "*"
                    }
                    .to_owned(),
                );
            }
            return;
        }
        "import_clause" => {
            if let Some(local) = node
                .named_child(0)
                .filter(|node| node.kind() == "identifier")
            {
                names.insert(text(local), "default".to_owned());
            }
        }
        "variable_declarator" => {
            if let (Some(local), Some(value)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("value"),
            ) && value.kind() == "call_expression"
                && value
                    .child_by_field_name("function")
                    .is_some_and(|node| text(node) == "require")
            {
                if local.kind() == "identifier" {
                    names.insert(text(local), "*".to_owned());
                } else if local.kind() == "object_pattern" {
                    let mut cursor = local.walk();
                    for entry in local.named_children(&mut cursor) {
                        if entry.kind() == "shorthand_property_identifier_pattern" {
                            names.insert(text(entry), text(entry));
                        } else if entry.kind() == "pair_pattern"
                            && let (Some(key), Some(value)) = (
                                entry.child_by_field_name("key"),
                                entry.child_by_field_name("value"),
                            )
                            && value.kind() == "identifier"
                        {
                            names.insert(text(value), text(key));
                        }
                    }
                }
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_import_names(child, source, names);
    }
}

fn go_symbol_at(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
    package: &str,
    short: &str,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let Some(directory) = go_import_directory(store, snapshot, caches, file, package)? else {
        return Ok(None);
    };
    let candidates: Vec<_> = snapshot
        .lookup_candidates(short)
        .unwrap_or_default()
        .iter()
        .filter(|hit| {
            snapshot
                .file_path(hit.file_id)
                .is_some_and(|path| importing_dir(path) == directory)
        })
        .collect();
    Ok(match candidates.as_slice() {
        [hit] => Some((*hit).clone()),
        _ => None,
    })
}

fn symbol_at(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
    depth: usize,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    if stored.language == "go"
        && let Some((package, short)) = name.rsplit_once('.')
    {
        return go_symbol_at(store, snapshot, caches, file, package, short);
    }
    if name.contains("::")
        && let Some(ty) = declared(
            store,
            caches,
            &format!("f{file}"),
            &name.replace("::", "."),
            module_position(snapshot, stored, file),
        )?
    {
        let Some(original) = ty.strip_prefix("typeof ") else {
            return Ok(None);
        };
        if original != name {
            return symbol_at(store, snapshot, stored, caches, file, original, depth - 1);
        }
    }
    let visible = if file == stored.file_id {
        visible_scopes(snapshot, &stored.reference.owner_id)
    } else {
        HashSet::from([""])
    };
    let tail = name.rsplit("::").next().unwrap_or(name);
    let candidates: Vec<_> = snapshot
        .lookup_candidates(tail)
        .unwrap_or_default()
        .iter()
        .filter(|hit| {
            hit.file_id == file
                && if name.contains("::") {
                    hit.qualified_name == name
                } else {
                    declares_in(hit, &visible) && shadows_import(snapshot, hit)
                }
        })
        .collect();
    if let [hit] = candidates.as_slice() {
        return Ok(Some((*hit).clone()));
    }
    if !candidates.is_empty() {
        return Ok(None);
    }
    if let Some((container, member)) = name.split_once("::") {
        if let Some(ty) = declared(
            store,
            caches,
            &format!("f{file}"),
            container,
            module_position(snapshot, stored, file),
        )? && let Some(original) = ty.strip_prefix("typeof ")
        {
            return symbol_at(
                store,
                snapshot,
                stored,
                caches,
                file,
                &format!("{original}::{member}"),
                depth - 1,
            );
        }
        if let Some((origin, original)) =
            named_import(store, snapshot, stored, caches, file, container, false)?
        {
            return symbol_at(
                store,
                snapshot,
                stored,
                caches,
                origin,
                &format!("{original}::{member}"),
                depth - 1,
            );
        }
    }
    if let Some(ty) = declared(
        store,
        caches,
        &format!("f{file}"),
        name,
        module_position(snapshot, stored, file),
    )? && let Some(original) = ty.strip_prefix("typeof ")
    {
        return symbol_at(store, snapshot, stored, caches, file, original, depth - 1);
    }
    if let Some((file, original)) =
        named_import(store, snapshot, stored, caches, file, name, false)?
    {
        let original = exported_value_name(store, caches, file, &original)?;
        return symbol_at(store, snapshot, stored, caches, file, &original, depth - 1);
    }
    Ok(None)
}

/// Interpret the collector's type evidence, not arbitrary TypeScript types.
/// Factory returns belong to the factory's module; Awaited unwraps Promise
/// only when the original binding records an await. A local factory shadow
/// prevents following a same-named imported function.
fn materialize_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    binding: &Binding,
) -> crate::EngineResult<Option<Binding>> {
    let (awaited, ty) = binding
        .ty
        .strip_prefix("Awaited<")
        .and_then(|ty| ty.strip_suffix('>'))
        .map_or((false, binding.ty.as_str()), |ty| (true, ty));
    let mut result = if let Some(factory) = ty
        .strip_prefix("ReturnType<typeof ")
        .and_then(|ty| ty.strip_suffix('>'))
    {
        if binding.file_id == stored.file_id
            && declared(
                store,
                caches,
                &stored.reference.owner_id,
                factory,
                (stored.reference.line, stored.reference.column),
            )?
            .is_some()
        {
            return Ok(None);
        }
        let Some(function) =
            symbol_at(store, snapshot, stored, caches, binding.file_id, factory, 8)?
        else {
            return Ok(None);
        };
        let Some(ty) = declared(
            store,
            caches,
            &function.entity_id,
            "return",
            (usize::MAX, usize::MAX),
        )?
        else {
            return Ok(None);
        };
        Binding {
            ty,
            file_id: function.file_id,
        }
    } else {
        Binding {
            ty: ty.to_owned(),
            file_id: binding.file_id,
        }
    };
    if stored.language == "go"
        && let Some(constraint) = declared(
            store,
            caches,
            &stored.reference.owner_id,
            &result.ty,
            (stored.reference.line, stored.reference.column),
        )?
    {
        result.ty = constraint;
    }
    if awaited {
        result.ty = result
            .ty
            .strip_prefix("Promise<")
            .and_then(|ty| ty.strip_suffix('>'))
            .unwrap_or(&result.ty)
            .to_owned();
    }
    Ok(Some(result))
}

/// The nearest lexical evidence wins, including an unknown binding. A receiver
/// root blocks looking up a same-named module, but can still use type resolution.
enum ValueLookup {
    Declaration(super::DeclaredBinding, String),
    Definition(NameIndexEntry),
    Receiver,
}

type Position = (usize, usize);
const END_OF_SCOPE: Position = (usize::MAX, usize::MAX);

// File initializers run in source order. Deferred bodies retain the module
// binding view; copied values are followed at their declaration position.
fn file_position(snapshot: &ResolutionSnapshot, stored: &StoredPendingRef) -> Position {
    let immediate = stored.reference.owner_id == format!("f{}", stored.file_id)
        || snapshot
            .entity_record(&stored.reference.owner_id)
            .is_some_and(|owner| {
                snapshot
                    .lookup_candidates(&owner.name)
                    .unwrap_or_default()
                    .iter()
                    .any(|hit| {
                        hit.entity_id == owner.entity_id
                            && hit.symbol_type == Some(SymbolType::Value)
                            && !hit.qualified_name.contains("::")
                    })
            });
    if immediate {
        (stored.reference.line, stored.reference.column)
    } else {
        END_OF_SCOPE
    }
}

fn module_position(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    file: u32,
) -> Position {
    if file == stored.file_id {
        file_position(snapshot, stored)
    } else {
        END_OF_SCOPE
    }
}

fn lexical_owners(snapshot: &ResolutionSnapshot, stored: &StoredPendingRef) -> Vec<String> {
    let mut owners = vec![stored.reference.owner_id.clone()];
    if let Some(owner) = snapshot.entity_record(&stored.reference.owner_id) {
        let mut scope = owner.qualified_name.as_str();
        while let Some((parent, _)) = scope.rsplit_once("::") {
            let name = parent.rsplit("::").next().unwrap_or(parent);
            owners.extend(
                snapshot
                    .lookup_candidates(name)
                    .unwrap_or_default()
                    .iter()
                    .filter(|hit| hit.file_id == stored.file_id && hit.qualified_name == parent)
                    .map(|hit| hit.entity_id.clone()),
            );
            scope = parent;
        }
    }
    let file = format!("f{}", stored.file_id);
    if owners.last() != Some(&file) {
        owners.push(file);
    }
    owners
}

fn scoped_definition(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    owner: &str,
    name: &str,
) -> Option<NameIndexEntry> {
    let scope = snapshot
        .entity_record(owner)
        .map(|entry| entry.qualified_name.as_str());
    let qualified = scope.map_or_else(|| name.to_owned(), |scope| format!("{scope}::{name}"));
    let mut hits = snapshot.lookup_candidates(name)?.iter().filter(|hit| {
        hit.file_id == stored.file_id
            && hit.qualified_name == qualified
            && hit.symbol_type != Some(SymbolType::Value)
            && shadows_import(snapshot, hit)
    });
    let first = hits.next()?;
    hits.next().is_none().then(|| first.clone())
}

fn lookup_value(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    name: &str,
) -> crate::EngineResult<Option<ValueLookup>> {
    for owner in lexical_owners(snapshot, stored) {
        let at = if owner == format!("f{}", stored.file_id) {
            file_position(snapshot, stored)
        } else {
            (stored.reference.line, stored.reference.column)
        };
        if let Some(entry) = declaration_at(store, caches, &owner, name, at)? {
            return Ok(Some(ValueLookup::Declaration(entry, owner)));
        }
        if let Some(hit) = scoped_definition(snapshot, stored, &owner, name) {
            return Ok(Some(ValueLookup::Definition(hit)));
        }
        if let Some((root, _)) = name.split_once('.')
            && (declaration_at(store, caches, &owner, root, at)?.is_some()
                || scoped_definition(snapshot, stored, &owner, root).is_some())
        {
            return Ok(Some(ValueLookup::Receiver));
        }
    }
    Ok(None)
}

fn before(entry: &super::DeclaredBinding) -> Position {
    let line = entry.line as usize;
    let column = entry.column as usize;
    if column == 0 {
        (line.saturating_sub(1), usize::MAX)
    } else {
        (line, column - 1)
    }
}

fn follow_value(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    value: ValueLookup,
    depth: usize,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    let ValueLookup::Declaration(entry, owner) = value else {
        return Ok(match value {
            ValueLookup::Definition(hit) => Some(hit),
            _ => None,
        });
    };
    let (live, ty) = entry
        .bound_type
        .strip_prefix("export ")
        .map_or((false, entry.bound_type.as_str()), |ty| (true, ty));
    let Some(name) = ty.strip_prefix("typeof ") else {
        return Ok(None);
    };
    let mut context = stored.clone();
    context.reference.owner_id = owner;
    if !live {
        (context.reference.line, context.reference.column) = before(&entry);
    }
    // Object methods refer to their own emitted definition, not another value
    // slot. Property overwrites have already selected the applicable entry.
    if name.contains("::") {
        return symbol_at(
            store,
            snapshot,
            &context,
            caches,
            context.file_id,
            name,
            depth - 1,
        );
    }
    if let Some(value) = lookup_value(store, snapshot, &context, caches, name)? {
        return follow_value(store, snapshot, &context, caches, value, depth - 1);
    }
    imported_value(store, snapshot, &context, caches, name, depth - 1, false)
}

fn exported_target(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
    depth: usize,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    let mut context = stored.clone();
    context.file_id = file;
    context.reference.owner_id = format!("f{file}");
    (context.reference.line, context.reference.column) = END_OF_SCOPE;
    let exported = declaration_at(
        store,
        caches,
        &context.reference.owner_id,
        &format!("[[export:{name}]]"),
        END_OF_SCOPE,
    )?;
    let value = if let Some(entry) = exported {
        Some(ValueLookup::Declaration(
            entry,
            context.reference.owner_id.clone(),
        ))
    } else {
        lookup_value(store, snapshot, &context, caches, name)?
    };
    match value {
        Some(value) => follow_value(store, snapshot, &context, caches, value, depth - 1),
        None => imported_value(store, snapshot, &context, caches, name, depth - 1, true),
    }
}

fn imported_value(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    name: &str,
    depth: usize,
    include_reexports: bool,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    let (local, member) = name
        .split_once('.')
        .map_or((name, None), |(local, member)| (local, Some(member)));
    let Some((file, original)) = named_import(
        store,
        snapshot,
        stored,
        caches,
        stored.file_id,
        local,
        include_reexports,
    )?
    else {
        return Ok(None);
    };
    if let Some(member) = member {
        let name = if original == "*" {
            member.to_owned()
        } else {
            let container = exported_value_name(store, caches, file, &original)?;
            format!("{container}.{member}")
        };
        return exported_target(store, snapshot, stored, caches, file, &name, depth - 1);
    }
    exported_target(store, snapshot, stored, caches, file, &original, depth - 1)
}

/// Lexical declarations and copied values are resolved before module imports.
pub(super) fn resolve_call(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    let name = stored.reference.receiver_name.as_ref().map_or_else(
        || stored.name_tail.clone(),
        |receiver| format!("{receiver}.{}", stored.name_tail),
    );
    let value = lookup_value(store, snapshot, stored, caches, &name)?;
    if matches!(value, Some(ValueLookup::Receiver)) {
        // Skip namespace-import fallbacks when a lexical receiver shadows them.
        return super::classify_names(store, snapshot, stored, caches).map(Some);
    }
    let authoritative = value.is_some();
    let target = match value {
        Some(value) => follow_value(store, snapshot, stored, caches, value, 16)?,
        None => imported_value(store, snapshot, stored, caches, &name, 16, false)?,
    };
    if target.is_none() && !authoritative {
        return Ok(None);
    }
    Ok(Some(target.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: member_resolution(stored, &hit),
            rule: Rule::ImportBound,
        }
    })))
}

fn exported_value_name(
    store: &IndexStore,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
) -> crate::EngineResult<String> {
    let mut name = name.to_owned();
    if let Some(ty) = declared(
        store,
        caches,
        &format!("f{file}"),
        &format!("[[export:{name}]]"),
        END_OF_SCOPE,
    )? && let Some(original) = ty
        .strip_prefix("export ")
        .unwrap_or(&ty)
        .strip_prefix("typeof ")
    {
        original.clone_into(&mut name);
    }
    for _ in 0..8 {
        let Some(ty) = declared(
            store,
            caches,
            &format!("f{file}"),
            &name,
            (usize::MAX, usize::MAX),
        )?
        else {
            break;
        };
        let Some(original) = ty.strip_prefix("typeof ") else {
            break;
        };
        if original == name {
            break;
        }
        original.clone_into(&mut name);
    }
    Ok(name)
}

/// The workspace module prefix anchors Go imports; external packages stay external.
fn go_module(root: &std::path::Path) -> Option<String> {
    let source = std::fs::read_to_string(root.join("go.mod")).ok()?;
    source.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "module")
            .then(|| words.next().map(|name| name.trim_matches('"').to_owned()))
            .flatten()
    })
}

/// Resolve an import qualifier inside the nearest Go module. Explicit aliases
/// override conventional package names (including /v2 suffix handling). A
/// matching qualifier outside this module does not fall back to a local name.
fn go_import_directory(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
    qualifier: &str,
) -> crate::EngineResult<Option<String>> {
    let imports = caches.imports(store, file)?;
    let Some((directory, module)) = enclosing_go_module(snapshot, caches, file) else {
        return Ok(None);
    };
    for import in imports.iter() {
        let Some(raw) = import.raw_text.as_deref() else {
            continue;
        };
        let prefix = raw.split(['"', '`']).next().unwrap_or_default().trim();
        let alias = prefix
            .split_whitespace()
            .next_back()
            .filter(|word| *word != "import")
            .unwrap_or_else(|| go_package_qualifier(&import.specifier));
        if alias != qualifier {
            continue;
        }
        if import.specifier == module {
            return Ok(Some(directory));
        }
        return Ok(import
            .specifier
            .strip_prefix(&module)
            .and_then(|path| path.strip_prefix('/'))
            .map(|path| {
                if directory.is_empty() {
                    path.to_owned()
                } else {
                    format!("{directory}/{path}")
                }
            }));
    }
    Ok(None)
}

// Restrict inference to the importing file's nearest module. A sibling module
// requires go.work/replace resolution, which this static rule does not infer.
fn enclosing_go_module(
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    file: u32,
) -> Option<(String, String)> {
    let root = caches.source_root.as_ref()?;
    let mut directory = importing_dir(snapshot.file_path(file)?);
    loop {
        let module = caches
            .go_modules
            .entry(directory.to_owned())
            .or_insert_with(|| go_module(&root.join(directory)));
        if let Some(module) = module {
            return Some((directory.to_owned(), module.clone()));
        }
        if directory.is_empty() {
            return None;
        }
        directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

fn go_package_qualifier(specifier: &str) -> &str {
    let (parent, tail) = specifier.rsplit_once('/').unwrap_or(("", specifier));
    if tail
        .strip_prefix('v')
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 2)
    {
        parent.rsplit('/').next().unwrap_or(parent)
    } else {
        tail
    }
}
