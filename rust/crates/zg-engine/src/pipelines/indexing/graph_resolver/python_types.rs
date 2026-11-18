//! Locate a declared Python receiver type through its importing module.

use super::{
    ClassScope, IndexStore, NameIndexEntry, ResolutionSnapshot, RuleCaches, StoredPendingRef,
    import_target_file, is_type_scope, type_root_scope_in, without_line_comments,
};

pub(super) fn scope(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    ty: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<ClassScope>> {
    let Some(path) = snapshot.file_path(stored.file_id) else {
        return Ok(None);
    };
    let mut imported = false;
    let mut candidates: Vec<&NameIndexEntry> = Vec::new();
    for reference in caches.imports(store, stored.file_id)?.iter() {
        let Some(statement) = reference.raw_text.as_deref() else {
            continue;
        };
        let Some(name) = imported_name(statement, &reference.specifier, ty) else {
            continue;
        };
        imported = true;
        let Some(file) = import_target_file(snapshot, "python", path, &reference.specifier) else {
            continue;
        };
        let qualified = name.replace('.', "::");
        let tail = qualified.rsplit("::").next().expect("type name");
        let Some(hits) = snapshot.lookup_candidates(tail) else {
            return Ok(None);
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
        if candidates.is_empty() {
            return type_root_scope_in(store, snapshot, stored, ty, caches, 0);
        }
    }
    // The imported declaration is a static contract. An override in one of
    // its subclasses does not identify the concrete receiver at runtime.
    Ok(match candidates.as_slice() {
        [hit] => Some(ClassScope {
            name: hit.qualified_name.clone(),
            declared_in: Some(hit.file_id),
            entity: Some(hit.entity_id.clone()),
        }),
        // A missing or ambiguous import must not fall back to a namesake.
        _ => None,
    })
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
            return type_suffix(ty, local)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
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
