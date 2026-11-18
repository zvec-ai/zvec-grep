//! Locate a declared Python receiver type through its importing module.

use super::{
    ClassScope, IndexStore, NameIndexEntry, ResolutionSnapshot, RuleCaches, StoredPendingRef,
    binding_holds_at, import_target_file, is_type_scope, without_line_comments,
};
use crate::storage::graph::ImportReference;

/// The resolved type and whether its identity depends on an import path.
pub(super) fn scope(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    ty: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<(Option<ClassScope>, bool)> {
    let Some(path) = snapshot.file_path(stored.file_id) else {
        return Ok((None, false));
    };
    // Imports in the function body are not visible to an annotation on its
    // signature. A later constructor assignment instead reads imports at the
    // assignment site. The binding cache already holds this receiver evidence.
    let position = caches
        .declared_bindings
        .get(&stored.reference.owner_id)
        .and_then(|bindings| {
            bindings.iter().rev().find(|binding| {
                Some(binding.name.as_str()) == stored.reference.receiver_name.as_deref()
                    && binding.bound_type == ty
                    && binding_holds_at(binding, stored.reference.line)
            })
        })
        .map_or(
            (stored.reference.line, stored.reference.column),
            |binding| (binding.line as usize, binding.column as usize),
        );
    let mut imported = false;
    let mut import_depth = 0;
    let mut candidates: Vec<&NameIndexEntry> = Vec::new();
    for reference in caches.imports(store, stored.file_id)?.iter() {
        let Some(depth) = visible_import_depth(snapshot, stored, reference, position) else {
            continue;
        };
        let Some(statement) = reference.raw_text.as_deref() else {
            continue;
        };
        let Some(name) = imported_name(statement, &reference.specifier, ty) else {
            continue;
        };
        if imported && depth < import_depth {
            continue;
        }
        if depth > import_depth {
            candidates.clear();
        }
        import_depth = depth;
        imported = true;
        let Some(file) = import_target_file(snapshot, "python", path, &reference.specifier) else {
            continue;
        };
        let qualified = name.replace('.', "::");
        let tail = qualified.rsplit("::").next().expect("type name");
        let Some(hits) = snapshot.lookup_candidates(tail) else {
            return Ok((None, true));
        };
        for hit in hits.iter().filter(|hit| {
            hit.file_id == file
                && hit.qualified_name == qualified
                && hit.symbol_type.is_some_and(is_type_scope)
        }) {
            if !candidates
                .iter()
                .any(|known| known.entity_id == hit.entity_id)
            {
                candidates.push(hit);
            }
        }
    }
    if !imported {
        let qualified = ty.replace('.', "::");
        let tail = qualified.rsplit("::").next().expect("type name");
        candidates = snapshot
            .lookup_candidates(tail)
            .into_iter()
            .flatten()
            .filter(|hit| {
                hit.file_id == stored.file_id
                    && hit.qualified_name == qualified
                    && hit.symbol_type.is_some_and(is_type_scope)
            })
            .collect();
        // Workspace uniqueness does not make a Python name visible here.
        // Missing local declarations and imports must remain unresolved.
    }
    // The imported declaration is a static contract. An override in one of
    // its subclasses does not identify the concrete receiver at runtime.
    Ok((
        match candidates.as_slice() {
            [hit] => Some(ClassScope {
                name: hit.qualified_name.clone(),
                declared_in: Some(hit.file_id),
                entity: Some(hit.entity_id.clone()),
            }),
            // A missing or ambiguous import must not fall back to a namesake.
            _ => None,
        },
        imported,
    ))
}

/// Import scope is the existing edge source, not a second symbol table.
/// Only the module or a lexical ancestor can supply the annotation's name.
fn visible_import_depth(
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    import: &ImportReference,
    position: (usize, usize),
) -> Option<usize> {
    if import.owner_id == format!("f{}", stored.file_id) {
        return Some(0);
    }
    if (import.position.0 as usize, import.position.1 as usize) > position {
        return None;
    }
    let owner = snapshot.entity_record(&stored.reference.owner_id)?;
    let scope = &snapshot.entity_record(&import.owner_id)?.qualified_name;
    (owner.qualified_name == *scope || owner.qualified_name.starts_with(&format!("{scope}::")))
        .then(|| scope.split("::").count())
}

/// Read the spelling inside the module named by this import reference.
/// The stored statement can contain several imports, so match the module as
/// well as its local alias before following a namespace-qualified annotation.
fn imported_name(statement: &str, specifier: &str, ty: &str) -> Option<String> {
    let statement = without_line_comments(statement, "#");
    let statement = statement.trim();
    if let Some(rest) = statement.strip_prefix("from ") {
        let (module, names) = rest.split_once(" import ")?;
        if module.trim() != specifier {
            return None;
        }
        for entry in names.trim().trim_matches(['(', ')']).split(',') {
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
