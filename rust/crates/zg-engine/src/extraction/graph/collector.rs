//! Walk-time collectors for name-based graph references.
//!
//! These run inside entity bodies — the region the symbol walker deliberately
//! does not descend into — so call sites are attributed to their innermost
//! owning entity without a second full parse. Mirrors the TypeScript
//! `src/engine/extraction/graph/edge-collector.ts` implementation.

use tree_sitter::Node;

use crate::domain::{FileFormat, GraphEdgeKind};
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

/// True for call-shaped nodes that name no callee worth reporting, shared by
/// the graph and the outline so both report the same calls:
/// - PHP's first-class callable syntax, `record(...)`, creates a closure
///   without invoking it (a spread such as `record(...$args)` is an ordinary
///   argument).
/// - `new class { ... }` and `(function () { ... })()` name the code they
///   define, not a callee. Calls inside that code are still collected.
/// - `new static`, `new self` and `new parent` name the class only relative
///   to the caller, which no symbol is called.
/// - PHP `make()()` calls the result of another call, which names no symbol.
///   The inner call is reported on its own.
pub(crate) fn is_unreported_call(node: Node<'_>, source: &[u8]) -> bool {
    let creates_callable = node
        .child_by_field_name("arguments")
        .is_some_and(|arguments| {
            named_children(arguments)
                .into_iter()
                .any(|argument| argument.kind() == "variadic_placeholder")
        });
    let is_creation = node.kind() == "object_creation_expression";
    let target = if is_creation {
        node.named_child(0)
    } else {
        node.child_by_field_name("function")
    };
    let target = target.map(|target| {
        if target.kind() == "parenthesized_expression" {
            target.named_child(0).unwrap_or(target)
        } else {
            target
        }
    });
    let calls_a_result =
        node.kind() == "function_call_expression" && target.is_some_and(is_call_node);
    creates_callable
        || calls_a_result
        || target.is_some_and(|target| {
            matches!(
                target.kind(),
                "anonymous_class" | "anonymous_function" | "arrow_function"
            ) || (is_creation && matches!(node_text(target, source), "static" | "self" | "parent"))
        })
}

/// True for subtrees whose calls never run with the enclosing body. PHP
/// attribute arguments, `#[Assert(new Length(5))]`, run on reflection.
pub(crate) fn is_call_scan_barrier(node: Node<'_>) -> bool {
    node.kind() == "attribute_list"
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

/// A PHP member, nullsafe or static call, split into the parts an edge stores.
struct PhpMemberCall {
    name: String,
    receiver: String,
    raw_text: String,
}

fn is_php_member_call(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression"
    )
}

/// Splits a PHP member or static call into its method name and receiver
/// (`$this->repo->find` gives `find` on `$this.repo`, `Audit::record` gives
/// `record` on `Audit`). The receiver is always kept: a call with a receiver
/// stays pending, while a bare name would bind to a same-named function in
/// the file. A receiver the resolver cannot read, such as `\Audit` or
/// `$this->factory()`, keeps its source spelling so the edge stays
/// unresolved. A dynamic method name, such as `$this->{'dyn'}()`, names no
/// symbol, so the call yields no edge.
fn php_member_call(node: Node<'_>, source: &[u8]) -> Option<PhpMemberCall> {
    let (receiver, separator) = match node.kind() {
        "member_call_expression" => (node.child_by_field_name("object")?, "->"),
        "nullsafe_member_call_expression" => (node.child_by_field_name("object")?, "?->"),
        "scoped_call_expression" => (node.child_by_field_name("scope")?, "::"),
        _ => return None,
    };
    let name = node_text(node.child_by_field_name("name")?, source);
    if !is_identifier_like(name) || name.starts_with('$') {
        return None;
    }
    let spelled = collapse_whitespace(node_text(receiver, source));
    let normalized = normalize_member_access(&spelled);
    let receiver = if is_identifier_like(&normalized) {
        normalized
    } else {
        spelled.chars().take(MAX_CALLEE_TEXT_CHARS).collect()
    };
    Some(PhpMemberCall {
        name: name.to_owned(),
        raw_text: format!("{spelled}{separator}{name}"),
        receiver,
    })
}

/// Rewrites C/C++ and PHP arrow member access to the dot spelling the
/// identifier gate and the receiver split understand.
fn normalize_member_access(callee: &str) -> String {
    callee.replace("?->", ".").replace("->", ".")
}

/// Counts call arguments when the grammar exposes an `arguments` field.
fn call_arity_of(node: Node<'_>) -> Option<usize> {
    let arguments = node
        .child_by_field_name("arguments")
        .or_else(|| node.child_by_field_name("argument_list"))
        .or_else(|| {
            node.child_by_field_name("value")
                .filter(|_| node.kind() == "compound_literal_expression")
        })?;
    let mut cursor = arguments.walk();
    Some(
        arguments
            .named_children(&mut cursor)
            .filter(|child| {
                !matches!(child.kind(), "," | "comment") && !child.kind().ends_with("_comment")
            })
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
    first_ok
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '.' | ':' | '#'))
}

/// Scans an entity body for call sites and buffers one `calls` reference per
/// distinct callee. Nested call expressions inside an argument list or a
/// receiver are collected too: the walk descends declarations only, so it
/// never revisits a call's interior on its own. `boundary` marks subtrees
/// owned by another entity (nested members of a container entity); their call
/// sites are claimed when the walk reaches the owning entity, so they are not
/// collected here.
pub(crate) fn scan_call_edges(
    entity_node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    if !context.is_collecting() {
        return;
    }
    visit_for_calls(entity_node, owner_index, source, context, boundary);
}

fn visit_for_calls(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
    boundary: Option<&dyn Fn(Node<'_>) -> bool>,
) {
    // A call node still descends below: `outer(inner())` and `factory().run()`
    // reach their inner calls only through its subtree, and the declaration
    // walk never comes back for them.
    if let Some((target, ty)) = constructor_target(node, source) {
        let name = ty.rsplit([':', '.']).next().unwrap_or(&ty);
        let raw = if matches!(node.kind(), "object_creation_expression" | "new_expression") {
            format!("new {}", node_text(target, source))
        } else if node.kind() == "compound_literal_expression" {
            format!("{ty}{{}}")
        } else {
            node_text(target, source).to_owned()
        };
        // Keep a receiver so partition cannot bind `new X()` to the class
        // before the resolver has considered X's explicit constructors.
        let position = target.start_position();
        context.add_call_edge(
            owner_index,
            name,
            Some(&ty),
            Some(&raw),
            call_arity_of(node),
            position.row + 1,
            position.column,
        );
    } else if is_call_node(node) && !is_unreported_call(node, source) && is_php_member_call(node) {
        if let Some(call) = php_member_call(node, source) {
            context.add_call_edge(
                owner_index,
                &call.name,
                Some(&call.receiver),
                Some(&call.raw_text),
                call_arity_of(node),
                node.start_position().row + 1,
                node.start_position().column,
            );
        }
    } else if is_call_node(node)
        && !is_unreported_call(node, source)
        && let Some(callee) = callee_text_of(node, source)
    {
        // C/C++ and PHP arrow member access joins the identifier gate and
        // the split under the dot spelling: `ptr->bar()` names the same
        // member shape as `ref.bar()`. The buffered raw text keeps the
        // original spelling.
        let normalized =
            structured_callee(node, source).unwrap_or_else(|| normalize_member_access(&callee));
        let normalized = normalize_super_call(&normalized);
        let intrinsic_jsx = matches!(
            node.kind(),
            "jsx_opening_element" | "jsx_self_closing_element"
        ) && !normalized.contains('.')
            && normalized.chars().next().is_some_and(char::is_lowercase);
        let proven_expression = normalized.rsplit_once('.').is_some_and(|(receiver, name)| {
            receiver.starts_with("ReturnType<typeof ") && is_identifier_like(name)
        });
        if !intrinsic_jsx
            && normalized.len() <= MAX_CALLEE_TEXT_CHARS
            && (is_identifier_like(&normalized) || proven_expression)
        {
            let (ref_name, receiver_name) = split_callee_name(&normalized);
            context.add_call_edge(
                owner_index,
                &ref_name,
                receiver_name.as_deref(),
                Some(
                    if matches!(
                        node.kind(),
                        "jsx_opening_element" | "jsx_self_closing_element"
                    ) {
                        node_text(node, source)
                    } else {
                        callee.as_str()
                    },
                ),
                call_arity_of(node),
                node.start_position().row + 1,
                node.start_position().column,
            );
        }
    }
    for child in named_children(node) {
        if is_call_scan_barrier(child) || boundary.is_some_and(|owns| owns(child)) {
            continue;
        }
        visit_for_calls(child, owner_index, source, context, boundary);
    }
}

/// Keep C++ constructions pending until explicit constructors are considered.
fn cpp_constructor_target<'tree>(
    node: Node<'tree>,
    source: &[u8],
) -> Option<(Node<'tree>, String)> {
    if !matches!(
        node.kind(),
        "new_expression" | "compound_literal_expression"
    ) || node.child_by_field_name("declarator").is_some()
    {
        return None;
    }
    let ty = node.child_by_field_name("type")?;
    // C compound literals use type_descriptor; primitive/array initialization
    // has no user-defined constructor, and templates need additional evidence.
    if !matches!(ty.kind(), "type_identifier" | "qualified_identifier") {
        return None;
    }
    Some((ty, node_text(ty, source).to_owned()))
}

/// Java this/super address constructors; C compound literals are not calls.
fn constructor_target<'tree>(node: Node<'tree>, source: &[u8]) -> Option<(Node<'tree>, String)> {
    if let Some(target) = cpp_constructor_target(node, source) {
        return Some(target);
    }
    match node.kind() {
        "object_creation_expression" => {
            let ty = node.child_by_field_name("type")?;
            if !matches!(
                ty.kind(),
                "type_identifier" | "scoped_type_identifier" | "generic_type"
            ) {
                return None;
            }
            Some((ty, java_type_name(ty, source)?))
        }
        "explicit_constructor_invocation" => {
            let target = node.child_by_field_name("constructor")?;
            Some((target, node_text(target, source).to_owned()))
        }
        _ => None,
    }
}

fn structured_callee(node: Node<'_>, source: &[u8]) -> Option<String> {
    let target = node.child_by_field_name("function")?;
    member_expression(target, source)
}

fn member_expression(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "selector_expression" => {
            let receiver = member_expression(node.child_by_field_name("operand")?, source)?;
            Some(format!(
                "{receiver}.{}",
                node_text(node.child_by_field_name("field")?, source)
            ))
        }
        "member_expression" => {
            let receiver = member_expression(node.child_by_field_name("object")?, source)?;
            let property = node.child_by_field_name("property")?;
            Some(format!("{receiver}.{}", node_text(property, source)))
        }
        "non_null_expression" | "parenthesized_expression" => {
            member_expression(node.named_child(0)?, source)
        }
        "identifier" | "this" | "super" => Some(node_text(node, source).to_owned()),
        "call_expression" | "new_expression" => creation_or_factory_type(node, source),
        _ => None,
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
    if !context.is_collecting() {
        return;
    }
    if matches!(
        format,
        FileFormat::TypeScript | FileFormat::JavaScript | FileFormat::Go | FileFormat::Cpp
    ) && let Some((_, ty)) = function_return_of(entity_node, source, format)
    {
        context.add_declare_binding(
            owner_index,
            "return",
            &ty,
            entity_node.start_position().row + 1,
            entity_node.start_position().column,
        );
    }
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
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && node.kind() == "export_statement"
        && node.child_by_field_name("source").is_none()
        && (node_text(node, source).starts_with("export default")
            || node_text(node, source).starts_with("export ="))
        && let Some(value) = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("declaration"))
            .or_else(|| node.named_child(0))
        && let Some(name) = (value.kind() == "identifier")
            .then_some(value)
            .or_else(|| value.child_by_field_name("name"))
    {
        context.add_declare_binding(
            owner_index,
            "[[export:default]]",
            &format!("typeof {}", node_text(name, source)),
            node.start_position().row + 1,
            node.start_position().column,
        );
    }
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && owner_index == super::FILE_SCOPE_INDEX
        && node.kind() == "variable_declarator"
        && let Some(value) = node.child_by_field_name("value")
        && value.kind() == "call_expression"
        && value
            .child_by_field_name("function")
            .is_some_and(|name| node_text(name, source) == "require")
        && let Some(path) = value
            .child_by_field_name("arguments")
            .and_then(|args| args.named_child(0))
        && path.kind() == "string"
    {
        context.add_import_edge(
            &normalize_module_path(node_text(path, source)),
            Some(node_text(node, source)),
            node.start_position().row + 1,
            node.start_position().column,
        );
    }
    if format == FileFormat::Go {
        collect_go_scoped_binding(node, owner_index, source, context);
    }
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && matches!(node.kind(), "variable_declarator" | "assignment_expression")
    {
        collect_object_bindings(node, owner_index, source, context);
    }
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && node.kind() == "export_specifier"
        && node
            .parent()
            .and_then(|clause| clause.parent())
            .is_none_or(|export| export.child_by_field_name("source").is_none())
        && let (Some(name), Some(alias)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("alias"),
        )
    {
        context.add_declare_binding(
            owner_index,
            // Export names are not local bindings; named exports remain live.
            &format!("[[export:{}]]", node_text(alias, source)),
            &format!("export typeof {}", node_text(name, source)),
            node.start_position().row + 1,
            node.start_position().column,
        );
    }
    let binding = declaration_binding_of(node, owner_index, source, format, context);
    if let Some((name, bound_type)) = binding {
        if format == FileFormat::TypeScript
            && node.kind() == "required_parameter"
            && node_text(node, source)
                .split_whitespace()
                .next()
                .is_some_and(|word| matches!(word, "public" | "private" | "protected" | "readonly"))
        {
            context.add_declare_binding(
                owner_index,
                &format!("this.{name}"),
                &bound_type,
                node.start_position().row + 1,
                node.start_position().column,
            );
        }
        collect_binding(node, owner_index, &name, &bound_type, format, context);
        if format == FileFormat::Java
            && node.kind() == "enhanced_for_statement"
            && let Some(body) = node.child_by_field_name("body")
        {
            visit_for_bindings(body, owner_index, source, format, context, boundary);
        }
        return;
    }
    for child in named_children(node) {
        if boundary.is_some_and(|owns| owns(child)) {
            continue;
        }
        visit_for_bindings(child, owner_index, source, format, context, boundary);
    }
}

fn collect_binding(
    node: Node<'_>,
    owner_index: usize,
    name: &str,
    ty: &str,
    format: FileFormat,
    context: &mut WalkContext,
) {
    if format == FileFormat::Cpp {
        let mut scope = node;
        while !matches!(
            scope.kind(),
            "compound_statement" | "function_definition" | "for_range_loop" | "for_statement"
        ) {
            let Some(parent) = scope.parent() else { break };
            scope = parent;
        }
        if node.kind() == "for_range_loop" {
            if let Some(body) = node.child_by_field_name("body") {
                add_binding_in_scope(body, Some(body), owner_index, name, ty, context);
            }
        } else {
            add_binding_in_scope(node, Some(scope), owner_index, name, ty, context);
        }
        return;
    }
    if format == FileFormat::Java {
        if node.kind() == "enhanced_for_statement" {
            // The loop variable is not in scope in the iterable expression.
            if let Some(body) = node.child_by_field_name("body") {
                add_binding_in_scope(body, Some(body), owner_index, name, ty, context);
            }
        } else {
            let scope = node
                .parent()
                .filter(|parent| parent.kind() == "local_variable_declaration")
                .and_then(|parent| parent.parent());
            add_binding_in_scope(node, scope, owner_index, name, ty, context);
        }
        return;
    }
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && node.kind() == "variable_declarator"
    {
        // The local name already shadows outer bindings before its
        // initializer, but has no callable value yet (TDZ or undefined).
        let scope = javascript_variable_scope(node);
        context.add_scoped_binding(
            owner_index,
            name,
            "?",
            (
                scope.start_position().row + 1,
                scope.start_position().column,
            ),
            (node.start_position().row + 1, node.start_position().column),
        );
        add_binding_in_scope(node, Some(scope), owner_index, name, ty, context);
    } else if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && node.kind() == "assignment_expression"
    {
        let end = context.assignment_scope_end(name, node.start_byte());
        add_binding_until(node, end, owner_index, name, ty, context);
    } else {
        add_binding_in_scope(node, None, owner_index, name, ty, context);
    }
}

fn javascript_variable_scope(mut node: Node<'_>) -> Node<'_> {
    // `var` belongs to the function; `let`/`const` belong to the nearest
    // block or loop, including declarations in a loop initializer.
    let function_scoped = node
        .parent()
        .is_some_and(|parent| parent.kind() == "variable_declaration");
    while let Some(parent) = node.parent() {
        node = parent;
        if node.kind() == "program" {
            break;
        }
        if matches!(
            node.kind(),
            "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "method_definition"
                | "generator_function"
                | "generator_function_declaration"
        ) {
            return node.child_by_field_name("body").unwrap_or(node);
        }
        if !function_scoped
            && matches!(
                node.kind(),
                "statement_block" | "for_statement" | "for_in_statement" | "switch_body"
            )
        {
            break;
        }
    }
    node
}

// Register declarations before collecting values, including declarations that
// appear after an assignment. Scope lookup must not inherit a TDZ marker's end
// or use the assignment's own block when it updates an outer binding.
fn collect_javascript_binding_scopes(node: Node<'_>, source: &[u8], context: &mut WalkContext) {
    let binding = match node.kind() {
        "variable_declarator" => node
            .child_by_field_name("name")
            .map(|name| (name, javascript_variable_scope(node))),
        "formal_parameters" => node.parent().map(|function| (node, function)),
        "arrow_function" => node
            .child_by_field_name("parameter")
            .map(|name| (name, node)),
        "catch_clause" => node
            .child_by_field_name("parameter")
            .zip(node.child_by_field_name("body")),
        "for_in_statement" => {
            let mut cursor = node.walk();
            let lexical = node
                .children_by_field_name("kind", &mut cursor)
                .any(|kind| matches!(node_text(kind, source), "let" | "const"));
            lexical
                .then(|| node.child_by_field_name("left").map(|name| (name, node)))
                .flatten()
        }
        _ => None,
    };
    if let Some((pattern, scope)) = binding {
        register_javascript_pattern_scope(pattern, scope, source, context);
    }
    for child in named_children(node) {
        collect_javascript_binding_scopes(child, source, context);
    }
}

fn register_javascript_pattern_scope(
    pattern: Node<'_>,
    scope: Node<'_>,
    source: &[u8],
    context: &mut WalkContext,
) {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            context.register_binding_scope(
                node_text(pattern, source),
                scope.start_byte()..scope.end_byte(),
                (scope.kind() != "program")
                    .then(|| (scope.end_position().row + 1, scope.end_position().column)),
            );
        }
        "pair_pattern" => {
            if let Some(value) = pattern.child_by_field_name("value") {
                register_javascript_pattern_scope(value, scope, source, context);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = pattern.child_by_field_name("left") {
                register_javascript_pattern_scope(left, scope, source, context);
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(name) = pattern.child_by_field_name("pattern") {
                register_javascript_pattern_scope(name, scope, source, context);
            }
        }
        "rest_pattern" | "array_pattern" | "object_pattern" | "formal_parameters" => {
            for child in named_children(pattern) {
                register_javascript_pattern_scope(child, scope, source, context);
            }
        }
        _ => {}
    }
}

fn add_binding_in_scope(
    node: Node<'_>,
    scope: Option<Node<'_>>,
    owner: usize,
    name: &str,
    ty: &str,
    context: &mut WalkContext,
) {
    let end = scope
        .filter(|scope| scope.kind() != "program")
        .map(|scope| (scope.end_position().row + 1, scope.end_position().column));
    add_binding_until(node, end, owner, name, ty, context);
}

fn add_binding_until(
    node: Node<'_>,
    end: Option<(usize, usize)>,
    owner: usize,
    name: &str,
    ty: &str,
    context: &mut WalkContext,
) {
    let start = (node.start_position().row + 1, node.start_position().column);
    if let Some(end) = end {
        context.add_scoped_binding(owner, name, ty, start, end);
    } else {
        context.add_declare_binding(owner, name, ty, start.0, start.1);
    }
}

fn collect_go_scoped_binding(
    node: Node<'_>,
    owner: usize,
    source: &[u8],
    context: &mut WalkContext,
) {
    let bound = match node.kind() {
        "type_case" => {
            let Some(switch) = node
                .parent()
                .filter(|parent| parent.kind() == "type_switch_statement")
            else {
                return;
            };
            let Some(alias) = switch
                .child_by_field_name("alias")
                .and_then(|alias| alias.named_child(0))
            else {
                return;
            };
            let mut cursor = node.walk();
            let types: Vec<_> = node
                .children_by_field_name("type", &mut cursor)
                .filter(Node::is_named)
                .collect();
            let [ty] = types.as_slice() else { return };
            type_name_of(*ty, source).map(|ty| (node_text(alias, source).to_owned(), ty, node))
        }
        "range_clause" => {
            let Some(value) = node.child_by_field_name("right") else {
                return;
            };
            let Some(name) = node
                .child_by_field_name("left")
                .and_then(|left| left.named_child(1))
            else {
                return;
            };
            let Some(parent) = node.parent() else { return };
            context
                .name_edges
                .iter()
                .rev()
                .find(|edge| {
                    edge.owner_index == owner
                        && edge.ref_kind == GraphEdgeKind::Declares
                        && edge.ref_name == node_text(value, source)
                })
                .and_then(|edge| edge.bound_type.as_deref())
                .and_then(|ty| ty.strip_prefix("[]"))
                .map(|ty| (node_text(name, source).to_owned(), ty.to_owned(), parent))
        }
        _ => None,
    };
    if let Some((name, ty, scope)) = bound {
        context.add_scoped_binding(
            owner,
            &name,
            &ty,
            (node.start_position().row + 1, node.start_position().column),
            (scope.end_position().row + 1, scope.end_position().column),
        );
    }
}

fn collect_object_bindings(node: Node<'_>, owner: usize, source: &[u8], context: &mut WalkContext) {
    let Some(name) = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("left"))
    else {
        return;
    };
    let Some(mut value) = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("right"))
    else {
        return;
    };
    while value.kind() == "parenthesized_expression" {
        let Some(inner) = value.named_child(0) else {
            return;
        };
        value = inner;
    }
    if value.kind() == "call_expression"
        && value
            .child_by_field_name("function")
            .is_some_and(|function| node_text(function, source) == "Object.freeze")
        && let Some(argument) = value
            .child_by_field_name("arguments")
            .and_then(|arguments| arguments.named_child(0))
    {
        value = argument;
    }
    if value.kind() != "object" {
        return;
    }
    let Some(object) = member_expression(name, source) else {
        return;
    };
    let end = if node.kind() == "variable_declarator" {
        let scope = javascript_variable_scope(node);
        (scope.kind() != "program")
            .then(|| (scope.end_position().row + 1, scope.end_position().column))
    } else {
        context.assignment_scope_end(&object, node.start_byte())
    };
    let mut keys = std::collections::BTreeSet::new();
    for property in named_children(value) {
        let key = property
            .child_by_field_name("key")
            .or_else(|| property.child_by_field_name("name"))
            .or_else(|| (property.kind() == "shorthand_property_identifier").then_some(property));
        let key = key
            .and_then(|key| {
                if key.kind() == "computed_property_name" {
                    key.named_child(0).filter(|key| key.kind() == "string")
                } else {
                    Some(key)
                }
            })
            .map(|key| node_text(key, source).trim_matches(['\'', '"']))
            .filter(|key| is_identifier_like(key));
        let Some(key) = key else {
            // An unknown spread/computed key can overwrite every preceding property.
            for key in &keys {
                add_binding_until(
                    property,
                    end,
                    owner,
                    &format!("{object}.{key}"),
                    "?",
                    context,
                );
            }
            continue;
        };
        keys.insert(key.to_owned());
        let value = property
            .child_by_field_name("value")
            .or_else(|| (property.kind() == "shorthand_property_identifier").then_some(property));
        let ty = match value.map(|value| value.kind()) {
            Some("identifier" | "shorthand_property_identifier") => format!(
                "typeof {}",
                node_text(value.expect("property value"), source)
            ),
            Some("arrow_function" | "function_expression")
                if node.kind() == "variable_declarator" =>
            {
                format!("typeof {object}::{key}")
            }
            _ if property.kind() == "method_definition" && node.kind() == "variable_declarator" => {
                format!("typeof {object}::{key}")
            }
            _ => "?".to_owned(),
        };
        add_binding_until(
            property,
            end,
            owner,
            &format!("{object}.{key}"),
            &ty,
            context,
        );
    }
}

/// Extracts `(bound name, declared type)` from a declaration node whose
/// shape one of the per-language readers recognizes.
fn declaration_binding_of(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    format: FileFormat,
    context: &WalkContext,
) -> Option<(String, String)> {
    match format {
        FileFormat::Java => java_binding_of(node, source, context),
        FileFormat::Python => python_binding_of(node, source, context),
        FileFormat::Rust => rust_binding_of(node, source, context),
        FileFormat::Go => go_binding_of(node, source, context),
        FileFormat::Cpp => cpp_binding_of(node, owner_index, source, context),
        FileFormat::C => c_binding_of(node, source),
        FileFormat::TypeScript => ts_binding_of(node, source),
        FileFormat::JavaScript => js_binding_of(node, source),
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
    if !context.is_collecting() {
        return;
    }
    if matches!(format, FileFormat::JavaScript | FileFormat::TypeScript) {
        collect_javascript_binding_scopes(root, source, context);
    }
    visit_for_return_types(root, source, format, context);
    if format == FileFormat::Cpp {
        collect_cpp_type_aliases(root, source, &[], context);
    }
    let script_scope = match format {
        FileFormat::JavaScript => Some(if has_javascript_module_syntax(root, source) {
            "module"
        } else {
            "script"
        }),
        FileFormat::TypeScript if !has_javascript_module_syntax(root, source) => Some("script"),
        _ => None,
    };
    if let Some(scope) = script_scope {
        // Only explicit script evidence authorizes cross-file global types.
        // A missing marker (including an older index) is not global evidence.
        context.add_declare_binding(
            super::FILE_SCOPE_INDEX,
            if format == FileFormat::TypeScript {
                "[[typescript_scope]]"
            } else {
                "[[javascript_scope]]"
            },
            scope,
            1,
            0,
        );
    }
    if matches!(
        format,
        FileFormat::TypeScript | FileFormat::JavaScript | FileFormat::Go
    ) {
        let boundary = |node: Node<'_>| {
            matches!(
                node.kind(),
                "function_declaration"
                    | "generator_function_declaration"
                    | "generator_function"
                    | "func_literal"
                    | "class"
                    | "method_definition"
                    | "method_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "type_spec"
                    | "function_expression"
                    | "arrow_function"
            )
        };
        visit_for_bindings(
            root,
            super::FILE_SCOPE_INDEX,
            source,
            format,
            context,
            Some(&boundary),
        );
    }
}

/// C++ type aliases belong to their declaring namespace/class, including using
/// declarations that do not produce a searchable entity. Keep them in the
/// existing file-owned declaration evidence rather than adding an index table.
fn collect_cpp_type_aliases(
    node: Node<'_>,
    source: &[u8],
    scopes: &[String],
    context: &mut WalkContext,
) {
    if node.kind() == "template_declaration" {
        collect_cpp_template_parameters(node, source, scopes, context);
    }
    if matches!(node.kind(), "function_definition" | "lambda_expression") {
        return;
    }
    let alias = match node.kind() {
        "alias_declaration" => node
            .child_by_field_name("name")
            .zip(node.child_by_field_name("type"))
            .map(|(name, ty)| {
                (
                    node_text(name, source).to_owned(),
                    node_text(ty, source).to_owned(),
                )
            }),
        "using_declaration"
            if !node_text(node, source)
                .split_whitespace()
                .take(2)
                .eq(["using", "namespace"]) =>
        {
            named_children(node)
                .into_iter()
                .find(|child| child.kind() == "qualified_identifier")
                .map(|target| {
                    let target = node_text(target, source).to_owned();
                    (
                        target.rsplit("::").next().unwrap_or(&target).to_owned(),
                        target,
                    )
                })
        }
        _ => None,
    };
    if let Some((name, target)) = alias {
        let name = scopes
            .iter()
            .map(String::as_str)
            .chain([name.as_str()])
            .collect::<Vec<_>>()
            .join("::");
        context.add_declare_binding(
            super::FILE_SCOPE_INDEX,
            &name,
            &target,
            node.start_position().row + 1,
            node.start_position().column,
        );
    }
    let mut scopes = scopes.to_vec();
    if matches!(
        node.kind(),
        "namespace_definition" | "class_specifier" | "struct_specifier"
    ) && let Some(name) = node.child_by_field_name("name")
    {
        scopes.push(node_text(name, source).to_owned());
    }
    for child in named_children(node) {
        collect_cpp_type_aliases(child, source, &scopes, context);
    }
}

/// Unbound template parameters shadow workspace names; erasing class arguments
/// must never turn T into an unrelated project class named T.
fn collect_cpp_template_parameters(
    node: Node<'_>,
    source: &[u8],
    scopes: &[String],
    context: &mut WalkContext,
) {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return;
    };
    let Some(declaration) = named_children(node).into_iter().rev().find(|child| {
        matches!(
            child.kind(),
            "class_specifier"
                | "struct_specifier"
                | "function_definition"
                | "alias_declaration"
                | "declaration"
        )
    }) else {
        return;
    };
    let name = declaration.child_by_field_name("name").or_else(|| {
        let mut declarator = declaration.child_by_field_name("declarator")?;
        while let Some(inner) = declarator.child_by_field_name("declarator") {
            declarator = inner;
        }
        Some(declarator)
    });
    let Some(name) = name else { return };
    let owner = scopes
        .iter()
        .map(String::as_str)
        .chain([node_text(name, source)])
        .collect::<Vec<_>>()
        .join("::");
    for parameter in named_children(parameters) {
        if !matches!(
            parameter.kind(),
            "type_parameter_declaration"
                | "optional_type_parameter_declaration"
                | "variadic_type_parameter_declaration"
        ) {
            continue;
        }
        if let Some(name) = parameter
            .child_by_field_name("name")
            .or_else(|| parameter.named_child(0))
        {
            context.add_declare_binding(
                super::FILE_SCOPE_INDEX,
                &format!("{owner}::{}", node_text(name, source)),
                "?",
                parameter.start_position().row + 1,
                parameter.start_position().column,
            );
        }
    }
}

fn has_javascript_module_syntax(node: Node<'_>, source: &[u8]) -> bool {
    matches!(node.kind(), "import_statement" | "export_statement")
        // Treat CommonJS evidence conservatively, even when the identifier
        // could be shadowed. It must not enable project-global class lookup.
        || (node.kind() == "identifier"
            && matches!(node_text(node, source), "require" | "module" | "exports"))
        || named_children(node)
            .into_iter()
            .any(|child| has_javascript_module_syntax(child, source))
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
    if !matches!(
        node.kind(),
        "function_definition" | "declaration" | "field_declaration"
    ) {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    let mut type_name = node_text(type_node, source).to_owned();
    if type_node.kind() == "primitive_type" {
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
        if declarator.kind() == "pointer_declarator" {
            type_name.push('*');
        }
        if declarator.kind() == "reference_declarator" {
            type_name.push('&');
        }
        declarator = declarator.child_by_field_name("declarator")?;
    }
    if !matches!(
        declarator.kind(),
        "identifier" | "field_identifier" | "qualified_identifier"
    ) {
        return None;
    }
    Some((node_text(declarator, source).to_owned(), type_name))
}

fn ts_return_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if !matches!(node.kind(), "function_declaration" | "method_definition") {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    if !matches!(name.kind(), "identifier" | "property_identifier") {
        return None;
    }
    let annotation = node.child_by_field_name("return_type")?;
    if annotation.kind() != "type_annotation" {
        return None;
    }
    let inner = annotation.named_child(0)?;
    if !matches!(inner.kind(), "type_identifier" | "generic_type") {
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

/// Generic arguments do not change the declaring Java class of a member.
fn java_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    // An array has its own members, not its element type's method set.
    if node.kind() == "array_type" {
        return None;
    }
    let node = if node.kind() == "generic_type" {
        node.named_child(0)?
    } else {
        node
    };
    type_name_of(node, source)
}

fn java_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    match node.kind() {
        "type_parameter" => {
            let children = named_children(node);
            let name = children
                .iter()
                .find(|child| child.kind() == "type_identifier")?;
            let bounds = children.iter().find(|child| child.kind() == "type_bound")?;
            // Intersections need member selection across all bounds; do not
            // claim the first bound is the complete receiver type.
            if bounds.named_child_count() != 1 {
                return None;
            }
            Some((
                format!("[[type:{}]]", node_text(*name, source)),
                java_type_name(bounds.named_child(0)?, source)?,
            ))
        }
        "enum_constant" => {
            let name = node.child_by_field_name("name")?;
            let declaration = node.parent()?.parent()?;
            if declaration.kind() != "enum_declaration" {
                return None;
            }
            Some((
                node_text(name, source).to_owned(),
                node_text(declaration.child_by_field_name("name")?, source).to_owned(),
            ))
        }

        // An explicit type selects the static method target. Inferred `var`
        // declarations use a creation expression or known factory return type.
        "variable_declarator" => {
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            // Explicit Java types remain authoritative, including fields
            // without initializers and variables initialized to null.
            if let Some(ty) = node.parent()?.child_by_field_name("type")
                && node_text(ty, source) != "var"
            {
                return Some((
                    node_text(name, source).to_owned(),
                    java_type_name(ty, source)?,
                ));
            }
            let value = node.child_by_field_name("value")?;
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
        "formal_parameter" | "enhanced_for_statement" => {
            let name = node.child_by_field_name("name")?;
            let type_node = node.child_by_field_name("type")?;
            let type_name = java_type_name(type_node, source)?;
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
    if !matches!(node.kind(), "let_declaration" | "parameter") {
        return None;
    }
    let pattern = node.child_by_field_name("pattern")?;
    if pattern.kind() != "identifier" {
        return None;
    }
    if let Some(ty) = node.child_by_field_name("type") {
        let ty = if ty.kind() == "reference_type" {
            ty.child_by_field_name("type")?
        } else {
            ty
        };
        if matches!(ty.kind(), "type_identifier" | "scoped_type_identifier")
            && let Some(ty) = type_name_of(ty, source)
        {
            return Some((node_text(pattern, source).to_owned(), ty));
        }
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
    let (name, declared, value) = match node.kind() {
        "short_var_declaration" => (
            node.child_by_field_name("left")?.named_child(0)?,
            None,
            node.child_by_field_name("right")?.named_child(0),
        ),
        "var_spec" => (
            node.child_by_field_name("name")?,
            node.child_by_field_name("type"),
            node.child_by_field_name("value")
                .and_then(|value| value.named_child(0)),
        ),
        "parameter_declaration" | "field_declaration" | "type_parameter_declaration" => (
            node.child_by_field_name("name")?,
            node.child_by_field_name("type"),
            None,
        ),
        _ => return None,
    };
    if !matches!(name.kind(), "identifier" | "field_identifier") {
        return None;
    }
    let bound_type = declared
        .and_then(|ty| {
            if ty.kind() == "slice_type" {
                Some(node_text(ty, source).to_owned())
            } else {
                type_name_of(ty, source)
            }
        })
        .or_else(|| value.and_then(|value| go_initializer_type(value, source, context)))
        // An unknown parameter/local still shadows a same-named function.
        // `?` cannot name a source type and must never resolve by name.
        .unwrap_or_else(|| "?".to_owned());
    Some((node_text(name, source).to_owned(), bound_type))
}

fn go_initializer_type(node: Node<'_>, source: &[u8], context: &WalkContext) -> Option<String> {
    match node.kind() {
        "unary_expression" if node_text(node, source).starts_with('&') => {
            go_initializer_type(node.child_by_field_name("operand")?, source, context)
        }
        "composite_literal" => type_name_of(node.child_by_field_name("type")?, source),
        "call_expression" => {
            let function = node.child_by_field_name("function")?;
            if function.kind() != "identifier" {
                return None;
            }
            let callee = node_text(function, source);
            if callee == "new" {
                return type_name_of(
                    node.child_by_field_name("arguments")?.named_child(0)?,
                    source,
                );
            }
            context.factory_return_of(callee).map(str::to_owned)
        }
        _ => None,
    }
}

fn cpp_binding_of(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    match node.kind() {
        // `auto x = X{}` / `auto x = new X()`: the placeholder type defers
        // to the initializer's spelled type. `auto x = makeX()`: a bare
        // call binds the registered factory's return type.
        "declaration" => {
            let declared = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            // Tree-sitter can parse `Pointer value(raw)` as a prototype. A
            // parameter-shaped token already bound as a value proves this is
            // direct initialization; an empty list remains a function declaration.
            if declarator.kind() == "function_declarator"
                && declared.kind() != "placeholder_type_specifier"
            {
                let parameters = named_children(declarator.child_by_field_name("parameters")?);
                if !parameters.is_empty()
                    && parameters.iter().all(|parameter| {
                        parameter.child_by_field_name("declarator").is_none()
                            && parameter.child_by_field_name("type").is_some_and(|ty| {
                                context.name_edges.iter().rev().any(|edge| {
                                    edge.ref_kind == GraphEdgeKind::Declares
                                        && edge.owner_index == owner_index
                                        && edge.ref_name == node_text(ty, source)
                                        && (edge.line, edge.column)
                                            < (
                                                node.start_position().row + 1,
                                                node.start_position().column,
                                            )
                                        && edge.scope_end.is_none_or(|end| {
                                            (
                                                node.start_position().row + 1,
                                                node.start_position().column,
                                            ) < end
                                        })
                                })
                            })
                    })
                {
                    return Some((
                        peel_declarator_name(
                            declarator.child_by_field_name("declarator")?,
                            source,
                        )?,
                        node_text(declared, source).to_owned(),
                    ));
                }
                return None;
            }
            let name = if declarator.kind() == "init_declarator" {
                declarator.child_by_field_name("declarator")?
            } else {
                declarator
            };
            let name = peel_declarator_name(name, source)?;
            if declared.kind() != "placeholder_type_specifier" {
                return Some((name, cpp_declared_type(declared, declarator, source)));
            }
            let value = declarator.child_by_field_name("value")?;
            let type_node = match value.kind() {
                "new_expression" | "compound_literal_expression" => {
                    value.child_by_field_name("type")?
                }
                "call_expression" | "identifier" => {
                    return Some((name, format!("decltype({})", node_text(value, source))));
                }
                _ => return None,
            };
            Some((
                name,
                format!(
                    "{}{}",
                    node_text(type_node, source),
                    if value.kind() == "new_expression" {
                        "*"
                    } else {
                        ""
                    }
                ),
            ))
        }
        // `void m(Left &a)`: the parameter name sits under the reference
        // declarator wrapper.
        "parameter_declaration" | "for_range_loop" => {
            let type_node = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            if type_node.kind() == "placeholder_type_specifier" {
                return None;
            }
            let name = peel_declarator_name(declarator, source)?;
            Some((name, cpp_declared_type(type_node, declarator, source)))
        }
        _ => None,
    }
}

fn cpp_declared_type(ty: Node<'_>, declarator: Node<'_>, source: &[u8]) -> String {
    let mut ty = node_text(ty, source).to_owned();
    let mut declarator = declarator;
    loop {
        if declarator.kind() == "pointer_declarator" {
            ty.push('*');
        }
        if declarator.kind() == "reference_declarator" {
            ty.push('&');
        }
        let Some(inner) = declarator
            .child_by_field_name("declarator")
            .or_else(|| declarator.named_child(0))
        else {
            break;
        };
        declarator = inner;
    }
    ty
}

/// Nullability does not change the target of a member call that executes.
/// Several concrete union members still need branch/type evidence.
fn ts_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut members = node_text(node, source)
        .split('|')
        .map(str::trim)
        .filter(|member| !matches!(*member, "null" | "undefined"));
    let ty = members.next()?;
    (members.next().is_none() && is_identifier_like(ty.strip_prefix("typeof ").unwrap_or(ty)))
        .then(|| ty.to_owned())
}

fn ts_binding_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    match node.kind() {
        "public_field_definition" | "field_definition" => {
            let name = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("property"))?;
            let ty = node
                .child_by_field_name("type")
                .and_then(|annotation| annotation.named_child(0))
                .and_then(|ty| ts_type_name(ty, source))
                .or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|value| creation_or_factory_type(value, source))
                })
                .unwrap_or_else(|| "?".to_owned());
            Some((node_text(name, source).to_owned(), ty))
        }
        // `const x = new X()`: the constructor expression spells the type.
        // `const x = makeX()`: a bare call binds the registered factory's
        // return type.
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            if matches!(
                value.kind(),
                "function_expression" | "arrow_function" | "class"
            ) || (value.kind() == "call_expression"
                && value
                    .child_by_field_name("function")
                    .is_some_and(|function| node_text(function, source) == "require"))
            {
                return None;
            }
            let type_name =
                creation_or_factory_type(value, source).unwrap_or_else(|| "?".to_owned());
            Some((node_text(name, source).to_owned(), type_name))
        }
        // `x = new X()`: a reassignment rebinds the name; the resolver
        // orders the bindings by line so calls after it see the new type.
        "assignment_expression" => {
            let left = node.child_by_field_name("left")?;
            let name = member_expression(left, source)?;
            let value = node.child_by_field_name("right")?;
            let type_name =
                creation_or_factory_type(value, source).unwrap_or_else(|| "?".to_owned());
            Some((name, type_name))
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
            Some((
                node_text(pattern, source).to_owned(),
                ts_type_name(inner, source).unwrap_or_else(|| "?".to_owned()),
            ))
        }
        _ => None,
    }
}

/// The type a creation or bare-call initializer spells: a `new X()`
/// constructor names `X`, a bare `makeX()` call binds the registered
/// factory's return type, and anything else carries no locally spellable
/// type.
fn creation_or_factory_type(value: Node<'_>, source: &[u8]) -> Option<String> {
    if value.kind() == "await_expression" {
        let returned = creation_or_factory_type(value.named_child(0)?, source)?;
        return Some(
            returned
                .strip_prefix("Promise<")
                .and_then(|ty| ty.strip_suffix('>'))
                .map_or_else(|| format!("Awaited<{returned}>"), str::to_owned),
        );
    }
    if value.kind() == "parenthesized_expression" {
        return creation_or_factory_type(value.named_child(0)?, source);
    }
    if value.kind() == "call_expression" {
        let function = value.child_by_field_name("function")?;
        if function.kind() == "selector_expression" {
            return Some(format!(
                "ReturnType<typeof {}>",
                node_text(function, source)
            ));
        }
        if function.kind() == "member_expression"
            && let Some(object) = function.child_by_field_name("object")
            && object.kind() == "new_expression"
        {
            let class = object.child_by_field_name("constructor")?;
            let method = function.child_by_field_name("property")?;
            return Some(format!(
                "ReturnType<typeof {}::{}>",
                node_text(class, source),
                node_text(method, source)
            ));
        }
        if function.kind() != "identifier" {
            return None;
        }
        let name = node_text(function, source);
        return Some(format!("ReturnType<typeof {name}>"));
    }
    if value.kind() == "identifier" {
        return Some(format!("typeof {}", node_text(value, source)));
    }
    if value.kind() != "new_expression" {
        return None;
    }
    let constructor = value.child_by_field_name("constructor")?;
    (constructor.kind() == "identifier").then(|| node_text(constructor, source).to_owned())
}

fn js_binding_of(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    match node.kind() {
        "field_definition" => ts_binding_of(node, source),
        "identifier"
            if node.parent().is_some_and(|parent| {
                parent.kind() == "formal_parameters"
                    || (parent.kind() == "arrow_function"
                        && parent.child_by_field_name("parameter") == Some(node))
            }) =>
        {
            Some((node_text(node, source).to_owned(), "?".to_owned()))
        }
        "assignment_pattern" | "rest_pattern"
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == "formal_parameters") =>
        {
            let name = node.named_child(0)?;
            (name.kind() == "identifier")
                .then(|| (node_text(name, source).to_owned(), "?".to_owned()))
        }
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
            if matches!(
                value.kind(),
                "function_expression" | "arrow_function" | "class"
            ) || (value.kind() == "call_expression"
                && value
                    .child_by_field_name("function")
                    .is_some_and(|function| node_text(function, source) == "require"))
            {
                return None;
            }
            let type_name =
                creation_or_factory_type(value, source).unwrap_or_else(|| "?".to_owned());
            Some((node_text(name, source).to_owned(), type_name))
        }
        // `x = new X()`: a reassignment rebinds the name; the resolver
        // orders the bindings by line so calls after it see the new type.
        "assignment_expression" => {
            let left = node.child_by_field_name("left")?;
            let name = member_expression(left, source)?;
            let value = node.child_by_field_name("right")?;
            let type_name =
                creation_or_factory_type(value, source).unwrap_or_else(|| "?".to_owned());
            Some((name, type_name))
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
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript)
        && let Some(clause) = named_children(node)
            .into_iter()
            .find(|child| child.kind() == "import_require_clause")
        && let Some(path) = clause.child_by_field_name("source")
    {
        return Some(normalize_module_path(node_text(path, source)));
    }
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
                    let wildcard = node
                        .named_children(&mut node.walk())
                        .any(|child| child.kind() == "asterisk");
                    return Some(if wildcard { format!("{path}.*") } else { path });
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
        && (node.kind() != "export_statement" || node.child_by_field_name("source").is_some())
}

/// Buffers an `imports` reference for an import declaration node. The owner
/// is the file scope: imports belong to the file, not to any entity.
pub(crate) fn collect_import_edge(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
) {
    if !context.is_collecting() {
        return;
    }
    if format == FileFormat::Go && node.kind() == "import_declaration" {
        // Visit each import_spec so grouped imports retain their own alias.
        return;
    }
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
    if !context.is_collecting() {
        return;
    }
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
                    Some((GraphEdgeKind::Extends, descendant))
                } else if implements_types.contains(&descendant.kind()) {
                    Some((GraphEdgeKind::Implements, descendant))
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
                    .map(|clause| (GraphEdgeKind::Extends, clause))
            })
            .chain(implements.iter().filter_map(|field| {
                entity_node
                    .child_by_field_name(field)
                    .map(|clause| (GraphEdgeKind::Implements, clause))
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
    fn file_binding_scan_stops_at_callable_and_class_bodies() {
        for (format, grammar, source) in [
            (
                FileFormat::JavaScript,
                tree_sitter_javascript::LANGUAGE,
                "function* generator() { var hidden = 1; } const f = function* () { var hidden = 2; }; const object = { method(hidden) { var local = 3; } }; const C = class { method(hidden) { var local = 4; } };",
            ),
            (
                FileFormat::TypeScript,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                "function* generator() { var hidden = 1; } const f = function* () { var hidden = 2; }; const object = { method(hidden: number) { var local = 3; } }; const C = class { method(hidden: number) { var local = 4; } };",
            ),
            (
                FileFormat::Go,
                tree_sitter_go::LANGUAGE,
                "package main\nvar f = func(hidden int) { var local int }\n",
            ),
        ] {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&grammar.into())
                .expect("fixture grammar");
            let tree = parser.parse(source, None).expect("fixture syntax tree");
            let mut context = WalkContext::new();
            scan_return_types(tree.root_node(), source.as_bytes(), format, &mut context);
            assert!(
                context
                    .name_edges()
                    .iter()
                    .all(|edge| !matches!(edge.ref_name.as_str(), "hidden" | "local")),
                "{format:?}"
            );
        }
    }

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
            vec![("x".to_owned(), "X*".to_owned(), 1)]
        );
        assert_eq!(
            bindings(
                FileFormat::TypeScript,
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "function m() { const x = new X(); x.foo(); }",
            ),
            vec![
                ("x".to_owned(), "?".to_owned(), 1),
                ("x".to_owned(), "X".to_owned(), 1),
            ]
        );
        assert_eq!(
            bindings(
                FileFormat::JavaScript,
                &tree_sitter_javascript::LANGUAGE.into(),
                "function m() { const x = new X(); x.foo(); }",
            ),
            vec![
                ("x".to_owned(), "?".to_owned(), 1),
                ("x".to_owned(), "X".to_owned(), 1),
            ]
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
                ("a".to_owned(), "Left&".to_owned(), 1),
                ("b".to_owned(), "Right&".to_owned(), 1),
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
            vec![
                ("[[typescript_scope]]".to_owned(), "script".to_owned(), 1),
                ("x".to_owned(), "?".to_owned(), 4),
                ("x".to_owned(), "ReturnType<typeof makeX>".to_owned(), 5),
            ]
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
            vec![("x".to_owned(), "decltype(makeX())".to_owned(), 5)]
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
            vec![
                ("[[javascript_scope]]".to_owned(), "script".to_owned(), 1),
                ("x".to_owned(), "?".to_owned(), 4),
                ("x".to_owned(), "ReturnType<typeof makeX>".to_owned(), 5),
            ]
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

        // The parameter has a known type, but the member-call result must
        // not borrow a same-named free factory's return type.
        assert_eq!(
            factory_bindings(
                FileFormat::Rust,
                &tree_sitter_rust::LANGUAGE.into(),
                "fn make_x() -> X {\n    todo!()\n}\nfn caller(v: V) {\n    let m = v.make_x();\n}\n",
            ),
            vec![("v".to_owned(), "V".to_owned(), 4)]
        );
    }
}
