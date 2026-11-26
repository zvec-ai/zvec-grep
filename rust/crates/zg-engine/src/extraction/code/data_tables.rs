//! Retain package-level lookup tables that the symbol walk skips.
mod catalog_labels;
mod class_forward;

use tree_sitter::Node;

use super::{
    CodeMetadata, EntityMetadata, ExtractedEntity, FileFormat, SymbolType, TextSource,
    append_code_entity, named_children, resolve_adapter,
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
    let Some(adapter) = resolve_adapter(format) else {
        return;
    };
    let mut retain = |node: Node<'_>, name: Node<'_>, forwarded: bool| {
        let metadata = EntityMetadata::Code(CodeMetadata {
            symbol_name: Some(source.text[name.byte_range()].to_owned()),
            symbol_type: Some(SymbolType::Value),
            language: Some(adapter.language()),
            is_exported: adapter.is_exported(node, false),
            documentation: adjacent_documentation(node, &source.text),
            ..CodeMetadata::default()
        });
        append_code_entity(
            source,
            node,
            metadata,
            max_chars,
            overlap_chars,
            |text, max, overlap| {
                if forwarded
                    && let Some(labels) = catalog_labels::fragments(node, &source.text, max)
                {
                    labels
                } else {
                    super::text_fragments(text, max, overlap)
                }
            },
            output,
        );
    };
    let mut python_tables = Vec::new();
    // Direct children only. Function and class entities already own their bodies.
    for node in named_children(root) {
        match (format, node.kind()) {
            (FileFormat::Python, "expression_statement") => {
                if let Some(name) = python_table_binding(node) {
                    python_tables.push((node, name));
                }
            }
            (FileFormat::Go, "var_declaration") => {
                for child in named_children(node) {
                    if child.kind() == "var_spec_list" {
                        for spec in named_children(child) {
                            if let Some(name) = go_table_binding(spec, &source.text) {
                                retain(spec, name, false);
                            }
                        }
                    } else if let Some(name) = go_table_binding(child, &source.text) {
                        retain(node, name, false);
                    }
                }
            }
            _ => {}
        }
    }
    let forwarded = class_forward::forwarded(&python_tables, root, &source.text);
    for (node, name) in python_tables {
        retain(node, name, forwarded.contains(&name.id()));
    }
}

fn python_table_binding(statement: Node<'_>) -> Option<Node<'_>> {
    let assignment = statement.named_child(0)?;
    if assignment.kind() != "assignment" || statement.has_error() {
        return None;
    }
    let name = assignment.child_by_field_name("left")?;
    if assignment
        .child_by_field_name("type")
        .is_some_and(contains_call)
    {
        return None;
    }
    let value = unparenthesize(assignment.child_by_field_name("right")?)?;
    (name.kind() == "identifier"
        && value.kind() == "dictionary"
        && has_entry(value, "pair")
        && is_table_data(value, FileFormat::Python))
    .then_some(name)
}

fn go_table_binding<'a>(spec: Node<'a>, source: &str) -> Option<Node<'a>> {
    if spec.kind() != "var_spec"
        || spec.has_error()
        || spec.child_by_field_name("type").is_some_and(contains_call)
    {
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
    let value = unparenthesize(values.next()?)?;
    let ty = value.child_by_field_name("type")?;
    if values.next().is_some()
        || value.kind() != "composite_literal"
        || ty.kind() != "map_type"
        || contains_call(ty)
    {
        return None;
    }
    let body = value.child_by_field_name("body")?;
    (has_entry(body, "keyed_element") && is_table_data(body, FileFormat::Go)).then_some(name)
}

fn unparenthesize(mut node: Node<'_>) -> Option<Node<'_>> {
    while node.kind() == "parenthesized_expression" {
        node = node
            .named_children(&mut node.walk())
            .find(|child| !child.is_extra())?;
    }
    Some(node)
}

fn has_entry(node: Node<'_>, kind: &str) -> bool {
    node.named_children(&mut node.walk())
        .any(|child| child.kind() == kind)
}

fn is_table_data(root: Node<'_>, format: FileFormat) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match (format, node.kind()) {
            (_, "comment" | "true" | "false" | "identifier")
            | (FileFormat::Python, "none" | "integer" | "float")
            | (
                FileFormat::Go,
                "field_identifier"
                | "nil"
                | "int_literal"
                | "float_literal"
                | "imaginary_literal"
                | "rune_literal"
                | "raw_string_literal"
                | "interpreted_string_literal",
            ) => {}
            (FileFormat::Python, "string") if !has_entry(node, "interpolation") => {}
            (FileFormat::Python, "attribute") | (FileFormat::Go, "selector_expression") => {
                if !is_name_reference(node) {
                    return false;
                }
            }
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
                if node.child_by_field_name("type").is_some_and(contains_call) {
                    return false;
                }
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

fn contains_call(root: Node<'_>) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "call" | "call_expression") {
            return true;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    false
}

fn is_name_reference(mut node: Node<'_>) -> bool {
    loop {
        let next = match node.kind() {
            "identifier" => return true,
            "attribute" => node.child_by_field_name("object"),
            "selector_expression" => node.child_by_field_name("operand"),
            "parenthesized_expression" => unparenthesize(node),
            _ => return false,
        };
        let Some(child) = next else { return false };
        node = child;
    }
}

// A blank line or a trailing comment on the previous declaration breaks attachment.
fn adjacent_documentation(node: Node<'_>, source: &str) -> Option<String> {
    let mut next_row = node.start_position().row;
    let mut comments = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(item) = sibling {
        if item.kind() != "comment" || item.end_position().row + 1 != next_row {
            break;
        }
        let start = item.start_byte();
        let line = &source[start - item.start_position().column..start];
        if !line.trim().is_empty() {
            break;
        }
        comments.push(super::adapter::clean_comment_text(
            &source[item.byte_range()],
        ));
        next_row = item.start_position().row;
        sibling = item.prev_named_sibling();
    }
    comments.reverse();
    let doc = comments.join("\n");
    (!doc.is_empty()).then_some(doc)
}
