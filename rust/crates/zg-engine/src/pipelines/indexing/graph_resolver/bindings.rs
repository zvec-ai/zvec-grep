//! Receiver bindings retain their declaring file while walking fields and scopes.
use super::{
    AliasBinding, ClassScope, Entry, HashSet, IndexStore, NameIndexEntry, Outcome, Resolution,
    ResolutionSnapshot, Rule, RuleCaches, StoredPendingRef, SymbolType, alias_target_file,
    bind_hierarchy_member, collect_base_scopes, declares_in, importing_dir, is_type_scope,
    member_resolution, shadows_import, visible_scopes,
};

#[derive(Clone)]
struct Binding {
    ty: String,
    file_id: u32,
}

fn declared(
    store: &IndexStore,
    caches: &mut RuleCaches,
    owner: &str,
    name: &str,
    position: (usize, usize),
) -> crate::EngineResult<Option<String>> {
    let bindings = match caches.declared_bindings.entry(owner.to_owned()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(store.declared_bindings(owner)?),
    };
    Ok(bindings
        .iter()
        .rev()
        .find(|binding| {
            binding.name == name
                && usize::try_from(binding.line)
                    .is_ok_and(|at| (at, binding.column as usize) <= position)
                && binding.scope_end.is_none_or(|end| position < end)
        })
        .map(|binding| binding.bound_type.clone()))
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
        (usize::MAX, usize::MAX),
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
            (usize::MAX, usize::MAX),
        )? {
            return Ok(Some(Binding {
                ty,
                file_id: variable.file_id,
            }));
        }
    }
    if let Some((file, original)) =
        named_import(store, snapshot, stored, caches, stored.file_id, name)?
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
                    (stored.reference.line, stored.reference.column),
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
        return script_type(store, snapshot, stored, caches, binding.file_id, name);
    }
    if stored.language == "go"
        && let Some((package, short)) = name.rsplit_once('.')
    {
        let Some(directory) = go_import_directory(store, caches, binding.file_id, package)? else {
            return Ok(None);
        };
        let candidates: Vec<_> = snapshot
            .lookup_candidates(short)
            .unwrap_or_default()
            .iter()
            .filter(|hit| {
                hit.symbol_type.is_some_and(is_type_scope)
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
            hit.symbol_type.is_some_and(is_type_scope)
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
    hits: &[&NameIndexEntry],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Resolution>> {
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
        && let Some(directory) = go_import_directory(store, caches, stored.file_id, receiver)?
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
        let Some(binding) = lexical_binding(store, snapshot, stored, caches, root)? else {
            return Ok(None);
        };
        binding
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
    hits: &[&NameIndexEntry],
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
    let root = ClassScope::at(snapshot, owner.qualified_name.clone(), Some(owner.file_id));
    let mut scopes = vec![root.clone()];
    let branched =
        collect_base_scopes(store, snapshot, &root, &mut scopes, &mut HashSet::new(), 8)?;
    let hierarchy_hits: Vec<_> = hits
        .iter()
        .filter(|hit| {
            hit.file_id == owner.file_id
                || hit
                    .qualified_name
                    .rsplit_once("::")
                    .is_none_or(|(scope, _)| scope != owner.qualified_name)
        })
        .copied()
        .collect();
    Ok(bind_hierarchy_member(
        snapshot,
        stored,
        &hierarchy_hits,
        Some(&root),
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
) -> crate::EngineResult<Option<(u32, String)>> {
    for import in store.import_references(file)? {
        let Some(raw) = import.raw_text.as_deref() else {
            continue;
        };
        let Some(original) = imported_name(raw, name) else {
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

fn imported_name(raw: &str, name: &str) -> Option<String> {
    if let Some(rest) = raw.trim_start().strip_prefix("import ") {
        let local = rest.split(|c: char| c.is_whitespace() || c == ',').next()?;
        if local == name && (rest.contains(" from ") || rest.contains("= require(")) {
            return Some("default".to_owned());
        }
    }
    let (_, entries) = raw.split_once('{')?;
    let (entries, _) = entries.split_once('}')?;
    entries.split(',').find_map(|entry| {
        let entry = entry.trim().strip_prefix("type ").unwrap_or(entry.trim());
        let (original, local) = entry.split_once(" as ").unwrap_or((entry, entry));
        (local.trim() == name).then(|| original.trim().to_owned())
    })
}

fn go_symbol_at(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &RuleCaches,
    file: u32,
    package: &str,
    short: &str,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let Some(directory) = go_import_directory(store, caches, file, package)? else {
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
            (usize::MAX, usize::MAX),
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
            (usize::MAX, usize::MAX),
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
            named_import(store, snapshot, stored, caches, file, container)?
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
        (usize::MAX, usize::MAX),
    )? && let Some(original) = ty.strip_prefix("typeof ")
    {
        return symbol_at(store, snapshot, stored, caches, file, original, depth - 1);
    }
    if let Some((file, original)) = named_import(store, snapshot, stored, caches, file, name)? {
        return symbol_at(store, snapshot, stored, caches, file, &original, depth - 1);
    }
    Ok(None)
}

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

fn callable_target(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    binding: &Binding,
    depth: usize,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    let Some(name) = binding.ty.strip_prefix("typeof ") else {
        return Ok(None);
    };
    let Some(target) = symbol_at(
        store,
        snapshot,
        stored,
        caches,
        binding.file_id,
        name,
        depth,
    )?
    else {
        return Ok(None);
    };
    if target
        .symbol_type
        .is_some_and(|kind| kind == SymbolType::Function || is_type_scope(kind))
    {
        return Ok(Some(target));
    }
    let name = target.qualified_name.rsplit("::").next().unwrap_or(name);
    let ty = declared(
        store,
        caches,
        &target.entity_id,
        name,
        (usize::MAX, usize::MAX),
    )?;
    if let Some(ty) = ty {
        return callable_target(
            store,
            snapshot,
            stored,
            caches,
            &Binding {
                ty,
                file_id: target.file_id,
            },
            depth - 1,
        );
    }
    Ok(None)
}

/// A value binding is authoritative even when it cannot identify a callable.
/// In particular a later object property must not fall back to an older method.
pub(super) fn resolve_call(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|text| text.starts_with('<'))
        && stored.reference.receiver_name.is_none()
    {
        return resolve_component(store, snapshot, stored, caches);
    }
    let name = stored.reference.receiver_name.as_ref().map_or_else(
        || stored.name_tail.clone(),
        |receiver| format!("{receiver}.{}", stored.name_tail),
    );
    let mut imported_only = false;
    let mut file = stored.file_id;
    let mut ty = declared(
        store,
        caches,
        &stored.reference.owner_id,
        &name,
        (stored.reference.line, stored.reference.column),
    )?;
    if let Some(local) = ty.as_mut() {
        *local = local_alias_type(store, caches, stored, local)?;
    }
    if ty.is_none()
        && let Some(receiver) = stored.reference.receiver_name.as_deref()
        && let Some(root) = receiver.split('.').next()
        && declared(
            store,
            caches,
            &stored.reference.owner_id,
            root,
            (stored.reference.line, stored.reference.column),
        )?
        .is_some()
    {
        return Ok(None);
    }
    if ty.is_none() {
        ty = declared(
            store,
            caches,
            &format!("f{file}"),
            &name,
            (usize::MAX, usize::MAX),
        )?;
    }
    if ty.is_none()
        && let Some((binding, inferred)) = imported_member_binding(store, snapshot, stored, caches)?
    {
        file = binding.file_id;
        ty = Some(binding.ty);
        imported_only = inferred;
    }

    if ty.is_none()
        && stored.reference.receiver_name.is_none()
        && let Some((origin, original)) =
            named_import(store, snapshot, stored, caches, file, &name)?
    {
        file = origin;
        ty = declared(
            store,
            caches,
            &format!("f{file}"),
            &original,
            (usize::MAX, usize::MAX),
        )?;
        if ty.is_none() {
            ty = Some(format!("typeof {original}"));
            imported_only = true;
        }
    }
    let Some(ty) = ty else { return Ok(None) };
    let target = callable_target(
        store,
        snapshot,
        stored,
        caches,
        &Binding { ty, file_id: file },
        8,
    )?;
    if target.is_none() && imported_only {
        return Ok(None);
    }
    Ok(Some(target.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: member_resolution(stored, &hit),
            rule: Rule::ImportBound,
        }
    })))
}

fn local_alias_type(
    store: &IndexStore,
    caches: &mut RuleCaches,
    stored: &StoredPendingRef,
    ty: &str,
) -> crate::EngineResult<String> {
    let mut ty = ty.to_owned();
    for _ in 0..8 {
        let Some(name) = ty
            .strip_prefix("typeof ")
            .filter(|name| !name.contains("::"))
        else {
            break;
        };
        let Some(bound) = declared(
            store,
            caches,
            &stored.reference.owner_id,
            name,
            (stored.reference.line, stored.reference.column),
        )?
        else {
            break;
        };
        if bound == ty {
            return Ok("?".to_owned());
        }
        ty = bound;
    }
    Ok(ty)
}

fn imported_member_binding(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<(Binding, bool)>> {
    let Some(receiver) = stored.reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    let Some((file, original)) =
        named_import(store, snapshot, stored, caches, stored.file_id, receiver)?
    else {
        return Ok(None);
    };
    let original = exported_value_name(store, caches, file, &original)?;
    let ty = declared(
        store,
        caches,
        &format!("f{file}"),
        &format!("{original}.{}", stored.name_tail),
        (usize::MAX, usize::MAX),
    )?;
    let inferred = ty.is_none();
    Ok(Some((
        Binding {
            file_id: file,
            ty: ty.unwrap_or_else(|| format!("typeof {original}::{}", stored.name_tail)),
        },
        inferred,
    )))
}

fn exported_value_name(
    store: &IndexStore,
    caches: &mut RuleCaches,
    file: u32,
    name: &str,
) -> crate::EngineResult<String> {
    let mut name = name.to_owned();
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

fn resolve_component(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if declared(
        store,
        caches,
        &stored.reference.owner_id,
        &stored.name_tail,
        (stored.reference.line, stored.reference.column),
    )?
    .is_some()
    {
        return Ok(Some(Outcome::Deferred));
    }
    let target = symbol_at(
        store,
        snapshot,
        stored,
        caches,
        stored.file_id,
        &stored.name_tail,
        8,
    )?
    .filter(|hit| {
        hit.symbol_type
            .is_some_and(|kind| kind == SymbolType::Function || is_type_scope(kind))
    });
    Ok(target.map(|hit| Outcome::Resolved {
        resolution: member_resolution(stored, &hit),
        rule: Rule::ImportBound,
    }))
}

/// The workspace module prefix anchors Go imports; external packages stay external.
pub(super) fn go_module(root: &std::path::Path) -> Option<String> {
    let source = std::fs::read_to_string(root.join("go.mod")).ok()?;
    source.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "module")
            .then(|| words.next().map(|name| name.trim_matches('"').to_owned()))
            .flatten()
    })
}

fn go_import_directory(
    store: &IndexStore,
    caches: &RuleCaches,
    file: u32,
    qualifier: &str,
) -> crate::EngineResult<Option<String>> {
    let Some(module) = caches.go_module.as_deref() else {
        return Ok(None);
    };
    for import in store.import_references(file)? {
        let Some(raw) = import.raw_text.as_deref() else {
            continue;
        };
        let prefix = raw.split(['"', '`']).next().unwrap_or_default().trim();
        let alias = prefix
            .split_whitespace()
            .next_back()
            .filter(|word| *word != "import")
            .unwrap_or_else(|| import.specifier.rsplit('/').next().unwrap_or_default());
        if alias != qualifier {
            continue;
        }
        if import.specifier == module {
            return Ok(Some(String::new()));
        }
        return Ok(import
            .specifier
            .strip_prefix(module)
            .and_then(|path| path.strip_prefix('/'))
            .map(str::to_owned));
    }
    Ok(None)
}
