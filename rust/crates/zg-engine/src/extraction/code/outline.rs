//! Structural summaries of stored source, without reading current workspace files.

use std::path::Path;

use super::{
    adapter::{LanguageAdapter, named_children, resolve_adapter},
    grammar,
};
use crate::domain::{Content, EntityMetadata, FileFormat, SymbolType};
use tree_sitter::{Node, Parser};

const MAX_CALLS: usize = 24;
const MAX_MEMBERS: usize = 32;
const MAX_LINE_CHARS: usize = 180;
const MAX_OUTLINE_CHARS: usize = 3_600;

pub(crate) fn code_outline(
    path: &Path,
    content: &Content,
    metadata: Option<&EntityMetadata>,
) -> Option<String> {
    let Some(EntityMetadata::Code(metadata)) = metadata else {
        return None;
    };
    let structural = matches!(
        metadata.symbol_type,
        Some(SymbolType::Class | SymbolType::Interface | SymbolType::Module)
    );
    if !structural && metadata.symbol_type != Some(SymbolType::Function) {
        return None;
    }
    let (Content::Code(source) | Content::Text(source)) = content else {
        return None;
    };
    let formats = FileFormat::from_file_name(path.file_name()?);
    let format = if formats.contains(&FileFormat::Cpp) {
        FileFormat::Cpp
    } else if formats
        .iter()
        .any(|format| matches!(format, FileFormat::Vue | FileFormat::Svelte))
    {
        // Component entities already contain extracted script source.
        FileFormat::TypeScript
    } else {
        *formats
            .iter()
            .find(|format| resolve_adapter(**format).is_some())?
    };
    let adapter = resolve_adapter(format)?;
    let jsx = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tsx"));
    let mut parser = Parser::new();
    parser.set_language(&grammar(format, jsx)?).ok()?;
    let mut tree = parser.parse(source, None)?;
    let mut bytes = source.as_bytes();
    let wrapped_source;
    if matches!(format, FileFormat::JavaScript | FileFormat::TypeScript)
        && metadata.symbol_type == Some(SymbolType::Function)
        && find_entity(
            tree.root_node(),
            adapter,
            bytes,
            metadata.symbol_name.as_deref(),
        )
        .is_none()
    {
        // Stored methods lack their class body; restore that syntax before finding calls.
        wrapped_source = format!("class OutlineContainer {{\n{source}\n}}");
        tree = parser.parse(&wrapped_source, None)?;
        bytes = wrapped_source.as_bytes();
    }
    let root = tree.root_node();
    let mut details = Vec::new();
    if structural {
        let entity =
            find_entity(root, adapter, bytes, metadata.symbol_name.as_deref()).unwrap_or(root);
        collect_members(entity, entity, adapter, bytes, 0, &mut details);
        if !details.is_empty() {
            details.insert(0, "members:".into());
        }
    } else {
        collect_calls(root, bytes, &mut details);
        if details.is_empty() {
            return None;
        }
        details = vec![format!("calls: {}", details.join(", "))];
    }
    let mut header = Vec::new();
    for line in source.lines().take(24) {
        header.push(line);
        if line.contains('{') {
            break;
        }
    }
    let header = clip(header.join("\n").trim(), 1_200);
    let outline = if details.is_empty() {
        header
    } else {
        format!("{header}\n\n{}", details.join("\n"))
    };
    Some(clip(outline.trim(), MAX_OUTLINE_CHARS))
}

fn find_entity<'a>(
    node: Node<'a>,
    adapter: &LanguageAdapter,
    source: &[u8],
    name: Option<&str>,
) -> Option<Node<'a>> {
    if adapter.is_entity(node) && adapter.extract_name(node, source).as_deref() == name {
        return Some(node);
    }
    named_children(node)
        .into_iter()
        .find_map(|child| find_entity(child, adapter, source, name))
}

fn collect_members(
    node: Node<'_>,
    root: Node<'_>,
    adapter: &LanguageAdapter,
    source: &[u8],
    depth: usize,
    members: &mut Vec<String>,
) {
    if members.len() >= MAX_MEMBERS || depth > 12 {
        return;
    }
    if node != root && adapter.is_entity(node) {
        for entity in adapter.resolve_entities(node, source) {
            if members.len() >= MAX_MEMBERS {
                break;
            }
            let kind = adapter.classify(entity).map_or("code", SymbolType::as_str);
            let name = adapter.extract_name(entity, source).unwrap_or_default();
            let signature = adapter
                .extract_signature(entity, source)
                .map(|value| clip(&one_line(&value), MAX_LINE_CHARS))
                .unwrap_or_default();
            let member = if signature.is_empty() {
                format!("{kind} {name}").trim().to_owned()
            } else if !name.is_empty() && !signature.contains(&name) {
                format!("{kind} {name}: {signature}")
            } else {
                format!("{kind} {signature}")
            };
            members.push(format!("- {member}"));
        }
        return;
    }
    for child in named_children(node) {
        collect_members(child, root, adapter, source, depth + 1, members);
    }
}

fn collect_calls(root: Node<'_>, source: &[u8], calls: &mut Vec<String>) {
    // An explicit stack avoids recursion on deeply nested expressions.
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if calls.len() >= MAX_CALLS {
            break;
        }
        if matches!(
            node.kind(),
            "call"
                | "call_expression"
                | "function_call_expression"
                | "method_invocation"
                | "object_creation_expression"
                | "new_expression"
        ) {
            let target = ["function", "name", "constructor", "type"]
                .into_iter()
                .find_map(|field| node.child_by_field_name(field))
                .or_else(|| node.named_child(0));
            if let Some(target) = target
                && let Ok(text) = target.utf8_text(source)
            {
                let name = one_line(text);
                let name = name.strip_prefix("new ").unwrap_or(&name).trim();
                if name.chars().count() <= MAX_LINE_CHARS
                    && name.chars().any(|character| {
                        character.is_ascii_alphabetic() || matches!(character, '_' | '$')
                    })
                    && !calls.iter().any(|call| call == name)
                {
                    calls.push(name.to_owned());
                }
            }
        }
        pending.extend(named_children(node).into_iter().rev());
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn clip(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    format!(
        "{}...",
        value.chars().take(max - 3).collect::<String>().trim_end()
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::domain::CodeMetadata;

    fn metadata(kind: SymbolType, name: &str) -> EntityMetadata {
        EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(kind),
            symbol_name: Some(name.into()),
            scope: None,
            signature: None,
            documentation: None,
            visibility: None,
            language: None,
        })
    }

    #[test]
    fn stored_functions_keep_syntactic_calls_in_source_order() {
        let source = "TEST_F(QueryInfoTest, Aggregation) {\n  auto engine = SQLEngine::create();\n  engine->parse_request();\n  ASSERT_EQ(meta_.get(), meta_.get());\n}";
        let extracted = crate::extraction::extract_for_indexing(
            &crate::extraction::TextSource {
                relative_path: crate::domain::SourcePath::new("missing.cpp").expect("source path"),
                formats: vec![FileFormat::Cpp],
                text: source.into(),
            },
            crate::extraction::ChunkOptions::default(),
        )
        .expect("C++ extraction");
        let entity = extracted.fragments.iter().find(|entity| {
            matches!(&entity.metadata, Some(EntityMetadata::Code(code)) if code.symbol_name.as_deref() == Some("TEST_F"))
        }).expect("test function entity");
        let outline = code_outline(
            Path::new("missing.cpp"),
            &entity.content,
            entity.metadata.as_ref(),
        )
        .expect("outline from stored source");
        assert_eq!(
            outline,
            "TEST_F(QueryInfoTest, Aggregation) {\n\ncalls: SQLEngine::create, engine->parse_request, ASSERT_EQ, meta_.get"
        );
    }

    #[test]
    fn component_script_entities_have_outlines_without_rereading_the_file() {
        for path in ["component.vue", "component.svelte"] {
            let outline = code_outline(
                Path::new(path),
                &Content::Code("function load() { refresh(); }".into()),
                Some(&metadata(SymbolType::Function, "load")),
            )
            .expect("component outline");
            assert!(outline.contains("calls: refresh"), "{outline}");
        }
    }

    #[test]
    fn stored_method_signatures_are_not_reported_as_calls() {
        for (path, source) in [
            ("store.ts", "get(key: string) { return load(key); }"),
            ("store.js", "get(key) { return load(key); }"),
            ("Store.java", "String get(String key) { return load(key); }"),
        ] {
            let outline = code_outline(
                Path::new(path),
                &Content::Code(source.into()),
                Some(&metadata(SymbolType::Function, "get")),
            )
            .expect("method outline");
            assert!(outline.ends_with("calls: load"), "{path}: {outline}");
        }
    }

    #[test]
    fn classes_include_direct_members_and_functions_without_calls_have_no_outline() {
        let source = "class Store {\n  get(key: string) { return load(key); }\n  close() {}\n}";
        let outline = code_outline(
            Path::new("store.ts"),
            &Content::Code(source.into()),
            Some(&metadata(SymbolType::Class, "Store")),
        )
        .expect("class outline");
        assert!(
            outline.contains("members:\n- function get(key: string)"),
            "{outline}"
        );
        assert!(outline.contains("- function close()"), "{outline}");
        assert!(!outline.contains("load("));
        assert!(
            code_outline(
                Path::new("empty.rs"),
                &Content::Code("fn empty() {}".into()),
                Some(&metadata(SymbolType::Function, "empty"))
            )
            .is_none()
        );
    }

    #[test]
    fn call_summaries_are_bounded_and_keep_unicode_valid() {
        let mut source = String::from("fn run() { ");
        for index in 0..40 {
            write!(&mut source, "call_{index}();").expect("source fixture");
        }
        source.push_str(" }");
        let outline = code_outline(
            Path::new("run.rs"),
            &Content::Code(source),
            Some(&metadata(SymbolType::Function, "run")),
        )
        .expect("calls");
        let calls = outline.split("calls: ").nth(1).expect("call list");
        assert_eq!(calls.split(", ").count(), MAX_CALLS);
        assert!(calls.ends_with("call_23"));
        assert_eq!(clip("中文测试", 3), "...");
    }
}
