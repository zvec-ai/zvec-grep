use std::{ops::Range, path::Path};

use tree_sitter::{Language, Node, Parser};

use crate::{CodeSymbol, CodeSymbolKind, FileFormat, file::ScannedFile};

pub(super) struct SyntaxUnit {
    pub lines: Range<usize>,
    pub symbol: Option<CodeSymbol>,
}

pub(super) fn syntax_ranges(
    file: &ScannedFile,
    source: &str,
    max_chars: usize,
) -> Option<Vec<SyntaxUnit>> {
    let language = language(file.format, &file.relative_path)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() && root.named_child_count() == 0 {
        return None;
    }

    let mut cursor = root.walk();
    let mut ranges = Vec::new();
    let mut grouped: Option<Group> = None;
    for node in root.named_children(&mut cursor) {
        if is_symbol(node.kind()) {
            flush_group(&mut ranges, grouped.take());
            ranges.push(SyntaxUnit {
                lines: line_range(node),
                symbol: symbol(node, source),
            });
            continue;
        }
        match grouped.as_mut() {
            Some(group) if node.end_byte().saturating_sub(group.start_byte) <= max_chars => {
                group.end_line = node.end_position().row + 1;
            }
            Some(_) => {
                flush_group(&mut ranges, grouped.take());
                grouped = Some(Group::from(node));
            }
            None => grouped = Some(Group::from(node)),
        }
    }
    flush_group(&mut ranges, grouped);
    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

struct Group {
    start_byte: usize,
    start_line: usize,
    end_line: usize,
}

impl From<Node<'_>> for Group {
    fn from(node: Node<'_>) -> Self {
        Self {
            start_byte: node.start_byte(),
            start_line: node.start_position().row,
            end_line: node.end_position().row + 1,
        }
    }
}

fn flush_group(ranges: &mut Vec<SyntaxUnit>, group: Option<Group>) {
    if let Some(group) = group {
        ranges.push(SyntaxUnit {
            lines: group.start_line..group.end_line,
            symbol: None,
        });
    }
}

fn symbol(node: Node<'_>, source: &str) -> Option<CodeSymbol> {
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))?
        .utf8_text(source.as_bytes())
        .ok()?
        .trim()
        .to_owned();
    if name.is_empty() {
        return None;
    }
    let kind = node.kind();
    Some(CodeSymbol {
        name,
        kind: if kind.contains("method") {
            CodeSymbolKind::Method
        } else if kind.contains("function") {
            CodeSymbolKind::Function
        } else if kind.contains("interface") {
            CodeSymbolKind::Interface
        } else if kind.contains("enum") {
            CodeSymbolKind::Enum
        } else if kind.contains("trait") {
            CodeSymbolKind::Trait
        } else if kind.contains("module") {
            CodeSymbolKind::Module
        } else if kind.contains("const") {
            CodeSymbolKind::Constant
        } else if kind.contains("class") || kind.contains("struct") || kind.contains("impl") {
            CodeSymbolKind::Type
        } else {
            CodeSymbolKind::Other
        },
    })
}

fn line_range(node: Node<'_>) -> Range<usize> {
    node.start_position().row..node.end_position().row + 1
}

fn is_symbol(kind: &str) -> bool {
    [
        "function",
        "method",
        "class",
        "struct",
        "enum",
        "trait",
        "impl",
        "interface",
        "declaration",
    ]
    .iter()
    .any(|part| kind.contains(part))
}

fn language(format: FileFormat, path: &Path) -> Option<Language> {
    match format {
        FileFormat::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        FileFormat::TypeScript => {
            if path.extension().is_some_and(|extension| extension == "tsx") {
                Some(tree_sitter_typescript::LANGUAGE_TSX.into())
            } else {
                Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            }
        }
        FileFormat::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        FileFormat::Python => Some(tree_sitter_python::LANGUAGE.into()),
        FileFormat::Go => Some(tree_sitter_go::LANGUAGE.into()),
        FileFormat::Java => Some(tree_sitter_java::LANGUAGE.into()),
        FileFormat::C => Some(tree_sitter_c::LANGUAGE.into()),
        FileFormat::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        _ => None,
    }
}
