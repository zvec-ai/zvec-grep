//! Walk-time collectors for name-based graph references.
//!
//! These run inside entity bodies — the region the symbol walker deliberately
//! does not descend into — so call sites are attributed to their innermost
//! owning entity without a second full parse. Mirrors the TypeScript
//! `src/engine/extraction/graph/edge-collector.ts` implementation.

use tree_sitter::Node;

use crate::domain::{FileFormat, GraphRefKind};
use crate::utils::collapse_whitespace;

use super::WalkContext;
use super::vocabulary::{
    CALLEE_FIELDS, COMMON_CALL_TYPES, HeritageVocabulary, heritage_vocabulary_for,
    import_vocabulary_for,
};

const MAX_CALLEE_TEXT_CHARS: usize = 180;

// Local mirrors of the `code::adapter` node helpers, which are not visible
// from this module tree.
fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn node_text<'source>(node: Node<'_>, source: &'source [u8]) -> &'source str {
    node.utf8_text(source).unwrap_or_default()
}

/// True when the node is a call-ish expression per the shared vocabulary.
pub(crate) fn is_call_node(node: Node<'_>) -> bool {
    COMMON_CALL_TYPES.contains(&node.kind())
}

/// Extracts the callee expression text for a call node, if recoverable.
fn callee_text_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Java spells member calls as separate `object` and `name` fields; the
    // shared field walk below would return the bare method name and drop
    // the receiver the resolver needs.
    if let Some(member) = java_member_callee(node, source) {
        return Some(member);
    }
    for field in CALLEE_FIELDS {
        if let Some(target) = node.child_by_field_name(field) {
            return Some(node_text(target, source).to_owned());
        }
    }
    node.named_child(0)
        .map(|first| node_text(first, source).to_owned())
}

/// Joins a Java `method_invocation`'s `object` and `name` fields back into
/// the spelled receiver expression (`this.foo`, `super.foo`, `util.run`).
/// Call-shaped receivers (`new Foo().run()`) keep the bare member name:
/// their rejoined text would fail the identifier gate and drop the edge
/// that the bare spelling resolves. Calls without an object field fall
/// through to the shared walk.
fn java_member_callee(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "method_invocation" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    let name = node.child_by_field_name("name")?;
    let object_text = node_text(object, source);
    if !is_identifier_like(object_text) {
        return None;
    }
    Some(format!("{object_text}.{}", node_text(name, source)))
}

/// Counts call arguments when the grammar exposes an `arguments` field.
fn call_arity_of(node: Node<'_>) -> Option<usize> {
    let arguments = node
        .child_by_field_name("arguments")
        .or_else(|| node.child_by_field_name("argument_list"))?;
    let mut cursor = arguments.walk();
    Some(
        arguments
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "," && !child.kind().ends_with("_comment"))
            .count(),
    )
}

/// Splits a callee expression into its receiver and short member name.
fn split_callee_name(callee_text: &str) -> (String, Option<String>) {
    let collapsed = collapse_whitespace(callee_text);
    let cleaned = strip_new_prefix(&collapsed);
    let Some(separator) = cleaned.rfind(['.', ':']) else {
        return (cleaned.to_owned(), None);
    };
    if separator == 0 || separator + 1 >= cleaned.len() {
        return (cleaned.to_owned(), None);
    }
    let receiver = cleaned[..separator].trim_end_matches(':');
    (
        cleaned[separator + 1..].to_owned(),
        Some(receiver.to_owned()),
    )
}

/// Collapses Python's `super().member` call receiver to the bare `super`
/// keyword, the same superclass address the other languages spell without
/// the call. The identifier gate below would otherwise drop the whole
/// edge because of the parenthesis pair.
fn normalize_super_call(callee: &str) -> String {
    let Some(separator) = callee.rfind('.') else {
        return callee.to_owned();
    };
    let (receiver, member) = callee.split_at(separator);
    if receiver == "super()" {
        format!("super{member}")
    } else {
        callee.to_owned()
    }
}

/// Strips a leading `new ` constructor prefix (`/^new\s+/`).
fn strip_new_prefix(cleaned: &str) -> &str {
    if let Some(rest) = cleaned.strip_prefix("new") {
        let after = rest.trim_start();
        if after.len() < rest.len() {
            return after;
        }
    }
    cleaned
}

/// Accepts plain and qualified identifiers (`dog`, `HashMap::new`, `a.b.c`)
/// in the ASCII shape tree-sitter grammars use for identifier tokens. The
/// TypeScript original expresses this as `/^[A-Za-z_$][A-Za-z0-9_$.:]*$/`.
fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let first_ok = first.is_ascii_alphabetic() || matches!(first, '_' | '$');
    first_ok && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '.' | ':'))
}

/// Scans an entity body for call sites and buffers one `calls` reference per
/// distinct callee. Nested call expressions inside an argument list are
/// skipped: the inner call is collected on its own visit when the walk
/// reaches it as a sibling — keeping attribution shallow matches how the
/// outline collector treats nested calls and avoids double-counting chains.
/// `boundary` marks subtrees owned by another entity (nested members of a
/// container entity); their call sites are claimed when the walk reaches the
/// owning entity, so they are not collected here.
pub(crate) fn scan_call_edges(
    entity_node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    visit_for_calls(entity_node, owner_index, source, context, boundary);
}

fn visit_for_calls(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    if is_call_node(node) {
        if let Some(callee) = callee_text_of(node, source) {
            // C/C++ arrow member access joins the identifier gate and the
            // split under the dot spelling: `ptr->bar()` names the same
            // member shape as `ref.bar()`. The buffered raw text keeps the
            // original spelling.
            let normalized = callee.replace("->", ".");
            let normalized = normalize_super_call(&normalized);
            if normalized.len() <= MAX_CALLEE_TEXT_CHARS && is_identifier_like(&normalized) {
                let (ref_name, receiver_name) = split_callee_name(&normalized);
                context.add_call_edge(
                    owner_index,
                    &ref_name,
                    receiver_name.as_deref(),
                    Some(callee.as_str()),
                    call_arity_of(node),
                    node.start_position().row + 1,
                    node.start_position().column,
                );
            }
        }
        // Do not descend into the call's own arguments; other collectors and
        // the walk revisit that region.
        return;
    }
    for child in named_children(node) {
        if boundary.is_some_and(|owns| owns(child)) {
            continue;
        }
        visit_for_calls(child, owner_index, source, context, boundary);
    }
}

/// Collects variable/parameter type bindings from one entity's subtree:
/// creation-initialized declarations (`var x = new X()`, `x = X()`,
/// `let x = X {}`, `x := new(X)`, `auto x = X{}`, `const x = new X()`) and
/// typed parameters (`caller(Left a, Right b)`). Each binding records the
/// receiver type the resolver may infer when a call at or after this line
/// names the variable. `boundary` mirrors [`scan_call_edges`]: subtrees
/// owned by another entity are skipped — those claim their own bindings.
pub(crate) fn scan_binding_declarations(
    entity_node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    visit_for_bindings(entity_node, owner_index, source, format, context, boundary);
}

fn visit_for_bindings(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    if let Some((name, bound_type)) = declaration_binding_of(node, source, format, context) {
        context.add_declare_binding(
            owner_index,
            &name,
            &bound_type,
            node.start_position().row + 1,
            node.start_position().column,
        );
        return;
    }
    for child in named_children(node) {
        if boundary.is_some_and(|owns| owns(child)) {
            continue;
        }
        visit_for_bindings(child, owner_index, source, format, context, boundary);
    }
}

/// Extracts `(bound name, declared type)` from a declaration node whose
/// shape one of the per-language readers recognizes.
fn declaration_binding_of(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &WalkContext,
) -> Option<(String, String)> {
    match format {
        FileFormat::Java => java_binding_of(node, source, context),
        FileFormat::Python => python_binding_of(node, source, context),
        FileFormat::Rust => rust_binding_of(node, source, context),
        FileFormat::Go => go_binding_of(node, source, context),
        FileFormat::Cpp => cpp_binding_of(node, source, context),
        FileFormat::C => c_binding_of(node, source),
        FileFormat::TypeScript => ts_binding_of(node, source, context),
        FileFormat::JavaScript => js_binding_of(node, source, context),
        _ => None,
    }
}

/// Collects this file's factory return types before the per-entity walk:
/// functions register their signature's return type, and untyped functions
/// (JavaScript, Python) register the type their `return` constructions
/// spell. The creation binding collector then answers a bare-call
/// initializer (`x = makeX()`) from that table regardless of whether the
/// factory is spelled before or after the caller.
pub(crate) fn scan_return_types(
    root: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
) {
    visit_for_return_types(root, source, format, context);
}

fn visit_for_return_types(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
) {
    if let Some((name, return_type)) = function_return_of(node, source, format) {
        context.register_return_type(&name, &return_type);
        // The body belongs to this registration; nested functions spell
        // their own.
        return;
    }
    for child in named_children(node) {
        visit_for_return_types(child, source, format, context);
    }
}

/// `(function name, factory return type)` of a function node whose
/// signature annotates the type — or, for the untyped languages, whose
/// `return` statements all construct the same one.
fn function_return_of(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
) -> Option<(String, String)> {
    match format {
        FileFormat::Java => java_return_of(node, source),
        FileFormat::Python => python_return_of(node, source),
        FileFormat::Rust => rust_return_of(node, source),
        FileFormat::Go => go_return_of(node, source),
        FileFormat::Cpp => cpp_return_of(node, source),
        FileFormat::TypeScript => ts_return_of(node, source),
        FileFormat::JavaScript => js_return_of(node, source),
        _ => None,
    }
}

/// The single type this function's `return` statements construct
/// (`return X()`, `return new X()`); several different types, or a body
/// with none, keep the factory answer unknown.
fn body_return_type_of(function: Node<'_>, source: &[u8]) -> Option<String> {
    let body = function.child_by_field_name("body")?;
    let mut types = Vec::new();
    collect_return_constructions(body, source, &mut types);
    types.sort();
    types.dedup();
    if let [only] = types.as_slice() {
        return Some(only.clone());
    }
    None
}

fn collect_return_constructions(node: Node<'_>, source: &[u8], types: &mut Vec<String>) {
    match node.kind() {
        // A nested function's returns spell its own factory, not this one.
        "function_definition"
        | "function_declaration"
        | "function_item"
        | "method_definition"
        | "method_declaration" => return,
        "return_statement" => {
            if let Some(type_name) = return_construction_of(node, source) {
                types.push(type_name);
            }
            return;
        }
        _ => {}
    }
    for child in named_children(node) {
        collect_return_constructions(child, source, types);
    }
}

/// The type one `return` statement constructs: a bare-identifier call
/// (`return X()`) or a `new` expression (`return new X()`). Attribute and
/// member callees stay untyped.
fn return_construction_of(stmt: Node<'_>, source: &[u8]) -> Option<String> {
    let expression = stmt.named_child(0)?;
    match expression.kind() {
        "new_expression" => {
            let constructor = expression.child_by_field_name("constructor")?;
            (constructor.kind() == "identifier").then(|| node_text(constructor, source).to_owned())
        }
        "call_expression" | "call" => {
            let function = expression.child_by_field_name("function")?;
            (function.kind() == "identifier").then(|| node_text(function, source).to_owned())
        }
        _ => None,
    }
}

fn java_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "method_declaration" {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    if type_node.kind() == "void_type" {
        return None;
    }
    let type_name = type_name_of(type_node, source)?;
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    Some((node_text(name, source).to_owned(), type_name))
}

fn python_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_definition" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    // An arrow annotation is authoritative; otherwise the `return`
    // constructions spell the factory type.
    if let Some(type_name) = node
        .child_by_field_name("return_type")
        .and_then(|annotation| type_name_of(annotation, source))
    {
        return Some((node_text(name, source).to_owned(), type_name));
    }
    let type_name = body_return_type_of(node, source)?;
    Some((node_text(name, source).to_owned(), type_name))
}

fn rust_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_item" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    let return_type = node.child_by_field_name("return_type")?;
    let type_name = type_name_of(return_type, source)?;
    Some((node_text(name, source).to_owned(), type_name))
}

fn go_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_declaration" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    let result = node.child_by_field_name("result")?;
    let type_name = type_name_of(result, source)?;
    Some((node_text(name, source).to_owned(), type_name))
}

fn cpp_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_definition" {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    let type_name = type_name_of(type_node, source)?;
    if type_name == "void" {
        return None;
    }
    // The function name sits under the declarator chain: the function
    // declarator itself, optionally wrapped in pointer/reference/array
    // declarators.
    let mut declarator = node.child_by_field_name("declarator")?;
    while matches!(
        declarator.kind(),
        "function_declarator" | "pointer_declarator" | "reference_declarator" | "array_declarator"
    ) {
        declarator = declarator.child_by_field_name("declarator")?;
    }
    if declarator.kind() != "identifier" {
        return None;
    }
    Some((node_text(declarator, source).to_owned(), type_name))
}

fn ts_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_declaration" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    let annotation = node.child_by_field_name("return_type")?;
    if annotation.kind() != "type_annotation" {
        return None;
    }
    let inner = annotation.named_child(0)?;
    if inner.kind() != "type_identifier" {
        return None;
    }
    Some((
        node_text(name, source).to_owned(),
        node_text(inner, source).to_owned(),
    ))
}

fn js_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "function_declaration" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    let type_name = body_return_type_of(node, source)?;
    Some((node_text(name, source).to_owned(), type_name))
}

/// Spelled receiver type of a type node. Pointer/reference/array wrappers
/// address the same method set as their inner type (`*Right`, `Right[]`).
fn type_name_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    let inner = match node.kind() {
        "pointer_type" | "array_type" | "slice_type" | "reference_type" => node.named_child(0)?,
        _ => node,
    };
    let text = node_text(inner, source);
    is_identifier_like(text).then(|| text.to_owned())
}

/// Follows pointer/reference declarator wrappers down to the spelled name.
fn peel_declarator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "reference_declarator" | "pointer_declarator" | "array_declarator" => {
            peel_declarator_name(node.named_child(0)?, source)
        }
        "identifier" => Some(node_text(node, source).to_owned()),
        _ => None,
    }
}

fn java_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    match node.kind() {
        // `var x = new X()` and `X x = new X()`: the creation expression
        // spells the type regardless of how the declaration's own type
        // field spells it (`var` placeholder or the explicit name). A
        // bare-call initializer (`var x = makeX()`) binds the factory's
        // registered return type instead; a receiver turns the callee
        // into a member call no factory table answers.
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            if value.kind() == "method_invocation" {
                if value.child_by_field_name("object").is_some() {
                    return None;
                }
                let callee = value.child_by_field_name("name")?;
                if callee.kind() != "identifier" {
                    return None;
                }
                let type_name = context.factory_return_of(node_text(callee, source))?;
                return Some((node_text(name, source).to_owned(), type_name.to_owned()));
            }
            if value.kind() != "object_creation_expression" {
                return None;
            }
            let type_node = value.child_by_field_name("type")?;
            Some((
                node_text(name, source).to_owned(),
                node_text(type_node, source).to_owned(),
            ))
        }
        // `void m(Left a)`: the parameter binds its declared type.
        "formal_parameter" => {
            let name = node.child_by_field_name("name")?;
            let type_node = node.child_by_field_name("type")?;
            let type_name = type_name_of(type_node, source)?;
            Some((node_text(name, source).to_owned(), type_name))
        }
        _ => None,
    }
}

/// `void (*work)(void) = target;`: a function-pointer initializer aliases
/// the callee — a call spelled through the pointer's name reaches the
/// function the initializer names, not a same-named declaration the local
/// hides. Plain declarations without an initializer bind nothing.
fn c_binding_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "declaration" {
        return None;
    }
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() != "init_declarator" {
        return None;
    }
    let value = declarator.child_by_field_name("value")?;
    if value.kind() != "identifier" {
        return None;
    }
    let mut name_node = declarator.child_by_field_name("declarator")?;
    loop {
        match name_node.kind() {
            // Parenthesized declarators wrap their inner spelling without a
            // field name; the pointer/function layers name it `declarator`.
            "parenthesized_declarator" => name_node = name_node.named_child(0)?,
            "pointer_declarator" | "function_declarator" | "array_declarator" => {
                name_node = name_node.child_by_field_name("declarator")?;
            }
            _ => break,
        }
    }
    if name_node.kind() != "identifier" {
        return None;
    }
    Some((
        node_text(name_node, source).to_owned(),
        node_text(value, source).to_owned(),
    ))
}

fn python_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    if node.kind() != "assignment" {
        return None;
    }
    // `x = X()`: the call's bare function name is the constructed type —
    // unless that name is a registered factory, whose return type binds
    // instead (`x = make_x()`). Attribute calls (`x = mod.X()`) and
    // non-call initializers carry no locally spellable type.
    let left = node.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let right = node.child_by_field_name("right")?;
    if right.kind() != "call" {
        return None;
    }
    let function = right.child_by_field_name("function")?;
    if function.kind() != "identifier" {
        return None;
    }
    let callee = node_text(function, source);
    let bound_type = context.factory_return_of(callee).unwrap_or(callee);
    Some((node_text(left, source).to_owned(), bound_type.to_owned()))
}

fn rust_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    if node.kind() != "let_declaration" {
        return None;
    }
    let pattern = node.child_by_field_name("pattern")?;
    if pattern.kind() != "identifier" {
        return None;
    }
    let value = node.child_by_field_name("value")?;
    // `let x = make_x()`: a bare call binds the callee's registered
    // factory return type; member calls stay untyped.
    if value.kind() == "call_expression" {
        let function = value.child_by_field_name("function")?;
        if function.kind() != "identifier" {
            return None;
        }
        let type_name = context.factory_return_of(node_text(function, source))?;
        return Some((node_text(pattern, source).to_owned(), type_name.to_owned()));
    }
    // `let x = X {}`: the struct literal spells the type.
    if value.kind() != "struct_expression" {
        return None;
    }
    let name = value.child_by_field_name("name")?;
    Some((
        node_text(pattern, source).to_owned(),
        node_text(name, source).to_owned(),
    ))
}

fn go_binding_of(node: Node<'_>, source: &[u8], context: &WalkContext) -> Option<(String, String)> {
    match node.kind() {
        // `x := new(X)`: the new call's type argument is the type.
        // `x := makeX()`: a bare call binds the registered factory's
        // return type instead.
        "short_var_declaration" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let call = right.named_child(0)?;
            if call.kind() != "call_expression" {
                return None;
            }
            let function = call.child_by_field_name("function")?;
            if function.kind() != "identifier" {
                return None;
            }
            let name = left.named_child(0)?;
            if name.kind() != "identifier" {
                return None;
            }
            let callee = node_text(function, source);
            if callee == "new" {
                let arguments = call.child_by_field_name("arguments")?;
                let type_node = arguments.named_child(0)?;
                return Some((
                    node_text(name, source).to_owned(),
                    node_text(type_node, source).to_owned(),
                ));
            }
            let type_name = context.factory_return_of(callee)?;
            Some((node_text(name, source).to_owned(), type_name.to_owned()))
        }
        // `func m(a Left, b *Right)`: the parameter binds its type.
        "parameter_declaration" => {
            let name = node.child_by_field_name("name")?;
            let type_node = node.child_by_field_name("type")?;
            let type_name = type_name_of(type_node, source)?;
            Some((node_text(name, source).to_owned(), type_name))
        }
        _ => None,
    }
}

fn cpp_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    match node.kind() {
        // `auto x = X{}` / `auto x = new X()`: the placeholder type defers
        // to the initializer's spelled type. `auto x = makeX()`: a bare
        // call binds the registered factory's return type.
        "declaration" => {
            let declared = node.child_by_field_name("type")?;
            if declared.kind() != "placeholder_type_specifier" {
                return None;
            }
            let declarator = node.child_by_field_name("declarator")?;
            if declarator.kind() != "init_declarator" {
                return None;
            }
            let name = declarator.child_by_field_name("declarator")?;
            let value = declarator.child_by_field_name("value")?;
            let type_node = match value.kind() {
                "new_expression" | "compound_literal_expression" => {
                    value.child_by_field_name("type")?
                }
                "call_expression" => {
                    let function = value.child_by_field_name("function")?;
                    if function.kind() != "identifier" {
                        return None;
                    }
                    let type_name = context.factory_return_of(node_text(function, source))?;
                    return Some((node_text(name, source).to_owned(), type_name.to_owned()));
                }
                _ => return None,
            };
            Some((
                node_text(name, source).to_owned(),
                node_text(type_node, source).to_owned(),
            ))
        }
        // `void m(Left &a)`: the parameter name sits under the reference
        // declarator wrapper.
        "parameter_declaration" => {
            let type_node = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            let type_name = type_name_of(type_node, source)?;
            let name = peel_declarator_name(declarator, source)?;
            Some((name, type_name))
        }
        _ => None,
    }
}

fn ts_binding_of(node: Node<'_>, source: &[u8], context: &WalkContext) -> Option<(String, String)> {
    match node.kind() {
        // `const x = new X()`: the constructor expression spells the type.
        // `const x = makeX()`: a bare call binds the registered factory's
        // return type.
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            let type_name = creation_or_factory_type(value, source, context)?;
            Some((node_text(name, source).to_owned(), type_name))
        }
        // `x = new X()`: a reassignment rebinds the name; the resolver
        // orders the bindings by line so calls after it see the new type.
        "assignment_expression" => {
            let left = node.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            let value = node.child_by_field_name("right")?;
            let type_name = creation_or_factory_type(value, source, context)?;
            Some((node_text(left, source).to_owned(), type_name))
        }
        // `function m(a: Left, b: Right)`: the parameter binds its type
        // annotation's single type; unions stay unbound (either type may
        // answer the call).
        "required_parameter" => {
            let pattern = node.child_by_field_name("pattern")?;
            if pattern.kind() != "identifier" {
                return None;
            }
            let annotation = node.child_by_field_name("type")?;
            if annotation.kind() != "type_annotation" {
                return None;
            }
            let inner = annotation.named_child(0)?;
            if inner.kind() != "type_identifier" {
                return None;
            }
            Some((
                node_text(pattern, source).to_owned(),
                node_text(inner, source).to_owned(),
            ))
        }
        _ => None,
    }
}

/// The type a creation or bare-call initializer spells: a `new X()`
/// constructor names `X`, a bare `makeX()` call binds the registered
/// factory's return type, and anything else carries no locally spellable
/// type.
fn creation_or_factory_type(
    value: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<String> {
    if value.kind() == "call_expression" {
        let function = value.child_by_field_name("function")?;
        if function.kind() != "identifier" {
            return None;
        }
        return context
            .factory_return_of(node_text(function, source))
            .map(str::to_owned);
    }
    if value.kind() != "new_expression" {
        return None;
    }
    let constructor = value.child_by_field_name("constructor")?;
    (constructor.kind() == "identifier").then(|| node_text(constructor, source).to_owned())
}

fn js_binding_of(node: Node<'_>, source: &[u8], context: &WalkContext) -> Option<(String, String)> {
    match node.kind() {
        // `const x = new X()`: the constructor expression spells the type.
        // `const x = makeX()`: a bare call binds the factory type the
        // function's `return` construction spells (JavaScript signs no
        // return types).
        "variable_declarator" => {
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            let value = node.child_by_field_name("value")?;
            let type_name = creation_or_factory_type(value, source, context)?;
            Some((node_text(name, source).to_owned(), type_name))
        }
        // `x = new X()`: a reassignment rebinds the name; the resolver
        // orders the bindings by line so calls after it see the new type.
        "assignment_expression" => {
            let left = node.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            let value = node.child_by_field_name("right")?;
            let type_name = creation_or_factory_type(value, source, context)?;
            Some((node_text(left, source).to_owned(), type_name))
        }
        _ => None,
    }
}

/// Normalizes an import module path for buffering (strips quotes/brackets).
fn normalize_module_path(module_text: &str) -> String {
    let unquoted = module_text
        .trim()
        .trim_start_matches(['<', '"', '\'', '`'])
        .trim_end_matches(['>', '"', '\'', '`']);
    strip_trailing_semicolons(unquoted).trim().to_owned()
}

/// Removes a trailing semicolon run plus following whitespace (`/;+\s*$/`).
fn strip_trailing_semicolons(value: &str) -> &str {
    let without_whitespace = value.trim_end();
    let semicolons = without_whitespace
        .chars()
        .rev()
        .take_while(|&ch| ch == ';')
        .count();
    if semicolons == 0 {
        return value;
    }
    &without_whitespace[..without_whitespace.len() - semicolons]
}

/// Extracts the module path from an import node via grammar fields or text.
fn module_path_of(node: Node<'_>, source: &[u8], format: FileFormat) -> Option<String> {
    if let Some(vocabulary) = import_vocabulary_for(format) {
        for field in vocabulary.module_fields {
            if let Some(target) = node.child_by_field_name(field) {
                if matches!(format, FileFormat::C | FileFormat::Cpp)
                    && let Some(path) = c_include_module_path(target, source)
                {
                    return Some(path);
                }
                let path = normalize_module_path(node_text(target, source));
                if !path.is_empty() {
                    // Python: the `name` field wraps the whole aliased unit
                    // on `import left as left_api`; the module path is the
                    // wrapper's inner `name` field. Unaliased imports and
                    // `from` modules carry no inner `name` and keep the
                    // outer path.
                    if format == FileFormat::Python
                        && let Some(inner) = target.child_by_field_name("name")
                    {
                        let inner_path = normalize_module_path(node_text(inner, source));
                        if !inner_path.is_empty() {
                            return Some(inner_path);
                        }
                    }
                    return Some(path);
                }
            }
            // Go: `import_declaration` carries no fields; the path lives on
            // the nested `import_spec` nodes (single and grouped imports).
            if format == FileFormat::Go {
                for descendant in descendants_within(node, 2) {
                    if let Some(target) = descendant.child_by_field_name(field) {
                        let path = normalize_module_path(node_text(target, source));
                        if !path.is_empty() {
                            return Some(path);
                        }
                    }
                }
            }
        }
        // Java: the declaration carries no path field; the module path is
        // the first identifier-like named child (`identifier` /
        // `scoped_identifier`).
        if format == FileFormat::Java {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let path = collapse_whitespace(node_text(child, source));
                if is_identifier_like(&path) {
                    return Some(path);
                }
            }
        }
    }
    // Text fallback (Rust `use a::b;`, Go grouped imports, C includes whose
    // path field is nested in a string literal node).
    let node_path = normalize_module_path(node_text(node, source));
    if format == FileFormat::Rust {
        return strip_rust_use_path(&node_path);
    }
    if !node_path.is_empty() && !node_path.contains('\n') {
        return Some(node_path);
    }
    None
}

/// Specializes C/C++ include paths by their node form, which tree-sitter
/// spells distinctly. `<x>` (`system_lib_string`) addresses the standard
/// library and `-I` include paths the workspace snapshot can never prove —
/// the angle brackets survive so the resolver terminates them as external.
/// `"x"` (`string_literal`) searches the includer's directory first per the
/// C standard, so it normalizes to `./x` and the relative-import rule
/// resolves it against the owning file. Other node kinds fall back to the
/// generic path.
fn c_include_module_path(target: Node<'_>, source: &[u8]) -> Option<String> {
    let path = normalize_module_path(node_text(target, source));
    if path.is_empty() {
        return None;
    }
    match target.kind() {
        "system_lib_string" => Some(format!("<{path}>")),
        "string_literal" => {
            if path.starts_with("./") || path.starts_with("../") {
                Some(path)
            } else {
                Some(format!("./{path}"))
            }
        }
        _ => None,
    }
}

/// Named descendants within `max_depth` levels below `node`.
fn descendants_within(node: Node<'_>, max_depth: usize) -> Vec<Node<'_>> {
    let mut found = Vec::new();
    if max_depth == 0 {
        return found;
    }
    for child in named_children(node) {
        found.push(child);
        found.extend(descendants_within(child, max_depth - 1));
    }
    found
}

/// Reduces a normalized `use` declaration to its module path: drops a leading
/// `use`, a trailing `as <ident>` alias, brace groups, and all whitespace.
fn strip_rust_use_path(normalized: &str) -> Option<String> {
    let without_alias = strip_trailing_as_alias(strip_leading_use(normalized));
    let stripped: String = without_alias
        .chars()
        .filter(|ch| !matches!(ch, '{' | '}') && !ch.is_whitespace())
        .collect();
    let stripped = stripped.strip_suffix(';').unwrap_or(&stripped);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// Strips a leading `use` keyword (`/^use\s+/i`).
fn strip_leading_use(text: &str) -> &str {
    if text
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"use"))
    {
        let rest = text[3..].trim_start();
        if rest.len() < text.len() - 3 {
            return rest;
        }
    }
    text
}

/// Drops a trailing `as <ident>` alias (`/\s+as\s+\w+$/i`). Whitespace inside
/// the kept prefix does not survive [`strip_rust_use_path`], so rejoining on
/// single spaces preserves the resulting path.
fn strip_trailing_as_alias(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() >= 3 {
        let alias = words[words.len() - 2];
        let last = words[words.len() - 1];
        if alias.eq_ignore_ascii_case("as")
            && last
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return words[..words.len() - 2].join(" ");
        }
    }
    text.to_owned()
}

/// True when the node type belongs to the language's import vocabulary.
pub(crate) fn is_import_node(node: Node<'_>, format: FileFormat) -> bool {
    import_vocabulary_for(format)
        .is_some_and(|vocabulary| vocabulary.import_types.contains(&node.kind()))
}

/// Buffers an `imports` reference for an import declaration node. The owner
/// is the file scope: imports belong to the file, not to any entity.
pub(crate) fn collect_import_edge(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
) {
    let Some(path) = module_path_of(node, source, format) else {
        return;
    };
    context.add_import_edge(
        &path,
        Some(node_text(node, source)),
        node.start_position().row + 1,
        node.start_position().column,
    );
}

/// Buffers `extends`/`implements` references for a class-like entity node,
/// reading the per-language heritage vocabulary.
pub(crate) fn collect_inheritance_edges(
    entity_node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    owner_index: usize,
    context: &mut WalkContext,
) {
    let Some(vocabulary) = heritage_vocabulary_for(format) else {
        return;
    };
    // (kind, clause node) pairs; the clause node feeds type-name extraction.
    let clauses = match vocabulary {
        HeritageVocabulary::Nested {
            extends_types,
            implements_types,
        } => descendants_within(entity_node, 2)
            .into_iter()
            .filter_map(|descendant| {
                if extends_types.contains(&descendant.kind()) {
                    Some((GraphRefKind::Extends, descendant))
                } else if implements_types.contains(&descendant.kind()) {
                    Some((GraphRefKind::Implements, descendant))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
        HeritageVocabulary::Fields {
            extends,
            implements,
        } => extends
            .iter()
            .filter_map(|field| {
                entity_node
                    .child_by_field_name(field)
                    .map(|clause| (GraphRefKind::Extends, clause))
            })
            .chain(implements.iter().filter_map(|field| {
                entity_node
                    .child_by_field_name(field)
                    .map(|clause| (GraphRefKind::Implements, clause))
            }))
            .collect(),
    };
    for (ref_kind, clause) in clauses {
        for name in type_names_from_clause(clause, source) {
            context.add_inheritance_edge(
                ref_kind,
                owner_index,
                &name,
                Some(node_text(clause, source)),
                clause.start_position().row + 1,
                clause.start_position().column,
            );
        }
    }
}

/// C++ base-clause decoration kinds that never name an inherited type.
const CLAUSE_DECORATION_KINDS: &[&str] = &[
    "access_specifier",
    "alignas_qualifier",
    "attribute_declaration",
    "ms_declspec_modifier",
    "virtual_specifier",
];

/// Splits an inheritance clause into type names. Handles the common shapes:
/// a single type node, a comma-separated list, and C++ base specifiers with
/// access labels (`public Base`).
fn type_names_from_clause(clause: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    collect_type_names(clause, source, &mut names);
    names
}

fn collect_type_names(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    if CLAUSE_DECORATION_KINDS.contains(&node.kind()) {
        return;
    }
    let text = collapse_whitespace(node_text(node, source));
    if text.is_empty() || text.len() > MAX_CALLEE_TEXT_CHARS {
        return;
    }
    if node.named_child_count() == 0 && is_identifier_like(&text) {
        names.push(text);
        return;
    }
    for child in named_children(node) {
        collect_type_names(child, source, names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_callee_names_into_receiver_and_member() {
        assert_eq!(split_callee_name("helper"), ("helper".to_owned(), None));
        assert_eq!(
            split_callee_name("dog.bark"),
            ("bark".to_owned(), Some("dog".to_owned()))
        );
        assert_eq!(
            split_callee_name("HashMap::new"),
            ("new".to_owned(), Some("HashMap".to_owned()))
        );
        // Receiver runs of `::` collapse to a single trailing run.
        assert_eq!(
            split_callee_name("a::b::c"),
            ("c".to_owned(), Some("a::b".to_owned()))
        );
        // `new` prefix is stripped; whitespace runs collapse.
        assert_eq!(
            split_callee_name("new   Widget"),
            ("Widget".to_owned(), None)
        );
        // A leading or trailing separator disables the split.
        assert_eq!(split_callee_name(".foo"), (".foo".to_owned(), None));
        assert_eq!(split_callee_name("foo."), ("foo.".to_owned(), None));
    }

    #[test]
    fn filters_identifier_like_texts() {
        assert!(is_identifier_like("helper"));
        assert!(is_identifier_like("$scope"));
        assert!(is_identifier_like("a.b.c"));
        assert!(is_identifier_like("HashMap::new"));
        assert!(!is_identifier_like(""));
        assert!(!is_identifier_like("1helper"));
        assert!(!is_identifier_like("foo bar"));
        assert!(!is_identifier_like("foo(3)"));
        assert!(!is_identifier_like("hélder"));
    }

    #[test]
    fn normalizes_module_paths() {
        assert_eq!(normalize_module_path("\"./utils.js\""), "./utils.js");
        assert_eq!(normalize_module_path("'./utils.js'"), "./utils.js");
        assert_eq!(normalize_module_path("<stdio.h>"), "stdio.h");
        // Quoted path with a trailing semicolon: the quote pair is stripped
        // before the semicolon, leaving the inner quote — matching the
        // TypeScript normalization order. Real grammar paths never combine
        // both shapes (fields exclude the semicolon; text fallbacks for Rust
        // and Go strip it separately).
        assert_eq!(normalize_module_path("  \"foo\";  "), "foo\"");
        assert_eq!(normalize_module_path(""), "");
    }

    #[test]
    fn strips_rust_use_paths() {
        assert_eq!(
            strip_rust_use_path("use std::collections::HashMap"),
            Some("std::collections::HashMap".to_owned())
        );
        // Trailing alias and grouped imports survive the text fallback.
        assert_eq!(
            strip_rust_use_path("use std::fmt::Write as W"),
            Some("std::fmt::Write".to_owned())
        );
        assert_eq!(
            strip_rust_use_path("use std::fmt::{Display, Write}"),
            Some("std::fmt::Display,Write".to_owned())
        );
        assert_eq!(strip_rust_use_path("use {}"), None);
    }

    #[test]
    fn reads_module_paths_from_grammar_nodes() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar loads");
        let source = "use std::collections::HashMap;\n";
        let tree = parser.parse(source, None).expect("rust source parses");
        let use_node = tree
            .root_node()
            .named_child(0)
            .expect("use declaration exists");
        assert_eq!(use_node.kind(), "use_declaration");
        assert_eq!(
            module_path_of(use_node, source.as_bytes(), FileFormat::Rust),
            Some("std::collections::HashMap".to_owned())
        );

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .expect("javascript grammar loads");
        let source = "import { helper } from \"./utils.js\";\n";
        let tree = parser
            .parse(source, None)
            .expect("javascript source parses");
        let import_node = tree
            .root_node()
            .named_child(0)
            .expect("import statement exists");
        assert_eq!(import_node.kind(), "import_statement");
        assert_eq!(
            module_path_of(import_node, source.as_bytes(), FileFormat::JavaScript),
            Some("./utils.js".to_owned())
        );
    }

    #[test]
    fn normalizes_the_super_call_receiver() {
        // Python spells the superclass dispatch with a call receiver; the
        // bare keyword is the shape the split and resolver consume.
        assert_eq!(normalize_super_call("super().foo"), "super.foo");
        assert_eq!(normalize_super_call("super().a.b"), "super().a.b");
        assert_eq!(normalize_super_call("self.foo"), "self.foo");
        assert_eq!(
            split_callee_name(&normalize_super_call("super().foo")),
            ("foo".to_owned(), Some("super".to_owned()))
        );
    }

    #[test]
    fn java_member_calls_keep_their_receiver() {
        fn collect<'t>(node: tree_sitter::Node<'t>, out: &mut Vec<tree_sitter::Node<'t>>) {
            if node.kind() == "method_invocation" {
                out.push(node);
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect(child, out);
            }
        }
        // Java spells member calls as separate `object`/`name` fields; the
        // callee text must rejoin them or the resolver loses the receiver.
        // Call-shaped receivers keep the bare member: their rejoined text
        // would fail the identifier gate and drop the edge entirely.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java grammar loads");
        let source =
            "class A { void m() { this.foo(); super.foo(); util.run(); plain(); new A().run(); } }";
        let tree = parser.parse(source, None).expect("java source parses");
        let mut calls = Vec::new();
        collect(tree.root_node(), &mut calls);
        let bytes = source.as_bytes();
        let texts: Vec<String> = calls
            .iter()
            .filter_map(|call| callee_text_of(*call, bytes))
            .collect();
        assert_eq!(
            texts,
            vec![
                "this.foo".to_owned(),
                "super.foo".to_owned(),
                "util.run".to_owned(),
                "plain".to_owned(),
                "run".to_owned(),
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn collects_creation_and_parameter_bindings() {
        fn bindings(
            format: FileFormat,
            lang: &tree_sitter::Language,
            source: &str,
        ) -> Vec<(String, String, usize)> {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(lang).expect("grammar loads");
            let tree = parser.parse(source, None).expect("parses");
            let mut context = WalkContext::new();
            let owner = context.next_entity_index();
            scan_binding_declarations(
                tree.root_node(),
                owner,
                source.as_bytes(),
                format,
                &mut context,
                None,
            );
            context
                .name_edges()
                .iter()
                .map(|edge| {
                    (
                        edge.ref_name.clone(),
                        edge.bound_type.clone().expect("bound type"),
                        edge.line,
                    )
                })
                .collect()
        }

        // Creation-initialized locals across the grammars.
        assert_eq!(
            bindings(
                FileFormat::Java,
                &tree_sitter_java::LANGUAGE.into(),
                "class A { void m() { var x = new X(); x.foo(); } }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::Python,
                &tree_sitter_python::LANGUAGE.into(),
                "def m():\n    x = X()\n    x.foo()\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 2)]
        );
        assert_eq!(
            bindings(
                FileFormat::Rust,
                &tree_sitter_rust::LANGUAGE.into(),
                "fn m() { let x = X {}; x.foo(); }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::Go,
                &tree_sitter_go::LANGUAGE.into(),
                "func m() {\n\tx := new(X)\n\tx.foo()\n}",
            ),
            vec![("x".to_owned(), "X".to_owned(), 2)]
        );
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &tree_sitter_cpp::LANGUAGE.into(),
                "void m() { auto x = X{}; x.foo(); }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &tree_sitter_cpp::LANGUAGE.into(),
                "void m() { auto x = new X(); x->foo(); }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::TypeScript,
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "function m() { const x = new X(); x.foo(); }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::JavaScript,
                &tree_sitter_javascript::LANGUAGE.into(),
                "function m() { const x = new X(); x.foo(); }",
            ),
            vec![("x".to_owned(), "X".to_owned(), 1)]
        );

        // Typed parameters.
        assert_eq!(
            bindings(
                FileFormat::Java,
                &tree_sitter_java::LANGUAGE.into(),
                "class A { void m(Left a, Right b) { a.run(); } }",
            ),
            vec![
                ("a".to_owned(), "Left".to_owned(), 1),
                ("b".to_owned(), "Right".to_owned(), 1),
            ]
        );
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &tree_sitter_cpp::LANGUAGE.into(),
                "void m(Left &a, Right &b) { a.run(); }",
            ),
            vec![
                ("a".to_owned(), "Left".to_owned(), 1),
                ("b".to_owned(), "Right".to_owned(), 1),
            ]
        );
        assert_eq!(
            bindings(
                FileFormat::Go,
                &tree_sitter_go::LANGUAGE.into(),
                "func m(a Left, b *Right) { a.run() }",
            ),
            vec![
                ("a".to_owned(), "Left".to_owned(), 1),
                ("b".to_owned(), "Right".to_owned(), 1),
            ]
        );
        assert_eq!(
            bindings(
                FileFormat::TypeScript,
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "function m(a: Left, b: Right) { a.run(); }",
            ),
            vec![
                ("a".to_owned(), "Left".to_owned(), 1),
                ("b".to_owned(), "Right".to_owned(), 1),
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn collects_factory_return_bindings() {
        fn factory_bindings(
            format: FileFormat,
            lang: &tree_sitter::Language,
            source: &str,
        ) -> Vec<(String, String, usize)> {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(lang).expect("grammar loads");
            let tree = parser.parse(source, None).expect("parses");
            let mut context = WalkContext::new();
            scan_return_types(tree.root_node(), source.as_bytes(), format, &mut context);
            let owner = context.next_entity_index();
            scan_binding_declarations(
                tree.root_node(),
                owner,
                source.as_bytes(),
                format,
                &mut context,
                None,
            );
            context
                .name_edges()
                .iter()
                .map(|edge| {
                    (
                        edge.ref_name.clone(),
                        edge.bound_type.clone().expect("bound type"),
                        edge.line,
                    )
                })
                .collect()
        }

        // Signature-annotated factories across the grammars.
        assert_eq!(
            factory_bindings(
                FileFormat::Rust,
                &tree_sitter_rust::LANGUAGE.into(),
                "fn make_x() -> X {\n    X {}\n}\nfn caller() {\n    let x = make_x();\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );
        assert_eq!(
            factory_bindings(
                FileFormat::Go,
                &tree_sitter_go::LANGUAGE.into(),
                "func makeX() *X {\n\treturn new(X)\n}\nfunc caller() {\n\tx := makeX()\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );
        assert_eq!(
            factory_bindings(
                FileFormat::TypeScript,
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "function makeX(): X {\n    return new X();\n}\nfunction caller() {\n    const x = makeX();\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );
        assert_eq!(
            factory_bindings(
                FileFormat::Java,
                &tree_sitter_java::LANGUAGE.into(),
                "class Use {\n    static X makeX() {\n        return new X();\n    }\n    static void caller() {\n        var x = makeX();\n    }\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 6)]
        );
        assert_eq!(
            factory_bindings(
                FileFormat::Cpp,
                &tree_sitter_cpp::LANGUAGE.into(),
                "X makeX() {\n    return X{};\n}\nvoid caller() {\n    auto x = makeX();\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );

        // Untyped languages infer from the body's `return` constructions.
        assert_eq!(
            factory_bindings(
                FileFormat::Python,
                &tree_sitter_python::LANGUAGE.into(),
                "def make_x():\n    return X()\n\ndef caller():\n    x = make_x()\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );
        assert_eq!(
            factory_bindings(
                FileFormat::JavaScript,
                &tree_sitter_javascript::LANGUAGE.into(),
                "function makeX() {\n    return new X();\n}\nfunction caller() {\n    const x = makeX();\n}\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );

        // A Python arrow annotation outranks what the body returns.
        assert_eq!(
            factory_bindings(
                FileFormat::Python,
                &tree_sitter_python::LANGUAGE.into(),
                "def make_x() -> X:\n    return Y()\n\ndef caller():\n    x = make_x()\n",
            ),
            vec![("x".to_owned(), "X".to_owned(), 5)]
        );

        // Same-named factories spelling different types stay ambiguous:
        // the bare call binds nothing rather than guessing.
        assert_eq!(
            factory_bindings(
                FileFormat::Rust,
                &tree_sitter_rust::LANGUAGE.into(),
                "fn make() -> X {\n    todo!()\n}\nfn make() -> Y {\n    todo!()\n}\nfn caller() {\n    let m = make();\n}\n",
            ),
            vec![]
        );

        // A member call never consults the factory table.
        assert_eq!(
            factory_bindings(
                FileFormat::Rust,
                &tree_sitter_rust::LANGUAGE.into(),
                "fn make_x() -> X {\n    todo!()\n}\nfn caller(v: V) {\n    let m = v.make_x();\n}\n",
            ),
            vec![]
        );
    }
}
