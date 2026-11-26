//! Retain package-level literal lookup tables that the symbol walk skips.
use tree_sitter::Node;

use super::{
    CodeMetadata, EntityMetadata, ExtractedEntity, FileFormat, TextSource, append_code_entity,
    named_children,
};

pub(super) fn append(
    source: &TextSource,
    root: Node<'_>,
    format: FileFormat,
    max_chars: usize,
    overlap_chars: usize,
    output: &mut Vec<ExtractedEntity>,
) {
    if !matches!(format, FileFormat::Python | FileFormat::Go) {
        return;
    }
    let mut retain = |node: Node<'_>, name: Node<'_>| {
        let binding = &source.text[name.byte_range()];
        let metadata = CodeMetadata {
            signature: Some(binding.to_owned()),
            ..CodeMetadata::default()
        };
        append_code_entity(
            source,
            node,
            Some(EntityMetadata::Code(metadata)),
            max_chars,
            overlap_chars,
            output,
        );
    };
    // Direct children only. Function and class entities already own their bodies.
    for node in named_children(root) {
        match (format, node.kind()) {
            (FileFormat::Python, "expression_statement") => {
                if let Some(name) = python_table_binding(node) {
                    retain(node, name);
                }
            }
            (FileFormat::Go, "var_declaration") => {
                for child in named_children(node) {
                    if child.kind() == "var_spec_list" {
                        for spec in named_children(child) {
                            if let Some(name) = go_table_binding(spec, &source.text) {
                                retain(spec, name);
                            }
                        }
                    } else if let Some(name) = go_table_binding(child, &source.text) {
                        retain(node, name);
                    }
                }
            }
            _ => {}
        }
    }
}

fn python_table_binding(statement: Node<'_>) -> Option<Node<'_>> {
    let assignment = statement.named_child(0)?;
    if assignment.kind() != "assignment" || statement.has_error() {
        return None;
    }
    let name = assignment.child_by_field_name("left")?;
    let mut value = assignment.child_by_field_name("right")?;
    while value.kind() == "parenthesized_expression" {
        value = value
            .named_children(&mut value.walk())
            .find(|child| !child.is_extra())?;
    }
    (name.kind() == "identifier"
        && value.kind() == "dictionary"
        && has_entry(value, "pair")
        && is_literal_data(value, FileFormat::Python))
    .then_some(name)
}

fn go_table_binding<'a>(spec: Node<'a>, source: &str) -> Option<Node<'a>> {
    if spec.kind() != "var_spec" || spec.has_error() {
        return None;
    }
    let mut cursor = spec.walk();
    let mut names = spec.children_by_field_name("name", &mut cursor);
    let name = names.next()?;
    if &source[name.byte_range()] == "_" || names.next().is_some() {
        return None;
    }
    let values = spec.child_by_field_name("value")?;
    let mut cursor = values.walk();
    let mut values = values
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra());
    let value = values.next()?;
    if values.next().is_some()
        || value.kind() != "composite_literal"
        || value.child_by_field_name("type")?.kind() != "map_type"
    {
        return None;
    }
    let body = value.child_by_field_name("body")?;
    (has_entry(body, "keyed_element") && is_literal_data(body, FileFormat::Go)).then_some(name)
}

fn has_entry(node: Node<'_>, kind: &str) -> bool {
    node.named_children(&mut node.walk())
        .any(|child| child.kind() == kind)
}

fn is_literal_data(root: Node<'_>, format: FileFormat) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match (format, node.kind()) {
            (_, "comment" | "true" | "false") => {}
            (FileFormat::Python, "none" | "integer" | "float") => {}
            (FileFormat::Python, "string") if !has_entry(node, "interpolation") => {}
            (
                FileFormat::Go,
                "nil"
                | "int_literal"
                | "float_literal"
                | "imaginary_literal"
                | "rune_literal"
                | "raw_string_literal"
                | "interpreted_string_literal",
            ) => {}
            (
                FileFormat::Python,
                "dictionary"
                | "pair"
                | "tuple"
                | "list"
                | "set"
                | "parenthesized_expression"
                | "concatenated_string"
                | "unary_operator",
            )
            | (
                FileFormat::Go,
                "literal_value" | "keyed_element" | "literal_element" | "parenthesized_expression",
            ) => pending.extend(named_children(node)),
            (FileFormat::Go, "unary_expression")
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|op| matches!(op.kind(), "+" | "-" | "^")) =>
            {
                pending.extend(named_children(node));
            }
            (FileFormat::Go, "composite_literal") => {
                let Some(body) = node.child_by_field_name("body") else {
                    return false;
                };
                pending.push(body);
            }
            _ => return false,
        }
    }
    true
}
