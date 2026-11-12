//! Rust impl identities follow explicit use paths inside their own crate.
//! Source/module and import evidence is cached for one sweep; unresolved paths
//! never justify matching a different file's same-named type.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use tree_sitter::{Node, Parser};

use super::{IndexStore, ResolutionSnapshot, RuleCaches, is_type_scope};

#[derive(Default)]
pub(super) struct TypeCache {
    identities: HashMap<(u32, String), (u32, String)>,
    imports: HashMap<u32, Arc<HashMap<String, Option<String>>>>,
    modules: HashMap<u32, Module>,
    crate_directories: HashMap<String, Option<String>>,
}

#[derive(Clone)]
struct Module {
    source_base: String,
    directory: String,
}

pub(super) fn identity(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    file: u32,
    scope: &str,
    caches: &mut RuleCaches,
) -> crate::EngineResult<(u32, String)> {
    identity_inner(store, snapshot, file, scope, caches, 8)
}

fn identity_inner(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    file: u32,
    scope: &str,
    caches: &mut RuleCaches,
    remaining: usize,
) -> crate::EngineResult<(u32, String)> {
    let key = (file, scope.to_owned());
    if let Some(identity) = caches.rust_types.identities.get(&key) {
        return Ok(identity.clone());
    }
    // Seed the local identity to bound cycles in reexports. Keep arguments:
    // dropping them would conflate concrete specialized impls.
    caches
        .rust_types
        .identities
        .insert(key.clone(), key.clone());
    if remaining == 0 {
        return Ok(key);
    }
    let (name, arguments) = scope.find('<').map_or((scope, ""), |at| scope.split_at(at));
    let imports = imports(store, file, caches)?;
    let Some(path) = imports.get(name).and_then(Option::as_deref) else {
        return Ok(key);
    };
    let Some((module, original)) = path.rsplit_once("::") else {
        return Ok(key);
    };
    let Some(source) = snapshot.file_path(file) else {
        return Ok(key);
    };
    let context = caches
        .rust_types
        .modules
        .entry(file)
        .or_insert_with(|| {
            Module::new(
                source,
                caches.source_root.as_deref(),
                &mut caches.rust_types.crate_directories,
            )
        })
        .clone();
    let mut targets = context.files(snapshot, file, module);
    if targets.len() > 1 {
        // A package may have both lib.rs and main.rs; the indexed type must
        // select one rather than arbitrarily picking an entry point.
        targets.retain(|file| {
            snapshot
                .lookup_candidates(original)
                .unwrap_or_default()
                .iter()
                .any(|hit| {
                    hit.file_id == *file
                        && hit.qualified_name == original
                        && hit.symbol_type.is_some_and(is_type_scope)
                })
        });
    }
    let result = if targets.len() == 1 {
        identity_inner(
            store,
            snapshot,
            *targets.iter().next().expect("one module"),
            &format!("{original}{arguments}"),
            caches,
            remaining - 1,
        )?
    } else {
        key.clone()
    };
    caches.rust_types.identities.insert(key, result.clone());
    Ok(result)
}

fn imports(
    store: &IndexStore,
    file: u32,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Arc<HashMap<String, Option<String>>>> {
    if let Some(imports) = caches.rust_types.imports.get(&file) {
        return Ok(Arc::clone(imports));
    }
    let mut imports = HashMap::new();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("Rust grammar");
    for import in caches.imports(store, file)?.iter() {
        let raw = import
            .raw_text
            .clone()
            .unwrap_or_else(|| format!("use {};", import.specifier));
        if let Some(tree) = parser.parse(&raw, None)
            && !tree.root_node().has_error()
        {
            let mut cursor = tree.root_node().walk();
            for node in tree.root_node().named_children(&mut cursor) {
                if let Some(argument) = node.child_by_field_name("argument") {
                    collect_use(argument, "", raw.as_bytes(), &mut imports);
                }
            }
        }
    }
    let imports = Arc::new(imports);
    caches.rust_types.imports.insert(file, Arc::clone(&imports));
    Ok(imports)
}

fn collect_use(
    node: Node<'_>,
    prefix: &str,
    source: &[u8],
    imports: &mut HashMap<String, Option<String>>,
) {
    let text = |node: Node<'_>| {
        node.utf8_text(source)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<String>()
    };
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(text)
                .unwrap_or_default();
            if let Some(list) = node.child_by_field_name("list") {
                collect_use(list, &qualified(prefix, &path), source, imports);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_use(child, prefix, source, imports);
            }
        }
        "use_as_clause" => {
            if let (Some(path), Some(alias)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) {
                add_import(imports, text(alias), qualified(prefix, &text(path)));
            }
        }
        "identifier" | "scoped_identifier" | "self" | "super" | "crate" => {
            let path = qualified(prefix, &text(node));
            if let Some(name) = path.rsplit("::").next() {
                add_import(imports, name.to_owned(), path);
            }
        }
        _ => {} // Glob imports alone do not prove a type's defining module.
    }
}

fn add_import(imports: &mut HashMap<String, Option<String>>, name: String, path: String) {
    imports
        .entry(name)
        .and_modify(|previous| {
            if previous.as_deref() != Some(&path) {
                *previous = None;
            }
        })
        .or_insert(Some(path));
}

fn qualified(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

impl Module {
    fn new(
        file: &str,
        root: Option<&Path>,
        directories: &mut HashMap<String, Option<String>>,
    ) -> Self {
        let parent = super::importing_dir(file);
        let crate_directory = nearest_crate(parent, root, directories);
        let inferred = file
            .rsplit_once("/src/")
            .map(|(prefix, _)| format!("{prefix}/src"));
        let source_base = crate_directory.map_or_else(
            || {
                inferred.unwrap_or_else(|| {
                    if file.starts_with("src/") {
                        "src".into()
                    } else {
                        String::new()
                    }
                })
            },
            |directory| {
                let src = slash_join(&directory, "src");
                if file.starts_with(&format!("{src}/")) {
                    src
                } else {
                    directory
                }
            },
        );
        let directory = if matches!(
            file.rsplit('/').next(),
            Some("lib.rs" | "main.rs" | "mod.rs")
        ) {
            parent.to_owned()
        } else {
            file.strip_suffix(".rs").unwrap_or(file).to_owned()
        };
        Self {
            source_base,
            directory,
        }
    }

    fn files(&self, snapshot: &ResolutionSnapshot, file: u32, module: &str) -> HashSet<u32> {
        if module == "self" {
            return HashSet::from([file]);
        }
        let mut parts = module.split("::").peekable();
        let mut base = match parts.peek().copied() {
            Some("self" | "super") => self.directory.clone(),
            Some("crate") => {
                parts.next();
                self.source_base.clone()
            }
            _ => self.source_base.clone(),
        };
        if parts.peek() == Some(&"self") {
            parts.next();
        }
        while parts.peek() == Some(&"super") {
            if base == self.source_base {
                return HashSet::new();
            }
            base = super::importing_dir(&base).to_owned();
            parts.next();
        }
        for part in parts {
            if part.is_empty() || matches!(part, "self" | "super" | "crate") {
                return HashSet::new();
            }
            base = slash_join(&base, part);
        }
        let mut paths = vec![format!("{base}.rs"), slash_join(&base, "mod.rs")];
        if base == self.source_base {
            paths.extend([slash_join(&base, "lib.rs"), slash_join(&base, "main.rs")]);
        }
        paths
            .iter()
            .filter_map(|path| snapshot.file_id_by_path(path))
            .collect()
    }
}

fn slash_join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

fn nearest_crate(
    directory: &str,
    root: Option<&Path>,
    cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let root = root?;
    let mut current = directory;
    let mut visited = Vec::new();
    let found = loop {
        if let Some(found) = cache.get(current) {
            break found.clone();
        }
        visited.push(current.to_owned());
        if root.join(current).join("Cargo.toml").is_file() {
            break Some(current.to_owned());
        }
        if current.is_empty() {
            break None;
        }
        current = super::importing_dir(current);
    };
    for directory in visited {
        cache.insert(directory, found.clone());
    }
    found
}
