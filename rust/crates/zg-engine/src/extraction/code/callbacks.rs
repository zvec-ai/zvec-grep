//! Split oversized registration calls at statements in their anonymous callbacks.

use tree_sitter::Node;

use super::adapter::named_children;
use crate::{domain::FileFormat, utils::utf16_len};

/// Interior byte offsets only. The opening call and closing delimiters stay
/// with the first and last groups of statements, respectively.
pub(super) fn boundaries(
    statement: Node<'_>,
    format: FileFormat,
    text: &str,
    budget: usize,
) -> Vec<usize> {
    if !matches!(
        format,
        FileFormat::Go | FileFormat::JavaScript | FileFormat::TypeScript
    ) || !over_budget(statement, text, budget)
    {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    let mut pending = callback_bodies(statement);
    while let Some(body) = pending.pop() {
        let statements = named_children(body)
            .into_iter()
            .filter(|n| !n.is_extra() && !n.is_missing());
        for (index, child) in statements.enumerate() {
            if index > 0 {
                candidates.push(child.start_byte());
            }
            if over_budget(child, text, budget) {
                pending.extend(callback_bodies(child));
                // Parameterized suites frequently put all their cases in a
                // single loop. Only walk control-flow blocks inside a callback.
                match child.kind() {
                    "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
                    | "if_statement" => {
                        for block in named_children(child) {
                            append_block(block, &mut pending);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if candidates.is_empty() {
        return candidates;
    }
    candidates.sort_unstable();
    candidates.dedup();
    // Charge each source slice once. Keep a short callback intact when its
    // enclosing call fits, and pack adjacent small statements into one item.
    let mut cuts = Vec::new();
    let mut previous = statement.start_byte();
    let mut used = 0;
    for end in candidates.into_iter().chain([statement.end_byte()]) {
        let size = utf16_len(&text[previous..end]);
        if used > 0 && used + size > budget {
            cuts.push(previous);
            used = 0;
        }
        used += size;
        previous = end;
    }
    cuts
}

fn over_budget(node: Node<'_>, text: &str, budget: usize) -> bool {
    let bytes = node.byte_range();
    bytes.len() > budget && text[bytes].encode_utf16().nth(budget).is_some()
}

fn callback_bodies(node: Node<'_>) -> Vec<Node<'_>> {
    let mut bodies = Vec::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "var_declaration"
            | "var_spec_list"
            | "expression_list"
            | "lexical_declaration"
            | "variable_declaration"
            | "expression_statement"
            | "parenthesized_expression"
            | "export_statement"
            | "await_expression" => pending.extend(named_children(node)),
            "var_spec" | "variable_declarator" => pending.extend(node.child_by_field_name("value")),
            "assignment_expression" => pending.extend(node.child_by_field_name("right")),
            "call_expression" => {
                let Some(args) = node.child_by_field_name("arguments") else {
                    continue;
                };
                for arg in named_children(args) {
                    if !matches!(
                        arg.kind(),
                        "func_literal"
                            | "arrow_function"
                            | "function_expression"
                            | "generator_function"
                    ) {
                        continue;
                    }
                    let Some(body) = arg.child_by_field_name("body") else {
                        continue;
                    };
                    append_block(body, &mut bodies);
                }
            }
            _ => {}
        }
    }
    bodies
}

fn append_block<'tree>(block: Node<'tree>, bodies: &mut Vec<Node<'tree>>) {
    match block.kind() {
        "statement_block" => bodies.push(block),
        "block" => bodies.extend(
            named_children(block)
                .into_iter()
                .filter(|n| n.kind() == "statement_list"),
        ),
        _ => {}
    }
}

pub(super) fn pieces(
    start: usize,
    end: usize,
    cuts: &[usize],
) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut previous = start;
    let first = cuts.partition_point(|&offset| offset <= start);
    cuts[first..]
        .iter()
        .copied()
        .take_while(move |&offset| offset < end)
        .chain([end])
        .map(move |stop| (std::mem::replace(&mut previous, stop), stop))
}
