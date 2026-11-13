//! Match file modules from indexed declarations without changing stored lexical scopes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tree_sitter::Parser;

use crate::{
    EngineResult,
    domain::{CodeMetadata, Content, EntityId, FileRecord, Language, SymbolType},
    storage::ReadView,
};

#[derive(Default)]
pub(super) struct RustModules {
    declarations: HashMap<String, Arc<Vec<ModuleDeclaration>>>,
    matches: HashMap<(PathBuf, String), bool>,
    external: HashMap<EntityId, bool>,
}

struct ModuleDeclaration {
    id: EntityId,
    file: FileRecord,
    scope: String,
}

impl RustModules {
    pub(super) fn matches_scope(
        &mut self,
        storage: &ReadView<'_>,
        file: &FileRecord,
        metadata: &CodeMetadata,
        scope: &str,
    ) -> EngineResult<bool> {
        if metadata.language != Some(Language::Rust) || scope.is_empty() {
            return Ok(false);
        }
        let scope = if scope == "crate" {
            ""
        } else {
            scope.strip_prefix("crate::").unwrap_or(scope)
        };
        let Some(module) = enclosing_module(scope, metadata.scope.as_deref().unwrap_or("")) else {
            return Ok(false);
        };
        self.file_matches(storage, file.relative_path().as_path(), module)
    }

    fn file_matches(
        &mut self,
        storage: &ReadView<'_>,
        file: &Path,
        module: &str,
    ) -> EngineResult<bool> {
        if module.is_empty() {
            return Ok(matches!(
                file.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs")
            ));
        }
        let key = (file.to_owned(), module.to_owned());
        if let Some(&matched) = self.matches.get(&key) {
            return Ok(matched);
        }
        let (parent_module, name) = module.rsplit_once("::").unwrap_or(("", module));
        let declarations = self.declarations(storage, name)?;
        let mut matched = false;
        for declaration in declarations.iter() {
            let parent = declaration.file.relative_path().as_path();
            let Some(enclosing) = enclosing_module(parent_module, &declaration.scope) else {
                continue;
            };
            let base = if matches!(
                parent.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs" | "mod.rs")
            ) {
                parent.parent().expect("source parent").to_path_buf()
            } else {
                parent.with_extension("")
            };
            let base = declaration
                .scope
                .split("::")
                .filter(|part| !part.is_empty())
                .fold(base, |base, part| base.join(part))
                .join(name);
            if (file == base.with_extension("rs") || file == base.join("mod.rs"))
                && self.is_external(storage, declaration)?
                && self.file_matches(storage, parent, enclosing)?
            {
                matched = true;
                break;
            }
        }
        self.matches.insert(key, matched);
        Ok(matched)
    }

    fn declarations(
        &mut self,
        storage: &ReadView<'_>,
        name: &str,
    ) -> EngineResult<Arc<Vec<ModuleDeclaration>>> {
        if let Some(declarations) = self.declarations.get(name) {
            return Ok(Arc::clone(declarations));
        }
        let ids = storage.find_symbol_entity_ids(name, None)?;
        let mut declarations = Vec::new();
        for batch in ids.chunks(128) {
            let metadata = storage.fetch_code_metadata(batch)?;
            let ids = metadata
                .iter()
                .filter(|(_, (_, metadata))| {
                    metadata.language == Some(Language::Rust)
                        && metadata.symbol_type == Some(SymbolType::Module)
                        && metadata.symbol_name.as_deref() == Some(name)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let file_ids = ids.iter().map(|id| metadata[id].0).collect::<Vec<_>>();
            let files = storage.fetch_files(&file_ids)?;
            for id in ids {
                let (file_id, metadata) = &metadata[&id];
                let Some(file) = files
                    .get(file_id)
                    .filter(|file| file.index_status().is_indexed())
                else {
                    continue;
                };
                declarations.push(ModuleDeclaration {
                    id,
                    file: file.clone(),
                    scope: metadata.scope.clone().unwrap_or_default(),
                });
            }
        }
        let declarations = Arc::new(declarations);
        self.declarations
            .insert(name.to_owned(), Arc::clone(&declarations));
        Ok(declarations)
    }

    // Load a module's body only after its path could own the requested file.
    // A common inline name such as `tests` must not hydrate every module body.
    fn is_external(
        &mut self,
        storage: &ReadView<'_>,
        declaration: &ModuleDeclaration,
    ) -> EngineResult<bool> {
        if let Some(&external) = self.external.get(&declaration.id) {
            return Ok(external);
        }
        let entity = storage
            .fetch_entities(std::slice::from_ref(&declaration.id))?
            .remove(&declaration.id);
        let mut external = false;
        if let Some(entity) = entity.filter(|entity| entity.file_id() == declaration.file.id())
            && let Content::Code(text) = entity.content()
            && text.trim_end().ends_with(';')
        {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .expect("Rust grammar");
            if let Some(tree) = parser
                .parse(text, None)
                .filter(|tree| !tree.root_node().has_error())
            {
                let mut cursor = tree.root_node().walk();
                let mut found = false;
                // Inline modules and attributes (notably #[path]) cannot establish
                // the conventional external-module path used below.
                external = tree.root_node().named_children(&mut cursor).all(|node| {
                    if node.kind() == "mod_item" && node.child_by_field_name("body").is_none() {
                        found = true;
                        true
                    } else {
                        matches!(node.kind(), "line_comment" | "block_comment")
                    }
                }) && found;
            }
        }
        self.external.insert(declaration.id.clone(), external);
        Ok(external)
    }
}

fn enclosing_module<'a>(scope: &'a str, lexical_scope: &str) -> Option<&'a str> {
    if lexical_scope.is_empty() {
        Some(scope)
    } else if scope == lexical_scope {
        Some("")
    } else {
        scope.strip_suffix(lexical_scope)?.strip_suffix("::")
    }
}
