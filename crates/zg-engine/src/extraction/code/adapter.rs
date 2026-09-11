use tree_sitter::Node;

use crate::domain::{FileFormat, SymbolType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdapterKind {
    C,
    Cpp,
    Go,
    Java,
    JavaScript,
    Python,
    Rust,
    TypeScript,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LanguageAdapter {
    kind: AdapterKind,
    entity_types: &'static [&'static str],
    scope_types: &'static [&'static str],
}

const C: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::C,
    entity_types: &[
        "declaration",
        "field_declaration",
        "function_definition",
        "macro_type_specifier",
        "struct_specifier",
        "union_specifier",
        "enum_specifier",
        "type_definition",
    ],
    scope_types: &["struct_specifier", "union_specifier"],
};
const CPP: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::Cpp,
    entity_types: &[
        "alias_declaration",
        "declaration",
        "field_declaration",
        "function_definition",
        "macro_type_specifier",
        "class_specifier",
        "struct_specifier",
        "union_specifier",
        "enum_specifier",
    ],
    scope_types: &[
        "namespace_definition",
        "class_specifier",
        "struct_specifier",
        "union_specifier",
    ],
};
const GO: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::Go,
    entity_types: &[
        "function_declaration",
        "method_elem",
        "method_spec",
        "method_declaration",
        "type_alias",
        "type_spec",
    ],
    scope_types: &["type_spec"],
};
const JAVA: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::Java,
    entity_types: &[
        "annotation_type_declaration",
        "class_declaration",
        "constructor_declaration",
        "enum_declaration",
        "interface_declaration",
        "method_declaration",
        "record_declaration",
    ],
    scope_types: &[
        "annotation_type_declaration",
        "class_declaration",
        "enum_declaration",
        "interface_declaration",
        "record_declaration",
    ],
};
const JAVASCRIPT: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::JavaScript,
    entity_types: &[
        "class_declaration",
        "field_definition",
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
        "pair",
        "variable_declarator",
    ],
    scope_types: &["class_declaration"],
};
const PYTHON: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::Python,
    entity_types: &[
        "class_definition",
        "decorated_definition",
        "function_definition",
    ],
    scope_types: &["class_definition", "decorated_definition"],
};
const RUST: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::Rust,
    entity_types: &[
        "enum_item",
        "function_item",
        "function_signature_item",
        "impl_item",
        "struct_item",
        "trait_item",
        "type_item",
        "union_item",
    ],
    scope_types: &["impl_item", "mod_item", "trait_item"],
};
const TYPESCRIPT: LanguageAdapter = LanguageAdapter {
    kind: AdapterKind::TypeScript,
    entity_types: &[
        "abstract_class_declaration",
        "abstract_method_signature",
        "class_declaration",
        "enum_declaration",
        "field_definition",
        "function_declaration",
        "generator_function_declaration",
        "interface_declaration",
        "method_signature",
        "method_definition",
        "pair",
        "public_field_definition",
        "type_alias_declaration",
        "variable_declarator",
    ],
    scope_types: &[
        "abstract_class_declaration",
        "class_declaration",
        "internal_module",
        "interface_declaration",
        "module_declaration",
        "namespace_declaration",
    ],
};

pub(super) fn resolve_adapter(format: FileFormat) -> Option<&'static LanguageAdapter> {
    match format {
        FileFormat::C => Some(&C),
        FileFormat::Cpp => Some(&CPP),
        FileFormat::Go => Some(&GO),
        FileFormat::Java => Some(&JAVA),
        FileFormat::JavaScript => Some(&JAVASCRIPT),
        FileFormat::Python => Some(&PYTHON),
        FileFormat::Rust => Some(&RUST),
        FileFormat::TypeScript => Some(&TYPESCRIPT),
        _ => None,
    }
}

impl LanguageAdapter {
    pub(super) fn is_entity(self, node: Node<'_>) -> bool {
        self.entity_types.contains(&node.kind()) && self.should_index_entity(node)
    }

    pub(super) fn is_scope(self, node: Node<'_>) -> bool {
        self.scope_types.contains(&node.kind()) && self.should_enter_scope(node)
    }

    pub(super) fn resolve_entities<'tree>(
        self,
        node: Node<'tree>,
        source: &[u8],
    ) -> Vec<Node<'tree>> {
        if matches!(self.kind, AdapterKind::JavaScript | AdapterKind::TypeScript)
            && node.kind() == "variable_declarator"
        {
            let entities = exported_object_function_entities(node, source);
            if !entities.is_empty() {
                return entities;
            }
        }
        vec![node]
    }

    pub(super) fn enter_scope_node(self, node: Node<'_>) -> Node<'_> {
        match self.kind {
            AdapterKind::Go => node.child_by_field_name("type").unwrap_or(node),
            AdapterKind::Python if node.kind() == "decorated_definition" => {
                inner_python_definition(node)
                    .filter(|inner| inner.kind() == "class_definition")
                    .unwrap_or(node)
            }
            _ => node,
        }
    }

    pub(super) fn extract_name(self, node: Node<'_>, source: &[u8]) -> Option<String> {
        match self.kind {
            AdapterKind::C | AdapterKind::Cpp => extract_c_family_name(node, source),
            AdapterKind::Go => field_text(node, "name", source),
            AdapterKind::Java => name_field(node, source),
            AdapterKind::JavaScript | AdapterKind::TypeScript => {
                extract_javascript_typescript_name(node, source)
            }
            AdapterKind::Python => {
                if node.kind() == "decorated_definition" {
                    inner_python_definition(node)
                        .and_then(|inner| field_text(inner, "name", source))
                } else {
                    field_text(node, "name", source)
                }
            }
            AdapterKind::Rust => {
                if node.kind() == "impl_item" {
                    field_text(node, "type", source)
                } else {
                    field_text(node, "name", source)
                }
            }
        }
    }

    pub(super) fn scope_breadcrumb(
        self,
        node: Node<'_>,
        source: &[u8],
        breadcrumb: &[String],
    ) -> Vec<String> {
        match self.kind {
            AdapterKind::C | AdapterKind::Cpp if is_c_function_type(node.kind()) => {
                let Some(name) = extract_raw_c_function_name(node, source) else {
                    return breadcrumb.to_vec();
                };
                let mut qualifier = name
                    .split("::")
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                qualifier.pop();
                if qualifier.is_empty() {
                    return breadcrumb.to_vec();
                }
                if breadcrumb.last() == qualifier.first() {
                    qualifier.remove(0);
                }
                breadcrumb.iter().cloned().chain(qualifier).collect()
            }
            AdapterKind::Go if node.kind() == "method_declaration" => {
                let Some(receiver) = node.child_by_field_name("receiver") else {
                    return breadcrumb.to_vec();
                };
                let Some(receiver_type) = text(receiver, source)
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .rfind(|part| !part.is_empty())
                else {
                    return breadcrumb.to_vec();
                };
                breadcrumb
                    .iter()
                    .cloned()
                    .chain([receiver_type.to_owned()])
                    .collect()
            }
            AdapterKind::JavaScript | AdapterKind::TypeScript => {
                let Some(object_name) = exported_object_variable_name(node, source) else {
                    return breadcrumb.to_vec();
                };
                breadcrumb.iter().cloned().chain([object_name]).collect()
            }
            _ => breadcrumb.to_vec(),
        }
    }

    pub(super) fn classify(
        self,
        node: Node<'_>,
        source: &[u8],
        breadcrumb: &[String],
    ) -> SymbolType {
        let specialized = match self.kind {
            AdapterKind::C | AdapterKind::Cpp => classify_c_family(node),
            AdapterKind::Go => classify_go(node),
            AdapterKind::JavaScript | AdapterKind::TypeScript => {
                classify_javascript_typescript(node, source)
            }
            _ => None,
        };
        specialized.unwrap_or_else(|| classify_code_node(node, breadcrumb))
    }

    pub(super) fn extract_signature(self, node: Node<'_>, source: &[u8]) -> Option<String> {
        match self.kind {
            AdapterKind::JavaScript | AdapterKind::TypeScript if node.kind() == "pair" => {
                let key = extract_javascript_typescript_name(node, source);
                let value = node.child_by_field_name("value");
                match (
                    key,
                    value.and_then(|item| extract_generic_signature(item, source)),
                ) {
                    (Some(key), Some(signature)) => Some(format!("{key}: {signature}")),
                    _ => extract_generic_signature(node, source),
                }
            }
            AdapterKind::Python => {
                extract_generic_signature(inner_python_definition(node).unwrap_or(node), source)
            }
            _ => extract_generic_signature(node, source),
        }
    }

    pub(super) fn extract_doc(node: Node<'_>, source: &[u8]) -> Option<String> {
        extract_preceding_doc(node, source)
    }

    pub(super) fn extract_modifiers(self, node: Node<'_>, source: &[u8]) -> Vec<String> {
        match self.kind {
            AdapterKind::Go => field_text(node, "name", source)
                .filter(|name| name.chars().next().is_some_and(char::is_uppercase))
                .map_or_else(Vec::new, |_| vec!["exported".to_owned()]),
            AdapterKind::Python => {
                let mut modifiers = extract_common_modifiers(node, source);
                let node_text = text(node, source);
                if node_text
                    .lines()
                    .any(|line| line.trim_start().starts_with("async def "))
                {
                    push_unique(&mut modifiers, "async");
                }
                if node_text
                    .lines()
                    .any(|line| line.trim_start().starts_with("@staticmethod"))
                {
                    push_unique(&mut modifiers, "static");
                }
                modifiers
            }
            _ => extract_common_modifiers(node, source),
        }
    }

    fn should_index_entity(self, node: Node<'_>) -> bool {
        match self.kind {
            AdapterKind::C | AdapterKind::Cpp => {
                if matches!(node.kind(), "declaration" | "field_declaration") {
                    find_descendant_by_kind(node, "function_declarator").is_some()
                } else if node.kind() == "macro_type_specifier" {
                    // The text check is repeated by name extraction; keeping the node here
                    // lets malformed macro function types fall out when no name is found.
                    true
                } else {
                    true
                }
            }
            AdapterKind::JavaScript | AdapterKind::TypeScript => {
                should_index_javascript_typescript_entity(node)
            }
            _ => true,
        }
    }

    fn should_enter_scope(self, node: Node<'_>) -> bool {
        match self.kind {
            AdapterKind::Go if node.kind() == "type_spec" => node
                .child_by_field_name("type")
                .is_some_and(|kind| matches!(kind.kind(), "interface_type" | "struct_type")),
            AdapterKind::Python if node.kind() == "decorated_definition" => {
                inner_python_definition(node)
                    .is_some_and(|inner| inner.kind() == "class_definition")
            }
            _ => true,
        }
    }
}

pub(super) fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub(super) fn text<'source>(node: Node<'_>, source: &'source [u8]) -> &'source str {
    node.utf8_text(source).unwrap_or_default()
}

fn field_text(node: Node<'_>, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .map(|child| text(child, source).to_owned())
}

fn name_field(node: Node<'_>, source: &[u8]) -> Option<String> {
    field_text(node, "name", source).or_else(|| {
        named_children(node)
            .into_iter()
            .find(|child| {
                matches!(
                    child.kind(),
                    "identifier" | "property_identifier" | "type_identifier"
                )
            })
            .map(|child| text(child, source).to_owned())
    })
}

fn is_c_function_type(kind: &str) -> bool {
    matches!(
        kind,
        "declaration" | "field_declaration" | "function_definition" | "macro_type_specifier"
    )
}

fn extract_c_family_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if is_c_function_type(node.kind()) {
        return extract_raw_c_function_name(node, source)
            .map(|name| name.rsplit("::").next().unwrap_or(&name).to_owned());
    }
    if let Some(name) = node.child_by_field_name("name") {
        return Some(text(find_identifier_leaf(name).unwrap_or(name), source).to_owned());
    }
    if node.kind() == "type_definition" {
        return node
            .child_by_field_name("declarator")
            .and_then(find_identifier_leaf)
            .map(|name| text(name, source).to_owned());
    }
    None
}

fn extract_raw_c_function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let declarator = node
        .child_by_field_name("declarator")
        .or_else(|| find_descendant_by_kind(node, "function_declarator"));
    if let Some(name) = declarator.and_then(find_identifier_leaf) {
        let name = text(name, source);
        if is_simple_c_identifier(name) {
            return Some(name.to_owned());
        }
    }
    extract_c_function_name(
        declarator.map_or_else(|| text(node, source), |item| text(item, source)),
    )
}

fn find_identifier_leaf(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..16 {
        if node.kind() == "identifier"
            || node.kind().ends_with("_identifier")
            || matches!(node.kind(), "destructor_name" | "operator_name")
        {
            return Some(node);
        }
        if matches!(
            node.kind(),
            "array_declarator"
                | "function_declarator"
                | "init_declarator"
                | "parenthesized_declarator"
                | "pointer_declarator"
                | "reference_declarator"
        ) {
            node = node.child_by_field_name("declarator")?;
        } else {
            return None;
        }
    }
    None
}

fn extract_c_function_name(value: &str) -> Option<String> {
    let mut last = None;
    for (index, character) in value.char_indices() {
        if character != '(' {
            continue;
        }
        let prefix = value[..index].trim_end();
        let start = prefix
            .char_indices()
            .rev()
            .find(|(_, item)| !item.is_ascii_alphanumeric() && !matches!(item, '_' | '~' | ':'))
            .map_or(0, |(offset, item)| offset + item.len_utf8());
        let candidate = &prefix[start..];
        if is_simple_c_identifier(candidate) {
            last = Some(candidate.to_owned());
        }
    }
    last
}

fn is_simple_c_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.split("::").all(|part| {
            let part = part.strip_prefix('~').unwrap_or(part);
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
                && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
}

fn classify_c_family(node: Node<'_>) -> Option<SymbolType> {
    match node.kind() {
        "type_definition" => Some(
            if ["struct_specifier", "union_specifier", "enum_specifier"]
                .into_iter()
                .any(|kind| {
                    find_descendant_by_kind(node, kind)
                        .is_some_and(|item| item.child_by_field_name("body").is_some())
                })
            {
                SymbolType::Class
            } else {
                SymbolType::Alias
            },
        ),
        "alias_declaration" => Some(SymbolType::Alias),
        "field_declaration" if find_descendant_by_kind(node, "function_declarator").is_some() => {
            Some(SymbolType::Function)
        }
        _ => None,
    }
}

fn classify_go(node: Node<'_>) -> Option<SymbolType> {
    match node.kind() {
        "type_alias" => Some(SymbolType::Alias),
        "type_spec" => Some(
            match node.child_by_field_name("type").map(|item| item.kind()) {
                Some("interface_type") => SymbolType::Interface,
                Some("struct_type") => SymbolType::Class,
                _ => SymbolType::Alias,
            },
        ),
        "method_elem" | "method_spec" => Some(SymbolType::Function),
        _ => None,
    }
}

fn should_index_javascript_typescript_entity(node: Node<'_>) -> bool {
    if node.kind() == "method_definition"
        && node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "object" | "object_expression"))
    {
        return false;
    }
    if node.kind() == "pair" {
        return has_function_value(node) && exported_object_variable(node).is_some();
    }
    if !matches!(
        node.kind(),
        "field_definition" | "public_field_definition" | "variable_declarator"
    ) {
        return true;
    }
    if node.kind() == "variable_declarator"
        && !exported_object_function_entities(node, &[]).is_empty()
    {
        return true;
    }
    has_function_value(node)
}

fn has_javascript_typescript_function_value(node: Node<'_>) -> bool {
    has_function_value(node)
}

fn has_function_value(node: Node<'_>) -> bool {
    let value = node.child_by_field_name("value").or_else(|| {
        named_children(node)
            .into_iter()
            .find(|child| matches!(child.kind(), "arrow_function" | "function_expression"))
    });
    value.is_some_and(contains_function_value)
}

fn contains_function_value(node: Node<'_>) -> bool {
    if matches!(node.kind(), "arrow_function" | "function_expression") {
        return true;
    }
    if !matches!(node.kind(), "call_expression" | "arguments") {
        return false;
    }
    named_children(node)
        .into_iter()
        .any(contains_function_value)
}

fn exported_object_function_entities<'tree>(node: Node<'tree>, _source: &[u8]) -> Vec<Node<'tree>> {
    if node.kind() != "variable_declarator" || closest_ancestor(node, "export_statement").is_none()
    {
        return Vec::new();
    }
    let Some(value) = node.child_by_field_name("value") else {
        return Vec::new();
    };
    if !matches!(value.kind(), "object" | "object_expression") {
        return Vec::new();
    }
    named_children(value)
        .into_iter()
        .filter(|child| {
            (child.kind() == "pair" && has_function_value(*child))
                || child.kind() == "method_definition"
        })
        .collect()
}

fn exported_object_variable(node: Node<'_>) -> Option<Node<'_>> {
    let object =
        closest_ancestor(node, "object").or_else(|| closest_ancestor(node, "object_expression"))?;
    let variable = object.parent()?;
    (variable.kind() == "variable_declarator"
        && closest_ancestor(variable, "export_statement").is_some())
    .then_some(variable)
}

fn exported_object_variable_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    exported_object_variable(node)
        .and_then(|variable| extract_javascript_typescript_name(variable, source))
}

fn extract_javascript_typescript_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "pair" {
        return field_text(node, "key", source)
            .map(|value| value.trim_matches(['\'', '"', '`']).to_owned());
    }
    name_field(node, source)
}

fn classify_javascript_typescript(node: Node<'_>, _source: &[u8]) -> Option<SymbolType> {
    if node.kind() == "pair"
        || matches!(
            node.kind(),
            "field_definition" | "public_field_definition" | "variable_declarator"
        )
    {
        return has_function_value(node).then_some(SymbolType::Function);
    }
    None
}

fn inner_python_definition(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "decorated_definition" {
        return None;
    }
    named_children(node)
        .into_iter()
        .find(|child| matches!(child.kind(), "function_definition" | "class_definition"))
}

fn extract_generic_signature(node: Node<'_>, source: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body").or_else(|| {
        named_children(node).into_iter().find(|child| {
            matches!(
                child.kind(),
                "statement_block"
                    | "compound_statement"
                    | "block"
                    | "class_body"
                    | "declaration_list"
                    | "field_declaration_list"
            )
        })
    });
    let node_text = text(node, source);
    let raw = body.map_or_else(
        || first_non_empty_line(node_text).to_owned(),
        |body| {
            let relative = body.start_byte().saturating_sub(node.start_byte());
            node_text[..relative].trim_end().to_owned()
        },
    );
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalized.trim_end_matches(['{', ';']).trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn extract_preceding_doc(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut comments = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(item) = sibling {
        if !matches!(
            item.kind(),
            "comment" | "line_comment" | "block_comment" | "documentation_comment"
        ) {
            break;
        }
        comments.push(clean_comment_text(text(item, source)));
        sibling = item.prev_named_sibling();
    }
    comments.reverse();
    let doc = comments.join("\n").trim().to_owned();
    (!doc.is_empty()).then_some(doc)
}

fn clean_comment_text(value: &str) -> String {
    let stripped = value
        .strip_prefix("/**")
        .or_else(|| value.strip_prefix("/*"))
        .unwrap_or(value);
    let stripped = stripped.strip_suffix("*/").unwrap_or(stripped);
    stripped
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("//") {
                rest.strip_prefix('/').unwrap_or(rest).trim_start()
            } else if let Some(rest) = trimmed.strip_prefix('#') {
                rest.trim_start()
            } else if let Some(rest) = trimmed.strip_prefix('*') {
                rest.trim_start()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn extract_common_modifiers(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut modifiers = Vec::new();
    if closest_ancestor(node, "export_statement").is_some() {
        push_unique(&mut modifiers, "exported");
    }
    let signature = extract_generic_signature(node, source)
        .unwrap_or_else(|| first_non_empty_line(text(node, source)).to_owned());
    for token in
        signature.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
    {
        match token {
            "public" | "private" | "protected" | "internal" | "static" | "async" => {
                push_unique(&mut modifiers, token);
            }
            "pub" => push_unique(&mut modifiers, "public"),
            _ => {}
        }
    }
    modifiers
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_owned());
    }
}

fn first_non_empty_line(value: &str) -> &str {
    value
        .lines()
        .find(|line| !line.trim().is_empty())
        .map_or("", str::trim)
}

fn closest_ancestor<'tree>(mut node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn find_descendant_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    if node.kind() == kind {
        return Some(node);
    }
    named_children(node)
        .into_iter()
        .find_map(|child| find_descendant_by_kind(child, kind))
}

fn classify_code_node(node: Node<'_>, breadcrumb: &[String]) -> SymbolType {
    let kind = node.kind();
    if kind == "decorated_definition" {
        return inner_python_definition(node).map_or(SymbolType::Value, |inner| {
            classify_code_node(inner, breadcrumb)
        });
    }
    if matches!(
        kind,
        "field_definition" | "public_field_definition" | "variable_declarator"
    ) && has_javascript_typescript_function_value(node)
    {
        return SymbolType::Function;
    }
    if kind.contains("method") || kind.contains("constructor") {
        return SymbolType::Function;
    }
    if !breadcrumb.is_empty()
        && (kind.contains("function") || kind == "declaration" || kind == "function_item")
    {
        return SymbolType::Function;
    }
    if kind.contains("function") || matches!(kind, "declaration" | "macro_type_specifier") {
        return SymbolType::Function;
    }
    if kind.contains("class")
        || kind.contains("struct")
        || kind.contains("impl")
        || kind.contains("enum")
        || kind.contains("union")
        || kind.contains("record")
    {
        return SymbolType::Class;
    }
    if kind.contains("interface") || kind.contains("protocol") || kind.contains("trait") {
        return SymbolType::Interface;
    }
    if kind.contains("module") || kind.contains("namespace") || kind == "mod_item" {
        return SymbolType::Module;
    }
    if kind.contains("alias")
        || kind.contains("typedef")
        || matches!(kind, "type_definition" | "type_item")
    {
        return SymbolType::Alias;
    }
    SymbolType::Value
}
