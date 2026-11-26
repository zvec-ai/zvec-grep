use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Default)]
struct Uses {
    declaration: usize,
    forwarded: bool,
    ambiguous: bool,
}

/// Skip only a table whose sole references are direct class attributes. A
/// missing local read may mean another module imports it. Any same-spelling
/// local binding, other reference, or explicitly quoted name keeps the table.
pub(super) fn filter<'tree>(
    tables: &mut Vec<(Node<'tree>, Node<'tree>)>,
    root: Node<'tree>,
    source: &str,
) {
    if tables.is_empty() || root.has_error() {
        return;
    }
    let mut uses = HashMap::new();
    for &(_, name) in tables.iter() {
        uses.insert(
            &source[name.byte_range()],
            Uses {
                declaration: name.id(),
                ..Uses::default()
            },
        );
    }
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "wildcard_import" {
            return;
        }
        if matches!(node.kind(), "identifier" | "string_content")
            && let Some(usage) = uses.get_mut(&source[node.byte_range()])
            && !usage.ambiguous
            && node.id() != usage.declaration
        {
            if node.kind() == "identifier" && is_class_attribute_input(node) {
                usage.forwarded = true;
            } else {
                usage.ambiguous = true;
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                tables.retain(|(_, name)| {
                    let usage = &uses[&source[name.byte_range()]];
                    usage.ambiguous || !usage.forwarded
                });
                return;
            }
        }
    }
}

fn outside_parentheses(mut node: Node<'_>) -> Node<'_> {
    while let Some(wrapper) = node
        .parent()
        .filter(|n| n.kind() == "parenthesized_expression")
    {
        node = wrapper;
    }
    node
}

fn is_class_attribute_input(name: Node<'_>) -> bool {
    let mut value = outside_parentheses(name);
    if let Some(splat) = value.parent().filter(|p| p.kind() == "dictionary_splat") {
        let Some(dict) = splat.parent().filter(|p| p.kind() == "dictionary") else {
            return false;
        };
        value = outside_parentheses(dict);
    }
    let Some(assignment) = value.parent() else {
        return false;
    };
    if assignment.kind() != "assignment"
        || assignment.child_by_field_name("right") != Some(value)
        || assignment
            .child_by_field_name("left")
            .is_none_or(|left| left.kind() != "identifier")
    {
        return false;
    }
    assignment
        .parent()
        .filter(|n| n.kind() == "expression_statement")
        .and_then(|n| n.parent())
        .filter(|n| n.kind() == "block")
        .and_then(|n| n.parent())
        .is_some_and(|n| n.kind() == "class_definition")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn kept(body: &str) -> Vec<String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("Python grammar");
        let tree = parser.parse(body, None).expect("Python tree");
        let root = tree.root_node();
        assert!(!root.has_error());
        let mut candidates: Vec<_> = root
            .named_children(&mut root.walk())
            .filter_map(|stmt| super::super::python_table_binding(stmt).map(|name| (stmt, name)))
            .collect();
        filter(&mut candidates, root, body);
        candidates
            .iter()
            .map(|(_, name)| body[name.byte_range()].to_owned())
            .collect()
    }

    #[test]
    fn only_direct_class_forwarding_suppresses_a_table() {
        let body = "Direct = {1: 2}\nSpread = {1: 2}\nExternal = {1: 2}\nMixed = {1: 2}\n\
            class Owner:\n    one = (Direct)\n    two: dict = {**(Spread), **Mixed}\n\
            def lookup(): return Mixed[1]\n";
        assert_eq!(kept(body), ["External", "Mixed"]);
    }

    #[test]
    fn rebinding_or_an_indirect_use_keeps_the_table() {
        for other in [
            "def shadow(Table): pass",
            "Table = {3: 4}",
            "__all__ = ['Table']",
            "from provider import *",
            "def read(): return forward(Table)",
            "class Other:\n    derived = {'key': Table}",
            "class Other:\n    if enabled: alias = Table",
        ] {
            let body = format!("Table = {{1: 2}}\nclass Owner:\n    alias = Table\n{other}\n");
            assert!(kept(&body).iter().any(|name| name == "Table"), "{other}");
        }
    }
}
