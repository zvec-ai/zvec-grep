//! Searchable source ranges for statements outside named symbols.

use tree_sitter::{Node, Parser};

use super::{
    adapter::named_children, append_code_entity, code_node_range, code_node_span, fallback,
};
use crate::{
    domain::{FileFormat, Range, TextRange},
    extraction::{ExtractedEntity, TextSource},
    utils::line_byte_offsets,
};

pub(super) fn extract_symbol_free_file(
    source: &TextSource,
    root: Node<'_>,
    format: FileFormat,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ExtractedEntity> {
    let plain = fallback(source, max_chars, overlap_chars);
    if plain.len() <= 1 {
        return plain;
    }
    if root.has_error() {
        // Go inserts a semicolon at EOF. Without a trailing newline its
        // grammar can report a missing separator after a valid const block.
        // Check with the terminator, then use only original source ranges.
        if format != FileFormat::Go || source.text.ends_with('\n') {
            return plain;
        }
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .is_err()
            || parser
                .parse(format!("{}\n", source.text), None)
                .is_none_or(|tree| tree.root_node().has_error())
        {
            return plain;
        }
    }
    // Both streams are in source order. The first chunk ending after a
    // statement's end also has the earliest possible covering start.
    let chunks: Vec<_> = plain
        .iter()
        .map(|entity| {
            let Range::Text(range) = entity.source_range else {
                unreachable!("plain text chunks have source coordinates")
            };
            (range.start_byte_offset(), range.end_byte_offset())
        })
        .collect();
    let mut candidate = 0;
    let mut split = Vec::new();
    for node in named_children(root) {
        if !is_searchable_statement(node, format) || node.start_byte() == node.end_byte() {
            continue;
        }
        while candidate < chunks.len() && chunks[candidate].1 < node.end_byte() {
            candidate += 1;
        }
        if candidate == chunks.len() || chunks[candidate].0 > node.start_byte() {
            split.push((node.start_byte(), node.end_byte()));
        }
    }
    if split.is_empty() {
        return plain;
    }
    let offsets = line_byte_offsets(&source.text);
    let ranges = restore_statement_ranges(&source.text, &chunks, &split);
    let mut output = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        let range = crate::utils::text_range_from_offsets(&source.text, &offsets, start, end)
            .expect("chunk and statement offsets come from the source");
        append_code_entity(source, range, None, max_chars, overlap_chars, &mut output);
    }
    output
}

// Subtract the recovered statements from the old chunks. This preserves
// the chunk boundaries that already covered every other statement.
fn restore_statement_ranges(
    source: &str,
    chunks: &[(usize, usize)],
    split: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let mut ranges = split.to_vec();
    for &(mut start, end) in chunks {
        let first = split.partition_point(|&(_, stop)| stop <= start);
        if first == split.len() || split[first].0 >= end {
            ranges.push((start, end));
            continue;
        }
        let mut keep_remainder = |from: usize, to: usize| {
            let slice = &source[from..to];
            let text = slice.trim();
            if !text.is_empty() {
                let from = to - slice.trim_start().len();
                ranges.push((from, from + text.len()));
            }
        };
        for &(from, to) in &split[first..] {
            if from >= end {
                break;
            }
            if start < from {
                keep_remainder(start, from);
            }
            start = start.max(to);
        }
        if start < end {
            keep_remainder(start, end);
        }
    }
    // Overlapping old chunks can leave the same suffix/prefix twice. Emit
    // maximal ranges in source order, preserving the configured text overlap.
    ranges.sort_unstable_by_key(|&(start, end)| (start, std::cmp::Reverse(end)));
    let mut covered_end = 0;
    ranges.retain(|&(start, end)| {
        let keep = end > covered_end
            && source[start..end]
                .chars()
                .any(|ch| ch != ';' && !ch.is_whitespace());
        covered_end = covered_end.max(end);
        keep
    });
    ranges
}

pub(super) fn append_statement_gaps(
    source: &TextSource,
    root: Node<'_>,
    format: FileFormat,
    gaps: &[(usize, usize)],
    (max_chars, overlap_chars): (usize, usize),
    output: &mut Vec<ExtractedEntity>,
) {
    let mut line_offsets = None;
    let mut statements = named_children(root)
        .into_iter()
        .filter(|&node| is_searchable_statement(node, format))
        .peekable();
    while let Some(statement) = statements.next() {
        let first = gaps.partition_point(|&(_, end)| end <= statement.start_byte());
        for &(gap_start, gap_end) in gaps[first..]
            .iter()
            .take_while(|&&(start, _)| start < statement.end_byte())
        {
            let start = gap_start.max(statement.start_byte());
            let end = gap_end.min(statement.end_byte());
            let range = if start == statement.start_byte() && end == statement.end_byte() {
                adjacent_statement_range(
                    statement,
                    &mut statements,
                    &source.text,
                    gap_end,
                    max_chars,
                )
            } else {
                // An inner symbol may leave a condition, call, or object property
                // outside its range. Bare declaration keywords and delimiters
                // contain no complete named syntax of their own.
                let keep_callback_comments =
                    matches!(format, FileFormat::JavaScript | FileFormat::TypeScript);
                if !contains_named_code(statement, start, end, &source.text, keep_callback_comments)
                {
                    continue;
                }
                let gap = &source.text[start..end];
                let start = end - gap.trim_start().len();
                let end = start + gap.trim().len();
                let offsets = line_offsets.get_or_insert_with(|| line_byte_offsets(&source.text));
                crate::utils::text_range_from_offsets(&source.text, offsets, start, end)
                    .expect("statement gap refers to source text")
            };
            append_code_entity(source, range, None, max_chars, overlap_chars, output);
        }
    }
}

fn adjacent_statement_range<'tree>(
    first: Node<'tree>,
    statements: &mut std::iter::Peekable<impl Iterator<Item = Node<'tree>>>,
    text: &str,
    gap_end: usize,
    max_chars: usize,
) -> TextRange {
    if first.start_position().row != first.end_position().row {
        return code_node_range(first);
    }
    let mut used = crate::utils::utf16_len(&text[first.byte_range()]);
    let mut last = first;
    while used < max_chars
        && let Some(&next) = statements.peek()
    {
        if next.end_byte() > gap_end
            || next.start_position().row != next.end_position().row
            || next.start_position().row > last.end_position().row + 1
            || !text[last.end_byte()..next.start_byte()]
                .chars()
                .all(char::is_whitespace)
        {
            break;
        }
        // Charge only the new statement and its separator, never the accumulated span.
        let additional = crate::utils::utf16_len(&text[last.end_byte()..next.end_byte()]);
        if additional > max_chars - used {
            break;
        }
        used += additional;
        last = statements.next().expect("peeked statement");
    }
    code_node_span(first, last)
}

fn contains_named_code(
    node: Node<'_>,
    start: usize,
    end: usize,
    text: &str,
    keep_callback_comments: bool,
) -> bool {
    let mut child = node.first_named_child_for_byte(start);
    while let Some(current) = child.filter(|child| child.start_byte() < end) {
        // The previous JS/TS callback chunks included comments left outside
        // named functions. Attached documentation is already outside the gap.
        let callback_comment = keep_callback_comments
            && current.kind() == "comment"
            && std::iter::successors(current.parent(), Node::parent).any(|ancestor| {
                matches!(
                    ancestor.kind(),
                    "arrow_function"
                        | "function_expression"
                        | "generator_function"
                        | "method_definition"
                )
            });
        if (!current.is_extra() || callback_comment) && !current.is_missing() {
            if start <= current.start_byte() && current.end_byte() <= end {
                if text[current.byte_range()]
                    .chars()
                    .any(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$'))
                {
                    return true;
                }
            } else if contains_named_code(current, start, end, text, keep_callback_comments) {
                return true;
            }
        }
        child = current.next_named_sibling();
    }
    false
}

fn is_searchable_statement(node: Node<'_>, format: FileFormat) -> bool {
    let kind = node.kind();
    match format {
        FileFormat::Go => matches!(kind, "const_declaration" | "var_declaration"),
        // In a compact source file a field outside a class is parsed as a
        // local declaration next to the entry method.
        FileFormat::Java => kind == "local_variable_declaration",
        FileFormat::Rust => {
            matches!(
                kind,
                "macro_definition" | "macro_invocation" | "foreign_mod_item"
            ) || (kind == "extern_crate_declaration" && node.child_by_field_name("alias").is_some())
                || (kind == "expression_statement"
                    && node
                        .named_child(0)
                        .is_some_and(|child| child.kind() == "macro_invocation"))
        }
        FileFormat::JavaScript | FileFormat::TypeScript => {
            matches!(
                kind,
                "statement_block"
                    | "lexical_declaration"
                    | "variable_declaration"
                    | "using_declaration"
                    | "ambient_declaration"
                    | "import_alias"
            ) || (kind == "export_statement"
                && (node.child_by_field_name("declaration").is_some()
                    || node.child_by_field_name("value").is_some()
                    // TypeScript's `export = expression` has no value field.
                    || node.children(&mut node.walk()).any(|child| child.kind() == "=")))
                || (kind.ends_with("_statement")
                    && !matches!(
                        kind,
                        "export_statement" | "import_statement" | "empty_statement"
                    ))
        }
        FileFormat::Python => {
            kind.ends_with("_statement")
                && !matches!(
                    kind,
                    "import_statement"
                        | "import_from_statement"
                        | "future_import_statement"
                        | "pass_statement"
                )
        }
        _ => false,
    }
}
