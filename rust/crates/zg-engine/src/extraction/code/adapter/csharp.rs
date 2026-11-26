use super::{
    Language, LanguageAdapter, Node, SymbolType, classify_code_node, extract_generic_signature,
    extract_signature_prefix, field_text, named_children,
};

pub(super) const ADAPTER: LanguageAdapter = LanguageAdapter {
    kind: Language::CSharp,
    entity_types: &[
        "class_declaration",
        "constructor_declaration",
        "conversion_operator_declaration",
        "delegate_declaration",
        "destructor_declaration",
        "enum_declaration",
        "enum_member_declaration",
        "event_declaration",
        "event_field_declaration",
        "field_declaration",
        "file_scoped_namespace_declaration",
        "indexer_declaration",
        "interface_declaration",
        "local_function_statement",
        "method_declaration",
        "namespace_declaration",
        "operator_declaration",
        "property_declaration",
        "record_declaration",
        "struct_declaration",
    ],
    scope_types: &[
        "class_declaration",
        "constructor_declaration",
        "enum_declaration",
        "interface_declaration",
        "local_function_statement",
        "method_declaration",
        "namespace_declaration",
        "record_declaration",
        "struct_declaration",
    ],
};

pub(super) fn resolve_entities(node: Node<'_>) -> Vec<Node<'_>> {
    if matches!(node.kind(), "field_declaration" | "event_field_declaration")
        && let Some(variables) = named_children(node)
            .into_iter()
            .find(|child| child.kind() == "variable_declaration")
    {
        return named_children(variables)
            .into_iter()
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
    }
    vec![node]
}

pub(super) fn declaration(node: Node<'_>) -> Node<'_> {
    if node.kind() == "variable_declarator"
        && let Some(statement) = node.parent().and_then(|parent| parent.parent())
        && matches!(
            statement.kind(),
            "field_declaration" | "event_field_declaration"
        )
    {
        return statement;
    }
    node
}

pub(super) fn extract_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = match node.kind() {
        "indexer_declaration" => "this[]".to_owned(),
        "operator_declaration" => format!("operator {}", field_text(node, "operator", source)?),
        "conversion_operator_declaration" => {
            format!("operator {}", field_text(node, "type", source)?)
        }
        "destructor_declaration" => format!("~{}", field_text(node, "name", source)?),
        _ => field_text(node, "name", source)?,
    };
    // Explicit interface implementations may share a member name in one type.
    let interface = named_children(node)
        .into_iter()
        .find(|child| child.kind() == "explicit_interface_specifier");
    Some(interface.map_or_else(
        || name.clone(),
        |interface| format!("{}{name}", super::text(interface, source).trim()),
    ))
}

pub(super) fn classify(node: Node<'_>) -> Option<SymbolType> {
    match node.kind() {
        "struct_declaration" => Some(SymbolType::Class),
        "delegate_declaration" => Some(SymbolType::Alias),
        "namespace_declaration" | "file_scoped_namespace_declaration" => Some(SymbolType::Module),
        "conversion_operator_declaration"
        | "destructor_declaration"
        | "local_function_statement"
        | "operator_declaration" => Some(SymbolType::Function),
        "enum_member_declaration"
        | "event_declaration"
        | "event_field_declaration"
        | "field_declaration"
        | "indexer_declaration"
        | "property_declaration"
        | "variable_declarator" => Some(SymbolType::Value),
        _ => classify_code_node(node),
    }
}

pub(super) fn extract_signature(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "variable_declarator" {
        let statement = declaration(node);
        let first = resolve_entities(statement).into_iter().next()?;
        let prefix = extract_signature_prefix(statement, source, first.start_byte());
        return Some(format!(
            "{} {}",
            prefix.trim(),
            extract_generic_signature(node, source)?
        ));
    }
    // Accessors and expression-bodied properties are implementation, not signature.
    let body = node.child_by_field_name("accessors").or_else(|| {
        node.child_by_field_name("value")
            .filter(|value| value.kind() == "arrow_expression_clause")
    });
    if let Some(body) = body {
        return Some(
            extract_signature_prefix(node, source, body.start_byte())
                .trim()
                .to_owned(),
        );
    }
    extract_generic_signature(node, source)
}
