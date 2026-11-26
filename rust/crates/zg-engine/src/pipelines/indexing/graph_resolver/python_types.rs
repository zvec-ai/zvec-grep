//! Locate a declared Python receiver type through its importing module.

use super::{
    ClassScope, IndexStore, NameIndexEntry, Outcome, ResolutionSnapshot, Rule, RuleCaches,
    StoredPendingRef, SymbolType, binding_holds_at_ref, import_target_file, is_type_scope,
    without_line_comments,
};
use crate::domain::{EdgeProvenance, EntityId, Range};
use crate::storage::graph::{ImportReference, Resolution};

type Position = (usize, usize);

/// In Python the same call syntax can invoke a function or instantiate a class.
/// Extraction marks imports that are visible past the call owner's local bindings.
pub(super) fn imported_call(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if stored.language != "python" || !stored.reference.metadata.contains_key("python_import_call")
    {
        return Ok(None);
    }
    let Some(name) = stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let owner = snapshot.entity_record(&stored.reference.owner_id);
    let site = LookupSite {
        scope: owner.map(|entity| entity.qualified_name.as_str()),
        position: (stored.reference.line, stored.reference.column),
        eager: owner.is_none_or(|entity| {
            definition_in_file(
                snapshot,
                stored.file_id,
                &entity.qualified_name,
                LookupKind::Type,
            )
            .is_some()
        }),
        kind: LookupKind::Callable,
    };
    let (hit, imported) = lookup(store, snapshot, stored.file_id, name, &site, caches)?;
    if !imported && hit.is_none() {
        return Ok(None);
    }
    if !imported && let Some(hit) = hit {
        let candidates = snapshot
            .lookup_candidates(&stored.name_tail)
            .unwrap_or_default();
        let local = candidates.iter().filter(|other| {
            other.file_id == hit.file_id
                && other.qualified_name == hit.qualified_name
                && site.kind.accepts(other)
        });
        // Keep competing local definitions as candidates, as the ordinary
        // bare-call resolver does when no import wins.
        if local.clone().nth(1).is_some() {
            return Ok(Some(Outcome::Ambiguous(
                local.map(|candidate| candidate.entity_id.clone()).collect(),
            )));
        }
    }
    // A visible explicit import owns the call even if its export is missing.
    // Keep it pending so the defining file can supply the target in a later sweep.
    Ok(Some(hit.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: Resolution {
                ref_id: stored.id,
                target_id: hit.entity_id.clone(),
                provenance: if imported {
                    EdgeProvenance::ImportBound
                } else {
                    EdgeProvenance::FileScoped
                },
            },
            rule: if imported {
                Rule::ImportBound
            } else {
                Rule::Unique
            },
        }
    })))
}

/// The resolved type and whether its identity depends on an import path.
pub(super) fn scope(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    ty: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<(Option<ClassScope>, bool)> {
    let imports = caches.imports(store, stored.file_id)?;
    let site = lookup_site(snapshot, stored, ty, caches, &imports);
    let (hit, imported) = lookup(store, snapshot, stored.file_id, ty, &site, caches)?;
    Ok((hit.map(class_scope), imported))
}

fn lookup<'s>(
    store: &IndexStore,
    snapshot: &'s ResolutionSnapshot,
    file: u32,
    name: &str,
    site: &LookupSite<'_>,
    caches: &mut RuleCaches,
) -> crate::EngineResult<(Option<&'s NameIndexEntry>, bool)> {
    let Some(path) = snapshot.file_path(file) else {
        return Ok((None, false));
    };
    let imports = caches.imports(store, file)?;
    match visible_binding(store, snapshot, file, name, site, &imports, caches)? {
        Some(Binding::Local(definition)) => Ok((
            local_definition(snapshot, definition, name, site.kind),
            false,
        )),
        Some(Binding::Import(import, target)) => Ok((
            imported_definition(store, snapshot, path, import, &target, site.kind, caches)?,
            true,
        )),
        None => Ok((None, false)),
    }
}

fn class_scope(hit: &NameIndexEntry) -> ClassScope {
    ClassScope {
        name: hit.qualified_name.clone(),
        declared_in: Some(hit.file_id),
        entity: Some(hit.entity_id.clone()),
    }
}

enum Binding<'s, 'i> {
    Local(&'s NameIndexEntry),
    Import(&'i ImportReference, String),
}

fn visible_binding<'s, 'i>(
    store: &IndexStore,
    snapshot: &'s ResolutionSnapshot,
    file: u32,
    ty: &str,
    site: &LookupSite<'_>,
    imports: &'i [ImportReference],
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Binding<'s, 'i>>> {
    let file_scope = format!("f{file}");
    let mut best = imports
        .iter()
        .filter_map(|import| {
            let scope = if import.owner_id == file_scope {
                None
            } else {
                Some(
                    snapshot
                        .entity_record(&import.owner_id)?
                        .qualified_name
                        .as_str(),
                )
            };
            let depth = site.depth(snapshot, file, scope)?;
            let position = (import.position.0 as usize, import.position.1 as usize);
            if !site.can_see(depth, position) {
                return None;
            }
            let name = imported_name(import.raw_text.as_deref()?, &import.specifier, ty)?;
            Some(((depth, position), Binding::Import(import, name)))
        })
        .max_by_key(|(key, _)| *key);

    let root = ty.split('.').next().unwrap_or(ty);
    let Some(classes) = snapshot.lookup_candidates(root) else {
        return Ok(best.map(|(_, binding)| binding));
    };
    for class in classes
        .iter()
        .filter(|hit| hit.file_id == file && site.kind.accepts(hit))
    {
        let scope = class
            .qualified_name
            .rsplit_once("::")
            .map(|(scope, _)| scope);
        // Skip unrelated lexical scopes before loading a declaration position.
        let Some(depth) = site.depth(snapshot, file, scope) else {
            continue;
        };
        let Some(position) = definition_position(store, caches, &class.entity_id)? else {
            continue;
        };
        if site.can_see(depth, position)
            && best
                .as_ref()
                .is_none_or(|(previous, _)| (depth, position) > *previous)
        {
            best = Some(((depth, position), Binding::Local(class)));
        }
    }
    Ok(best.map(|(_, binding)| binding))
}

fn local_definition<'a>(
    snapshot: &'a ResolutionSnapshot,
    class: &'a NameIndexEntry,
    ty: &str,
    kind: LookupKind,
) -> Option<&'a NameIndexEntry> {
    match ty.split_once('.') {
        Some((_, suffix)) => definition_in_file(
            snapshot,
            class.file_id,
            &format!("{}::{}", class.qualified_name, suffix.replace('.', "::")),
            kind,
        ),
        None => Some(class),
    }
}

/// Follow explicit package exports. Only a from-list can implicitly load its
/// named child module; plain attribute access does not import a child.
fn imported_definition<'a>(
    store: &IndexStore,
    snapshot: &'a ResolutionSnapshot,
    path: &str,
    import: &ImportReference,
    name: &str,
    kind: LookupKind,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<&'a NameIndexEntry>> {
    let mut importer = path;
    let mut specifier = import.specifier.clone();
    let mut name = name.to_owned();
    let mut from_list = is_from_import(import);
    let mut seen = std::collections::HashSet::new();
    let mut export_position = (usize::MAX, usize::MAX);
    // Bound long forwarding chains, including cycles that grow a dotted name.
    for _ in 0..16 {
        let target = import_target_file(snapshot, "python", importer, &specifier);
        if let Some(file) = target {
            let Some(target_path) = snapshot.file_path(file) else {
                return Ok(None);
            };
            if !std::path::Path::new(target_path).ends_with("__init__.py") {
                return Ok(definition_in_file(
                    snapshot,
                    file,
                    &name.replace('.', "::"),
                    kind,
                ));
            }
            if !seen.insert((file, name.clone(), export_position)) {
                return Ok(None);
            }
            let imports = caches.imports(store, file)?;
            let module = LookupSite {
                scope: None,
                position: export_position,
                eager: true,
                kind,
            };
            match visible_binding(store, snapshot, file, &name, &module, &imports, caches)? {
                Some(Binding::Local(class)) => {
                    return Ok(local_definition(snapshot, class, &name, kind));
                }
                Some(Binding::Import(next, forwarded)) => {
                    let self_import =
                        import_target_file(snapshot, "python", target_path, &next.specifier)
                            == Some(file);
                    importer = target_path;
                    specifier.clone_from(&next.specifier);
                    name = forwarded;
                    from_list = is_from_import(next);
                    // A same-package alias captures the binding before this import.
                    // If none exists there, the from-list may load the named child.
                    export_position = if self_import {
                        let (line, column) = next.position;
                        if column == 0 {
                            (line.saturating_sub(1) as usize, usize::MAX)
                        } else {
                            (line as usize, column as usize - 1)
                        }
                    } else {
                        (usize::MAX, usize::MAX)
                    };
                    continue;
                }
                None => {}
            }
        } else if !specifier.starts_with('.')
            && let Some(hit) = definition_under_source_root(snapshot, &specifier, &name, kind)
        {
            return Ok(Some(hit));
        }
        if !from_list {
            return Ok(None);
        }
        let Some((child, rest)) = name.split_once('.') else {
            return Ok(None);
        };
        if !specifier.ends_with('.') {
            specifier.push('.');
        }
        specifier.push_str(child);
        name = rest.to_owned();
        export_position = (usize::MAX, usize::MAX);
        from_list = false;
    }
    Ok(None)
}

/// A workspace may put its Python sources under an extra import root. When the
/// root is unknown, both the declared name and its module path must still match.
fn definition_under_source_root<'a>(
    snapshot: &'a ResolutionSnapshot,
    specifier: &str,
    name: &str,
    kind: LookupKind,
) -> Option<&'a NameIndexEntry> {
    let module = specifier.replace('.', "/");
    let file = format!("{module}.py");
    let init = format!("{module}/__init__.py");
    let qualified = name.replace('.', "::");
    let mut hits = snapshot
        .lookup_candidates(name.rsplit('.').next()?)?
        .iter()
        .filter(|hit| {
            hit.qualified_name == qualified
                && kind.accepts(hit)
                && snapshot.file_path(hit.file_id).is_some_and(|path| {
                    let path = std::path::Path::new(path);
                    path.ends_with(&file) || path.ends_with(&init)
                })
        });
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit)
}

fn is_from_import(import: &ImportReference) -> bool {
    import.raw_text.as_deref().is_some_and(|text| {
        text.trim_start()
            .strip_prefix("from")
            .is_some_and(|tail| tail.starts_with(char::is_whitespace) || tail.starts_with('\\'))
    })
}

#[derive(Clone, Copy)]
enum LookupKind {
    Type,
    Callable,
}

impl LookupKind {
    fn accepts(self, hit: &NameIndexEntry) -> bool {
        hit.symbol_type.is_some_and(|ty| {
            is_type_scope(ty) || (matches!(self, Self::Callable) && ty == SymbolType::Function)
        })
    }
}

struct LookupSite<'a> {
    kind: LookupKind,
    scope: Option<&'a str>,
    position: Position,
    eager: bool,
}

fn lookup_site<'a>(
    snapshot: &'a ResolutionSnapshot,
    stored: &StoredPendingRef,
    ty: &str,
    caches: &RuleCaches,
    imports: &[ImportReference],
) -> LookupSite<'a> {
    let binding = caches
        .declared_bindings
        .get(&stored.reference.owner_id)
        .and_then(|bindings| {
            bindings.iter().rev().find(|binding| {
                Some(binding.name.as_str()) == stored.reference.receiver_name.as_deref()
                    && binding.bound_type == ty
                    && binding_holds_at_ref(binding, stored)
            })
        });
    let site = binding.map_or(
        (stored.reference.line, stored.reference.column),
        |binding| {
            binding
                .type_origin
                .unwrap_or((binding.line as usize, binding.column as usize))
        },
    );
    // Constructors in function bodies and postponed annotations can use module
    // names bound after the function was defined. Function-local imports still
    // have to precede the site, including for annotations on nested functions.
    let annotation = binding.and_then(|binding| binding.raw_text.as_deref());
    let eager = annotation.is_some_and(|text| !text.starts_with(['\'', '"']))
        && !imports.iter().any(|import| {
            import.specifier == "__future__"
                && import
                    .raw_text
                    .as_deref()
                    .is_some_and(|text| imported_name(text, "__future__", "annotations").is_some())
        });
    let owner = snapshot
        .entity_record(&stored.reference.owner_id)
        .map(|owner| owner.qualified_name.as_str());
    let scope = if annotation.is_some() {
        owner.and_then(|name| name.rsplit_once("::").map(|(parent, _)| parent))
    } else {
        owner
    };
    LookupSite {
        kind: LookupKind::Type,
        scope,
        position: site,
        eager,
    }
}

impl LookupSite<'_> {
    /// Class bodies supply names to their direct method signatures, but do
    /// not form enclosing function scopes for code nested inside a method.
    fn depth(
        &self,
        snapshot: &ResolutionSnapshot,
        file: u32,
        scope: Option<&str>,
    ) -> Option<usize> {
        let Some(scope) = scope else { return Some(0) };
        let suffix = self.scope?.strip_prefix(scope)?;
        if !suffix.is_empty()
            && (!suffix.starts_with("::")
                || definition_in_file(snapshot, file, scope, LookupKind::Type).is_some())
        {
            return None;
        }
        Some(scope.split("::").count())
    }

    fn can_see(&self, depth: usize, position: Position) -> bool {
        position <= self.position || (depth == 0 && !self.eager)
    }
}

fn definition_in_file<'a>(
    snapshot: &'a ResolutionSnapshot,
    file: u32,
    qualified: &str,
    kind: LookupKind,
) -> Option<&'a NameIndexEntry> {
    let mut hits = snapshot
        .lookup_candidates(qualified.rsplit("::").next()?)?
        .iter()
        .filter(|hit| hit.file_id == file && hit.qualified_name == qualified && kind.accepts(hit));
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit)
}

/// Definition positions already live in canonical entities. Read each relevant
/// local declaration once per sweep and retain only its source coordinates.
fn definition_position(
    store: &IndexStore,
    caches: &mut RuleCaches,
    id: &str,
) -> crate::EngineResult<Option<Position>> {
    if let Some(position) = caches.python_definition_positions.get(id) {
        return Ok(*position);
    }
    let entity_id = EntityId::from_string(id.to_owned());
    let position = store.read_snapshot(|view| {
        Ok(view
            .fetch_entities(std::slice::from_ref(&entity_id))?
            .get(&entity_id)
            .and_then(|entity| match entity.source_range() {
                // A class or function statement binds its name when the definition finishes.
                Range::Text(range) => Some((range.end_line(), range.end_byte_column())),
                _ => None,
            }))
    })?;
    caches
        .python_definition_positions
        .insert(id.to_owned(), position);
    Ok(position)
}

/// Read the spelling inside the module named by this import reference.
/// The stored statement can contain several imports, so match the module as
/// well as its local alias before following a namespace-qualified annotation.
fn imported_name(statement: &str, specifier: &str, ty: &str) -> Option<String> {
    let statement = without_line_comments(statement, "#");
    let normalized;
    let statement = if statement.contains(['\\', '\n', '\r', '\t']) || statement.contains("  ") {
        normalized = statement
            .replace("\\\r\n", " ")
            .replace("\\\n", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        normalized.as_str()
    } else {
        statement.trim()
    };
    if let Some(rest) = statement.strip_prefix("from ") {
        let (module, names) = rest.split_once(" import ")?;
        if module.trim() != specifier {
            return None;
        }
        for entry in names.trim().trim_matches(['(', ')']).rsplit(',') {
            let (name, local) = import_entry(entry);
            if let Some(suffix) = type_suffix(ty, local) {
                return Some(format!("{name}{suffix}"));
            }
        }
    } else if let Some(names) = statement.strip_prefix("import ") {
        for entry in names.split(',') {
            let (module, local) = import_entry(entry);
            if module != specifier {
                continue;
            }
            // Plain `import a.b` is used as `a.b.Type`; an alias is used as
            // `alias.Type`. The suffix must actually name a type.
            if let Some(name) = type_suffix(ty, local)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .filter(|name| !name.is_empty())
            {
                return Some(name.to_owned());
            }
        }
    }
    None
}

fn import_entry(entry: &str) -> (&str, &str) {
    let entry = entry.trim();
    entry
        .split_once(" as ")
        .map_or((entry, entry), |(name, local)| (name.trim(), local.trim()))
}

fn type_suffix<'a>(ty: &'a str, local: &str) -> Option<&'a str> {
    ty.strip_prefix(local)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with('.'))
}
