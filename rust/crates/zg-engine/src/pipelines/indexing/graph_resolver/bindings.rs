//! Receiver bindings retain their declaring file while walking fields and scopes.
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
    Ok(bindings
        .iter()
        .rev()
        .find(|binding| {
            binding.name == name
                && usize::try_from(binding.line)
                    .is_ok_and(|at| (at, binding.column as usize) <= position)
                && binding.scope_end.is_none_or(|end| position < end)
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
    hits: &[NameIndexEntry],
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
    if let Some((local, value)) = raw.split_once('=')
        && local.trim() == name
        && value.trim_start().starts_with("require(")
    {
        return Some("*".to_owned());
    }
    if let Some(rest) = raw.trim_start().strip_prefix("import * as ")
        && rest.split_whitespace().next() == Some(name)
    {
        return Some("*".to_owned());
    }
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
        let original = exported_value_name(store, caches, file, &original)?;
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
    imported_value(store, snapshot, &context, caches, name, depth - 1)
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
        None => imported_value(store, snapshot, &context, caches, name, depth - 1),
    }
}

fn imported_value(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
    name: &str,
    depth: usize,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    if depth == 0 {
        return Ok(None);
    }
    let (local, member) = name
        .split_once('.')
        .map_or((name, None), |(local, member)| (local, Some(member)));
    let Some((file, original)) =
        named_import(store, snapshot, stored, caches, stored.file_id, local)?
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
        None => imported_value(store, snapshot, stored, caches, &name, 16)?,
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
