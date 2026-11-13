//! C/C++ preprocessor directive scan.
//!
//! The resolver needs the directives, not just the imports: whether a call
//! site spells a macro depends on what the translation unit had defined at
//! that line. Directives are recorded flat and in source order, with each
//! conditional's branches and closing marker spelled out, so the resolver can
//! replay them with a condition stack instead of re-reading the syntax tree.
//!
//! `#endif` is an anonymous token in both grammars and `#else` reaches the tree
//! as a named alternative node, so extraction synthesises the markers the
//! replay needs at the positions the source spells them.

use tree_sitter::Node;

use crate::domain::{FileFormat, PreprocessorDirective, PreprocessorKind};

use super::collector::{module_path_of, node_text};

/// Directives of one file, in source order. Only C and C++ carry a
/// preprocessor, so every other language answers nothing.
pub(crate) fn scan_preprocessor(
    root: Node<'_>,
    source: &[u8],
    format: FileFormat,
) -> Vec<PreprocessorDirective> {
    if !matches!(format, FileFormat::C | FileFormat::Cpp) {
        return Vec::new();
    }
    let mut directives = Vec::new();
    scan_block(root, source, format, &mut directives);
    directives
}

/// Records every directive of one block — the file, a function body, a
/// conditional branch — in source order.
///
/// Ordinary nodes are entered rather than skipped: `#define` written inside a
/// function body takes effect at its own line for the rest of the file, which
/// is where the preprocessor puts it too.
fn scan_block(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_child(child, source, format, out);
    }
}

/// Records one node when it is a directive and enters it otherwise: a
/// directive nested in a body is a directive of the block that body sits in.
fn scan_child(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    match node.kind() {
        "preproc_include" => push_include(node, source, format, out),
        "preproc_def" | "preproc_function_def" => push_define(node, source, out),
        "preproc_call" => push_call(node, source, out),
        "preproc_if" | "preproc_ifdef" => scan_conditional(node, source, format, out),
        "preproc_elif" => push_elif(node, source, format, out),
        "preproc_elifdef" => push_elifdef(node, source, format, out),
        "preproc_else" => push_else(node, source, format, out),
        _ => scan_block(node, source, format, out),
    }
}

/// One `#include`, with the specifier normalised the way the import rule
/// spells it: a quoted path searches the includer's directory first (`./x.h`),
/// an angle-bracket path addresses include roots this workspace cannot prove
/// (`<vector>`), which the resolver leaves unresolved.
fn push_include(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    let Some(path) = module_path_of(node, source, format) else {
        return;
    };
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Include,
        name: None,
        value: Some(path),
        line: node.start_position().row + 1,
    });
}

/// One `#define` or `#define NAME(args) body`. The body is kept verbatim: a
/// condition that tests the macro (`#if ENABLE_TRACE`) reads its value, and a
/// function-like macro defines its name the same way an object-like one does.
fn push_define(node: Node<'_>, source: &[u8], out: &mut Vec<PreprocessorDirective>) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Define,
        name: Some(node_text(name, source).to_owned()),
        value: node
            .child_by_field_name("value")
            .map(|value| node_text(value, source).trim().to_owned()),
        line: node.start_position().row + 1,
    });
}

/// `#undef` and `#pragma once` reach the tree as one generic directive node
/// whose first field is the directive token itself.
fn push_call(node: Node<'_>, source: &[u8], out: &mut Vec<PreprocessorDirective>) {
    let directive = node
        .child_by_field_name("directive")
        .map(|directive| node_text(directive, source).trim())
        .unwrap_or_default();
    let argument = node
        .child_by_field_name("argument")
        .map(|argument| node_text(argument, source).trim())
        .unwrap_or_default();
    let (kind, name) = match directive {
        "#undef" => (PreprocessorKind::Undef, Some(argument.to_owned())),
        "#pragma" if argument == "once" => (PreprocessorKind::PragmaOnce, None),
        _ => return,
    };
    out.push(PreprocessorDirective {
        kind,
        name,
        value: None,
        line: node.start_position().row + 1,
    });
}

/// A conditional's opening directive followed by its branches and the
/// synthesised closing marker.
fn scan_conditional(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    let Some(kind) = conditional_kind(node) else {
        return;
    };
    out.push(PreprocessorDirective {
        kind,
        name: guard_name(node, source),
        value: condition_text(node, source),
        line: node.start_position().row + 1,
    });
    scan_branches(node, source, format, out);
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Endif,
        name: None,
        value: None,
        line: endif_line(node),
    });
}

/// `#ifdef` and `#ifndef` share one node kind, so the opening token is what
/// tells them apart; `#if` is a node kind of its own.
fn conditional_kind(node: Node<'_>) -> Option<PreprocessorKind> {
    Some(match node.child(0)?.kind() {
        "#ifdef" => PreprocessorKind::Ifdef,
        "#ifndef" => PreprocessorKind::Ifndef,
        "#if" => PreprocessorKind::If,
        _ => return None,
    })
}

fn guard_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source).to_owned())
}

fn condition_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("condition")
        .map(|condition| node_text(condition, source).trim().to_owned())
}

/// A conditional's own branch plus the `#elif`/`#else` chain hanging off its
/// alternative field. The guard name, the condition and the opening token are
/// not branches, so they are skipped.
fn scan_branches(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    let name = node.child_by_field_name("name");
    let condition = node.child_by_field_name("condition");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if Some(child) == name || Some(child) == condition {
            continue;
        }
        scan_child(child, source, format, out);
    }
}

fn push_elif(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Elif,
        name: None,
        value: condition_text(node, source),
        line: node.start_position().row + 1,
    });
    scan_branches(node, source, format, out);
}

/// `#elifdef`/`#elifndef`: the opening token says which of the two the source
/// spelled, and it travels in `value` so the replay can negate it.
fn push_elifdef(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Elifdef,
        name: guard_name(node, source),
        value: node
            .child(0)
            .map(|opener| node_text(opener, source).trim().to_owned()),
        line: node.start_position().row + 1,
    });
    scan_branches(node, source, format, out);
}

fn push_else(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    out: &mut Vec<PreprocessorDirective>,
) {
    out.push(PreprocessorDirective {
        kind: PreprocessorKind::Else,
        name: None,
        value: None,
        line: node.start_position().row + 1,
    });
    scan_branches(node, source, format, out);
}

/// The line `#endif` stands on. The token is anonymous, so it has to be found
/// among the conditional's children; a block the grammar recovered without one
/// closes at the node's own end instead.
fn endif_line(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    let line = node
        .children(&mut cursor)
        .find(|child| child.kind() == "#endif")
        .map_or_else(
            || node.end_position().row,
            |endif| endif.start_position().row,
        );
    line + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn scan(source: &str, format: FileFormat) -> Vec<PreprocessorDirective> {
        let language = match format {
            FileFormat::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            _ => tree_sitter_c::LANGUAGE.into(),
        };
        let mut parser = Parser::new();
        parser.set_language(&language).expect("c grammar");
        let tree = parser.parse(source, None).expect("parses");
        scan_preprocessor(tree.root_node(), source.as_bytes(), format)
    }

    fn spelled(directives: &[PreprocessorDirective]) -> Vec<(PreprocessorKind, String, usize)> {
        directives
            .iter()
            .map(|directive| {
                let operand = directive
                    .name
                    .clone()
                    .or_else(|| directive.value.clone())
                    .unwrap_or_default();
                (directive.kind, operand, directive.line)
            })
            .collect()
    }

    /// Every branch boundary is spelled out, because the replay needs the
    /// block structure and `#endif` is only a token in the grammar.
    #[test]
    fn records_conditionals_with_their_branches_and_closing_marker() {
        let directives = scan(
            concat!(
                "#define A 1\n",
                "#include \"x.h\"\n",
                "#if 1\n",
                "#undef B\n",
                "#else\n",
                "#define C 2\n",
                "#endif\n",
                "#pragma once\n",
            ),
            FileFormat::C,
        );
        assert_eq!(
            spelled(&directives),
            vec![
                (PreprocessorKind::Define, "A".to_owned(), 1),
                (PreprocessorKind::Include, "./x.h".to_owned(), 2),
                (PreprocessorKind::If, "1".to_owned(), 3),
                (PreprocessorKind::Undef, "B".to_owned(), 4),
                (PreprocessorKind::Else, String::new(), 5),
                (PreprocessorKind::Define, "C".to_owned(), 6),
                (PreprocessorKind::Endif, String::new(), 7),
                (PreprocessorKind::PragmaOnce, String::new(), 8),
            ]
        );
    }

    /// The opening token distinguishes the two guards, and `#elif` continues
    /// the same block instead of opening a nested one.
    #[test]
    fn distinguishes_guards_and_continues_elif_chains() {
        let directives = scan(
            concat!(
                "#ifndef GUARD\n",
                "#define GUARD\n",
                "#elif defined(OTHER)\n",
                "#define C\n",
                "#else\n",
                "#define D\n",
                "#endif\n",
                "#ifdef ON\n",
                "#endif\n",
            ),
            FileFormat::Cpp,
        );
        assert_eq!(
            spelled(&directives),
            vec![
                (PreprocessorKind::Ifndef, "GUARD".to_owned(), 1),
                (PreprocessorKind::Define, "GUARD".to_owned(), 2),
                (PreprocessorKind::Elif, "defined(OTHER)".to_owned(), 3),
                (PreprocessorKind::Define, "C".to_owned(), 4),
                (PreprocessorKind::Else, String::new(), 5),
                (PreprocessorKind::Define, "D".to_owned(), 6),
                (PreprocessorKind::Endif, String::new(), 7),
                (PreprocessorKind::Ifdef, "ON".to_owned(), 8),
                (PreprocessorKind::Endif, String::new(), 9),
            ]
        );
    }

    /// A `#define` written inside a function body takes effect at its own line
    /// for the rest of the file, so the scan must not stop at entity bounds.
    #[test]
    fn records_directives_written_inside_a_body() {
        let directives = scan(
            concat!("void f() {\n", "#define INNER 1\n", "  INNER;\n", "}\n",),
            FileFormat::C,
        );
        assert_eq!(
            spelled(&directives),
            vec![(PreprocessorKind::Define, "INNER".to_owned(), 2)]
        );
    }

    /// Angle-bracket includes address roots the workspace cannot prove, so
    /// they keep their form; a quoted one searches the includer first.
    #[test]
    fn normalizes_both_include_forms() {
        let directives = scan("#include <vector>\n#include \"a/b.h\"\n", FileFormat::Cpp);
        let spelled: Vec<&str> = directives
            .iter()
            .filter_map(|directive| directive.value.as_deref())
            .collect();
        assert_eq!(spelled, vec!["<vector>", "./a/b.h"]);
    }

    /// The scan is C/C++ only: another language's `#` is not a directive.
    #[test]
    fn ignores_languages_without_a_preprocessor() {
        assert!(scan("#define A 1\n", FileFormat::Rust).is_empty());
    }
}
