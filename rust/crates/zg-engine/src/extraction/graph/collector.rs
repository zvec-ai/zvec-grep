//! Walk-time collectors for name-based graph references.
//!
//! These run inside entity bodies — the region the symbol walker deliberately
//! does not descend into — so call sites are attributed to their innermost
//! owning entity without a second full parse. Mirrors the TypeScript
//! `src/engine/extraction/graph/edge-collector.ts` implementation.

use tree_sitter::Node;

use crate::domain::{FileFormat, GraphEdgeKind};
use crate::utils::collapse_whitespace;

use super::vocabulary::{
    CALLEE_FIELDS, COMMON_CALL_TYPES, HeritageVocabulary, heritage_vocabulary_for,
    import_vocabulary_for,
};
use super::{Constructs, FILE_SCOPE_INDEX, WalkContext};

const MAX_CALLEE_TEXT_CHARS: usize = 180;

// Local mirrors of the `code::adapter` node helpers, which are not visible
// from this module tree.
fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub(crate) fn node_text<'source>(node: Node<'_>, source: &'source [u8]) -> &'source str {
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

/// The member name, receiver and raw text a call site names.
///
/// C/C++ arrow member access joins the identifier gate and the split under the
/// dot spelling: `ptr->bar()` names the same member shape as `ref.bar()`. The
/// raw text keeps the original spelling. A receiver the gate rejects because it
/// reads through something other than a name (`statuses[0].ok()`,
/// `(*it).second.Flush()`) is answered by the path the expression spells, which
/// the resolver evaluates the same way it evaluates a loop variable.
fn callee_of(node: Node<'_>, source: &[u8]) -> Option<(String, Option<String>, String)> {
    let callee = callee_text_of(node, source)?;
    let normalized = callee.replace("->", ".");
    let normalized = normalize_super_call(&normalized);
    if normalized.len() <= MAX_CALLEE_TEXT_CHARS && is_identifier_like(&normalized) {
        let (ref_name, receiver_name) = split_callee_name(&normalized);
        return Some((ref_name, receiver_name, callee));
    }
    let (ref_name, receiver_name) = cpp_member_callee(node, source)?;
    Some((ref_name, Some(receiver_name), callee))
}

/// Reads a C++ member call written through an expression the identifier gate
/// rejects: `statuses[0].ok()` and `(*it).second.Flush()` name a member of
/// something the receiver path reaches.
///
/// Only a `field_expression` function qualifies. A callee the grammar spells as
/// one identifier (`helper()`, `Namespace::helper()`) never reaches here — its
/// text passes the gate — and a call-shaped receiver (`factory().run()`) has no
/// name whose type the resolver could read.
fn cpp_member_callee(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    let function = node.child_by_field_name("function")?;
    if function.kind() != "field_expression" {
        return None;
    }
    let field = function.child_by_field_name("field")?;
    let argument = function.child_by_field_name("argument")?;
    let receiver = cpp_receiver_path(argument, source)?;
    Some((node_text(field, source).to_owned(), receiver))
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
    if let Some((target, ty)) = java_constructor_target(node, source) {
        let name = ty.rsplit('.').next().unwrap_or(&ty);
        let raw = if node.kind() == "object_creation_expression" {
            format!("new {}", node_text(target, source))
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
        && let Some((_, _, callee)) = callee_of(node, source)
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
            let line = node.start_position().row + 1;
            let column = node.start_position().column;
            let constructs = construction_site(node);
            if constructs.is_some() {
                // A construction names its type, and that spelling — `X` after the
                // `new` keyword — is where the call stands.
                let position = node
                    .child_by_field_name("type")
                    .map_or_else(|| node.start_position(), |spelled| spelled.start_position());
                context.add_construction_edge(
                    owner_index,
                    &ref_name,
                    receiver_name.as_deref(),
                    Some(callee.as_str()),
                    call_arity_of(node),
                    position.row + 1,
                    position.column,
                    constructs,
                );
            } else {
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
                    line,
                    column,
                );
            }
        }
    } else if node.kind() == "compound_literal_expression"
        && let Some((name, position)) = constructed_type_name(node, source)
    {
        // `X{}` builds a value through the same constructor `X(…)` reaches, but
        // the grammar spells it as a literal rather than a call, so the type
        // field is the whole call site.
        context.add_construction_edge(
            owner_index,
            &name,
            None,
            Some(&name),
            call_arity_of(node),
            position.row + 1,
            position.column,
            Constructs::New,
        );
    }
    for child in named_children(node) {
        if is_call_scan_barrier(child) || boundary.is_some_and(|owns| owns(child)) {
            continue;
        }
        visit_for_calls(child, owner_index, source, context, boundary);
    }
}

/// The plain type a brace construction builds and where it is written:
/// `X{}` names `X` at that spelling. A template or qualified spelling names a
/// type the bare-name resolution cannot place, so it stays unreported.
fn constructed_type_name(node: Node<'_>, source: &[u8]) -> Option<(String, tree_sitter::Point)> {
    let spelled = node.child_by_field_name("type")?;
    if spelled.kind() != "type_identifier" {
        return None;
    }
    Some((
        node_text(spelled, source).to_owned(),
        spelled.start_position(),
    ))
}

/// Where a call node stands relative to building a value: a `new X()` site, a
/// call filling a variable's initializer or an assignment's right-hand side
/// (`auto w = Widget(1)`), or an ordinary call.
///
/// The distinction decides whether a type name can answer the call. A type is
/// not callable in general, so a call whose result is itself invoked
/// (`PREFIX(scan)(x)`), passed as an argument (`f(helper(x))`), returned
/// (`return helper(x);`) or left alone as a statement (`WithConstructor();`,
/// the most vexing parse of a declaration) is not a construction: another
/// entity of that name — a function — is the target whenever one exists.
fn construction_site(node: Node<'_>) -> Constructs {
    if matches!(node.kind(), "new_expression" | "object_creation_expression") {
        return Constructs::New;
    }
    let Some(parent) = node.parent() else {
        return Constructs::None;
    };
    match parent.kind() {
        "init_declarator" | "variable_declarator" => {
            if parent
                .child_by_field_name("value")
                .is_some_and(|value| value == node)
            {
                Constructs::Value
            } else {
                Constructs::None
            }
        }
        "assignment_expression" => {
            if parent
                .child_by_field_name("right")
                .is_some_and(|right| right == node)
            {
                Constructs::Value
            } else {
                Constructs::None
            }
        }
        // `Widget(1)` wrapped in parentheses is still the same construction
        // site once the wrapper is peeled off.
        "parenthesized_expression" => construction_site(parent),
        _ => Constructs::None,
    }
}

/// Java construction names a type, while this/super name the enclosing or
/// direct base constructor. Other grammars' creation nodes keep their rules.
fn java_constructor_target<'tree>(
    node: Node<'tree>,
    source: &[u8],
) -> Option<(Node<'tree>, String)> {
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
        FileFormat::TypeScript | FileFormat::JavaScript | FileFormat::Go
    ) && let Some((_, ty)) = function_return_of(entity_node, source, format)
    {
        context.add_declare_binding(
            owner_index,
            "return",
            &ty,
            entity_node.start_position().row + 1,
            entity_node.start_position().column,
            None,
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
    let binding = declaration_binding_of(node, source, format, context);
    let alias = alias_target_of(node, source, format);
    let mut shadowed = Vec::new();
    shadowing_names_of(node, source, format, &mut shadowed);
    for name in shadowed {
        // The name is taken by this owner's body even when nothing binds it to
        // an index: a parameter typed `() => void` still hides the imported
        // function the body calls by that name.
        context.register_shadowed_name(owner_index, &name);
    }
    let line = node.start_position().row + 1;
    let column = node.start_position().column;
    if let Some((name, target)) = alias {
        // A top-level declaration belongs to the file, not to the entity the
        // declarator itself became: every body of the file can see the alias.
        let alias_owner = if at_file_level(node) {
            FILE_SCOPE_INDEX
        } else {
            owner_index
        };
        context.register_value_alias(alias_owner, &name, &target);
    }
    if matches!(format, FileFormat::TypeScript | FileFormat::JavaScript) {
        collect_ts_js_statement_bindings(node, owner_index, source, context);
    }
    if format == FileFormat::Cpp {
        collect_cpp_statement_bindings(node, owner_index, source, context);
    }
    if format == FileFormat::Go {
        collect_go_scoped_binding(node, owner_index, source, context);
    }
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
                line,
                column,
                None,
            );
        }
        // A scoped binding carries the range it lives in, for the languages
        // whose declarations state a scope narrower than the owner's body.
        if matches!(
            format,
            FileFormat::TypeScript
                | FileFormat::JavaScript
                | FileFormat::Java
                | FileFormat::Go
                | FileFormat::Cpp
        ) {
            collect_binding(node, owner_index, &name, &bound_type, format, context);
            if format == FileFormat::Java
                && node.kind() == "enhanced_for_statement"
                && let Some(body) = node.child_by_field_name("body")
            {
                visit_for_bindings(body, owner_index, source, format, context, boundary);
            }
        } else {
            let until = binding_scope_end(node);
            context.add_declare_binding(owner_index, &name, &bound_type, line, column, until);
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

/// Records the bindings a TypeScript or JavaScript statement states without
/// declaring a name of the body: an export's own key, a `require` module, an
/// object literal's properties, and a re-export's alias.
fn collect_ts_js_statement_bindings(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
) {
    if node.kind() == "export_statement"
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
            None,
        );
    }
    if owner_index == super::FILE_SCOPE_INDEX
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
    if matches!(node.kind(), "variable_declarator" | "assignment_expression") {
        collect_object_bindings(node, owner_index, source, context);
    }
    if node.kind() == "export_specifier"
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
            None,
        );
    }
}

/// Records what a C++ using-declaration states: a directive at the file's top
/// level opens its namespace to the whole unit, and a declaration inside a
/// class body borrows the named base member into that class's lookup.
fn collect_cpp_statement_bindings(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
) {
    if node.kind() != "using_declaration" {
        return;
    }
    if owner_index == super::FILE_SCOPE_INDEX
        && let Some(space) = node.named_child(0)
        && space.kind() == "identifier"
    {
        // `using namespace rocksdb;` is a directive of the unit, not of one
        // body: the names it opens are read wherever the file is compiled. The
        // grammar reads it as a using declaration whose single name is the
        // namespace, so a bare name here is the directive.
        context.add_declare_binding(
            super::FILE_SCOPE_INDEX,
            "[[cpp_using_namespace]]",
            node_text(space, source),
            node.start_position().row + 1,
            node.start_position().column,
            None,
        );
    }
    if let Some((borrowed, _)) = cpp_binding_of(node, source, context) {
        // `using First<T>::Run;` puts that base member into the class's own
        // lookup: a bare `Run()` written in the class's bodies reaches the
        // base's member rather than a namespace-scope function of the name.
        context.register_borrowed_name(owner_index, &borrowed);
    }
}

/// Last line a binding the statement itself scopes stays in effect, when that
/// is not simply the rest of the body.
///
/// A loop states its variable for its own body only (`for (Engine* e : es)`),
/// so a call after the loop through the same name reads the name's other
/// declaration, not the loop's. Everything else — a local, a parameter, a
/// member — holds until the body ends, which the owner's range already
/// expresses.
fn binding_scope_end(node: Node<'_>) -> Option<usize> {
    // A declaration written in the loop's own header (`for (auto it = v.begin(); …)`)
    // belongs to the loop even though the declaration node is matched first.
    let mut statement = node;
    while matches!(statement.kind(), "declaration" | "init_declarator") {
        statement = statement.parent()?;
    }
    if !matches!(
        statement.kind(),
        "for_range_loop" | "for_in_statement" | "for_statement" | "for_expression"
    ) {
        return None;
    }
    let body = statement.child_by_field_name("body")?;
    Some(body.end_position().row + 1)
}

/// Whether a declaration sits at the file's top level: nothing that owns a
/// body stands between it and the root, so the name it introduces is visible
/// to every entity of the file rather than to one body.
fn at_file_level(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "compilation_unit" | "module" | "program" | "source_file" | "translation_unit" => {
                return true;
            }
            // Any body or container between the declaration and the root
            // makes the name local to it: a function body, a class body, an
            // object literal, a loop body.
            "block"
            | "class_body"
            | "compound_statement"
            | "declaration_list"
            | "enum_body"
            | "field_declaration_list"
            | "interface_body"
            | "object"
            | "object_pattern"
            | "statement_block" => return false,
            _ => current = parent,
        }
    }
    false
}

/// Extracts `(bound name, aliased name)` from a declaration whose initializer
/// is a bare name: `const dispatch = send` makes `dispatch` another spelling of
/// `send`, so a call written through either reaches the same function.
///
/// Only a single name on each side qualifies. Destructuring (`const [a, b] =
/// pair`) and calls (`const x = makeX()`) bind something other than one named
/// value, and an initializer that names a variable aliases that variable —
/// the resolver follows the chain one link at a time, so a wrong guess here
/// would bend a later call.
pub(crate) fn alias_target_of(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
) -> Option<(String, String)> {
    let (name, value) = match format {
        FileFormat::TypeScript | FileFormat::JavaScript | FileFormat::Java => {
            if node.kind() != "variable_declarator" {
                return None;
            }
            (
                node.child_by_field_name("name")?,
                node.child_by_field_name("value")?,
            )
        }
        FileFormat::Python => {
            if node.kind() != "assignment" {
                return None;
            }
            (
                node.child_by_field_name("left")?,
                node.child_by_field_name("right")?,
            )
        }
        FileFormat::Go => {
            if node.kind() != "short_var_declaration" {
                return None;
            }
            // `a, b := f()` binds several names to one call; only a
            // single-to-single assignment says "this name is that name".
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            if left.named_child_count() != 1 || right.named_child_count() != 1 {
                return None;
            }
            (left.named_child(0)?, right.named_child(0)?)
        }
        FileFormat::Rust => {
            if node.kind() != "let_declaration" {
                return None;
            }
            (
                node.child_by_field_name("pattern")?,
                node.child_by_field_name("value")?,
            )
        }
        // C/C++ function-pointer aliases already travel as the bound type
        // (`c_binding_of`), which partition has consulted since they were
        // the only alias form; a second table would split that evidence.
        _ => return None,
    };
    if name.kind() != "identifier" || value.kind() != "identifier" {
        return None;
    }
    let name = node_text(name, source);
    let target = node_text(value, source);
    (name != target).then(|| (name.to_owned(), target.to_owned()))
}

fn collect_binding(
    node: Node<'_>,
    owner_index: usize,
    name: &str,
    ty: &str,
    format: FileFormat,
    context: &mut WalkContext,
) {
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
        context.add_declare_binding(owner, name, ty, start.0, start.1, None);
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
        FileFormat::TypeScript => ts_binding_of(node, source),
        FileFormat::JavaScript => js_binding_of(node, source),
        _ => None,
    }
}

/// Appends the names a declaration node takes for its own scope, whether or
/// not the index can spell the type they hold.
///
/// [`declaration_binding_of`] answers only bindings that name a type, because
/// a binding is evidence for later receiver calls. A name is occupied even
/// when its type is not spellable — `function invoke(dispatch: () => void)`
/// hides an imported `dispatch` from every call the body writes — and a bare
/// call through an occupied name reaches the local value rather than the
/// file-level symbol. Keeping occupancy separate from typing lets partition
/// ask the two questions apart: "which type does this name hold" and "is this
/// name the file's".
fn shadowing_names_of(node: Node<'_>, source: &[u8], format: FileFormat, names: &mut Vec<String>) {
    match format {
        FileFormat::TypeScript => shadowing_names_in_typescript(node, source, names),
        FileFormat::JavaScript => shadowing_names_in_javascript(node, source, names),
        FileFormat::Python => shadowing_names_in_python(node, source, names),
        FileFormat::Go => shadowing_names_in_go(node, source, names),
        FileFormat::Rust => shadowing_names_in_rust(node, source, names),
        FileFormat::Java => shadowing_names_in_java(node, source, names),
        FileFormat::C | FileFormat::Cpp => shadowing_names_in_c_family(node, source, names),
        _ => {}
    }
}

/// Appends `candidate`'s text when it is a plain identifier. Destructuring
/// patterns carry several names in one node, so they are read by their
/// callers instead.
fn push_shadowing_name(candidate: Option<Node<'_>>, source: &[u8], names: &mut Vec<String>) {
    if let Some(node) = candidate.filter(|node| node.kind() == "identifier") {
        names.push(node_text(node, source).to_owned());
    }
}

fn shadowing_names_in_typescript(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "required_parameter" | "optional_parameter" => {
            push_shadowing_name(node.child_by_field_name("pattern"), source, names);
        }
        "variable_declarator" => {
            push_shadowing_name(node.child_by_field_name("name"), source, names);
        }
        _ => {}
    }
}

fn shadowing_names_in_javascript(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "formal_parameters" => {
            for child in named_children(node) {
                match child.kind() {
                    "identifier" => names.push(node_text(child, source).to_owned()),
                    "assignment_pattern" => {
                        push_shadowing_name(child.child_by_field_name("left"), source, names);
                    }
                    "rest_pattern" => {
                        push_shadowing_name(child.named_child(0), source, names);
                    }
                    // Destructuring patterns bind several names; the
                    // callers below read a name per node.
                    _ => {}
                }
            }
        }
        "variable_declarator" => {
            push_shadowing_name(node.child_by_field_name("name"), source, names);
        }
        _ => {}
    }
}

fn shadowing_names_in_python(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "parameters" => {
            for child in named_children(node) {
                let candidate = match child.kind() {
                    "identifier" => Some(child),
                    // `a: Left` carries the name as its first child.
                    "typed_parameter" => child.named_child(0),
                    "default_parameter" | "typed_default_parameter" => {
                        child.child_by_field_name("name")
                    }
                    _ => None,
                };
                push_shadowing_name(candidate, source, names);
            }
        }
        "assignment" => {
            push_shadowing_name(node.child_by_field_name("left"), source, names);
        }
        _ => {}
    }
}

fn shadowing_names_in_go(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "parameter_declaration" | "variadic_parameter_declaration" => {
            push_shadowing_name(node.child_by_field_name("name"), source, names);
        }
        "short_var_declaration" => {
            if let Some(left) = node.child_by_field_name("left") {
                push_shadowing_name(left.named_child(0), source, names);
            }
        }
        _ => {}
    }
}

fn shadowing_names_in_rust(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        // `fn m(a: Left)`; `mut a` wraps the name in `mut_pattern`.
        "parameter" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                let candidate = if pattern.kind() == "identifier" {
                    Some(pattern)
                } else {
                    pattern.named_child(0)
                };
                push_shadowing_name(candidate, source, names);
            }
        }
        "let_declaration" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                push_shadowing_name(Some(pattern), source, names);
            }
        }
        _ => {}
    }
}

fn shadowing_names_in_java(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "formal_parameter" | "catch_formal_parameter" => {
            push_shadowing_name(node.child_by_field_name("name"), source, names);
        }
        "local_variable_declaration" => {
            for child in named_children(node) {
                if child.kind() == "variable_declarator" {
                    push_shadowing_name(child.child_by_field_name("name"), source, names);
                }
            }
        }
        _ => {}
    }
}

fn shadowing_names_in_c_family(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    match node.kind() {
        "parameter_declaration" => {
            if let Some(declarator) = node.child_by_field_name("declarator") {
                let name = peel_declarator_name(declarator, source);
                names.extend(name);
            }
        }
        "declaration" => {
            if let Some(declarator) = node.child_by_field_name("declarator") {
                // `int x = 1` wraps the name in an `init_declarator`.
                let declarator = if declarator.kind() == "init_declarator" {
                    declarator.child_by_field_name("declarator")
                } else {
                    Some(declarator)
                };
                let name = declarator.and_then(|node| peel_declarator_name(node, source));
                names.extend(name);
            }
        }
        _ => {}
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
            None,
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
    if matches!(format, FileFormat::C | FileFormat::Cpp) {
        // A translation-unit declaration states the type of the name it
        // introduces (`extern MemTableRep::Iterator* iter;`), and every body of
        // the file may read that name. A class body states its members for the
        // class entity itself, so those are left to its own scan.
        let boundary = |node: Node<'_>| {
            matches!(
                node.kind(),
                "function_definition"
                    | "lambda_expression"
                    | "class_specifier"
                    | "struct_specifier"
                    | "union_specifier"
                    | "enum_specifier"
                    | "template_declaration"
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
    // A definition states the result beside its body, a prototype beside its
    // parameter list. A call's result type reads the same either way, and the
    // readers that need it — a `decltype(f(...))` base, an `auto x = f()`
    // caller — often see only the prototype, the definition living elsewhere.
    let declarator = match node.kind() {
        "function_definition" => node.child_by_field_name("declarator")?,
        "declaration" => {
            let declarator = node.child_by_field_name("declarator")?;
            // A variable declaration (`Widget w;`) states no result type; only
            // a prototype's declarator ends in a parameter list.
            let mut inner = declarator;
            while matches!(
                inner.kind(),
                "pointer_declarator" | "reference_declarator" | "array_declarator"
            ) {
                inner = inner.child_by_field_name("declarator")?;
            }
            if inner.kind() != "function_declarator" {
                return None;
            }
            declarator
        }
        _ => return None,
    };
    let type_node = node.child_by_field_name("type")?;
    let type_name = cpp_type_name_of(type_node, source)?;
    if type_name == "void" {
        return None;
    }
    // The function name sits under the declarator chain: the function
    // declarator itself, optionally wrapped in pointer/reference/array
    // declarators.
    let mut declarator = declarator;
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

/// Follows pointer/reference/attribute declarator wrappers down to the
/// spelled name.
///
/// The wrapper carries the name in its `declarator` field; a cv-qualifier
/// (`const`, `volatile`) or a macro attribute sits beside it as a child, so
/// reading the first named child answers `const` instead of the name.
fn peel_declarator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node;
    loop {
        match current.kind() {
            // A `typedef` spells its new name with the type grammar's own
            // identifier (`typedef Iter<Element> iterator`).
            "identifier" | "field_identifier" | "type_identifier" => {
                return Some(node_text(current, source).to_owned());
            }
            "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator"
            | "init_declarator" => {
                current = current.child_by_field_name("declarator").or_else(|| {
                    named_children(current)
                        .into_iter()
                        .find(|child| carries_name(child.kind()))
                })?;
            }
            _ => return None,
        }
    }
}

/// Declarator node kinds that wrap another declarator, so the search for the
/// spelled name has to descend through them. A function declarator is
/// deliberately absent: it names a callable, not a receiver.
fn carries_name(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator"
    )
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

/// Whether a declaration's declarator states a value rather than a function.
///
/// `std::unique_ptr<W> it(raw);` is the most vexing parse: the grammar reads
/// the initializer as a parameter list, exactly as it reads a prototype's. Only
/// a body states one of these, and what it states there is the holder being
/// built from `raw` — a prototype written inside a body is a statement about a
/// function declared elsewhere, which is never spelled this way. A holder is
/// what makes the reading possible at all: it is a type built from one value.
fn cpp_vexing_initializer(declared: Node<'_>, declarator: Node<'_>, source: &[u8]) -> bool {
    let Some(type_name) = cpp_type_name_of(declared, source) else {
        return false;
    };
    if !cpp_holds_one_value(&type_name) {
        return false;
    }
    let Some(parameters) = declarator.child_by_field_name("parameters") else {
        return false;
    };
    let parameters = named_children(parameters);
    if parameters.is_empty()
        || !parameters
            .iter()
            .all(|parameter| matches!(parameter.kind(), "parameter_declaration" | "identifier"))
    {
        return false;
    }
    inside_function_body(declarator)
}

/// The templates whose value is built from one other value, which is what
/// makes a declaration beside them readable as either a variable or a function.
const CPP_HOLDING_TEMPLATES: &[&str] = &["unique_ptr", "shared_ptr", "weak_ptr"];

/// Whether a type spelling is a holding template's application
/// (`std::unique_ptr<Widget>`).
fn cpp_holds_one_value(type_name: &str) -> bool {
    type_name
        .split_once('<')
        .map(|(head, _)| head)
        .and_then(|head| head.rsplit("::").next())
        .is_some_and(|name| CPP_HOLDING_TEMPLATES.contains(&name))
}

/// Whether a node is written inside a function's body rather than beside it.
fn inside_function_body(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "compound_statement" => return true,
            "translation_unit" | "declaration_list" | "field_declaration_list" => return false,
            _ => current = parent,
        }
    }
    false
}

fn cpp_binding_of(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
) -> Option<(String, String)> {
    match node.kind() {
        // `Widget w;` / `Widget* p = f();` / `Widget& r = w;` /
        // `std::unique_ptr<W> h(new W());`: the declaration spells the type
        // the name holds — through pointer/reference wrappers, an enclosing
        // class (`MemTableRep::Iterator`) and a holding template argument.
        // `auto x = ...` carries no type of its own and is handled below.
        "declaration" => {
            let declared = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            if declared.kind() != "placeholder_type_specifier" {
                // A function prototype declares a function, not a receiver.
                // `std::unique_ptr<W> h(raw);` is the exception: the grammar
                // reads the initializer as a parameter list, and a parameter
                // that spells no type is the object's argument, so the name
                // beside the type is the object's.
                let declarator = match declarator.kind() {
                    "function_declarator"
                        if cpp_vexing_initializer(declared, declarator, source) =>
                    {
                        declarator.child_by_field_name("declarator")?
                    }
                    "function_declarator" => return None,
                    _ => declarator,
                };
                let name = peel_declarator_name(declarator, source)?;
                let type_name = cpp_type_name_of(declared, source)?;
                return Some((name, cpp_array_suffix(&type_name, declarator)));
            }
            // `auto x = X{}` / `auto* const x = new X()`: the placeholder type
            // defers to the initializer's spelled type. `auto x = makeX()`: a
            // bare call binds the registered factory's return type.
            if declarator.kind() != "init_declarator" {
                return None;
            }
            let name = peel_declarator_name(declarator, source)?;
            let value = declarator.child_by_field_name("value")?;
            let type_node = match value.kind() {
                "new_expression" | "compound_literal_expression" => {
                    value.child_by_field_name("type")?
                }
                "call_expression" => {
                    let function = value.child_by_field_name("function")?;
                    if function.kind() != "identifier" {
                        // `auto it = values.begin()`: the container states the
                        // position the iterator walks, so the binding names the
                        // range's element and the resolver reads it from the
                        // range's own declared type.
                        return Some((name, cpp_iterator_marker_of(function, source)?));
                    }
                    let type_name = context.factory_return_of(node_text(function, source))?;
                    return Some((name, type_name.to_owned()));
                }
                // `auto& v = it->second`: the placeholder denotes whatever the
                // path on the right names, which the resolver reads from the
                // declarations that path walks.
                _ => {
                    let path = cpp_receiver_path(value, source)?;
                    return Some((name, cpp_type_of_marker(&path)));
                }
            };
            Some((name, node_text(type_node, source).to_owned()))
        }
        // `void m(Left &a)`: the parameter name sits under the reference
        // declarator wrapper, and the type may be qualified or a holder.
        // `Widget* member_;`: the class states the receiver type of the name
        // its methods call through, exactly as a parameter states one. The
        // binding belongs to the class entity, so every method body reaches it.
        "parameter_declaration" | "field_declaration" => {
            let type_node = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            if declarator.kind() == "function_declarator" {
                return None;
            }
            let type_name = cpp_type_name_of(type_node, source)?;
            let name = peel_declarator_name(declarator, source)?;
            Some((name, cpp_array_suffix(&type_name, declarator)))
        }
        // `for (Engine* car : engines)`: the loop states the type of what it
        // yields, and the binding holds for its body alone. A placeholder type
        // (`for (const auto& meta : blob_files)`) defers to the element the
        // range's own type yields, which is answered where that type is
        // declared.
        "for_range_loop" => {
            let type_node = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            let name = peel_declarator_name(declarator, source)?;
            if type_node.kind() != "placeholder_type_specifier" {
                let type_name = cpp_type_name_of(type_node, source)?;
                return Some((name, cpp_array_suffix(&type_name, declarator)));
            }
            let range = node.child_by_field_name("right")?;
            let range = cpp_receiver_path(range, source)?;
            Some((name, cpp_element_marker(&range)))
        }
        // `using Shard = ClockCacheShard<Table>` / `typedef Iter<Element>
        // iterator`: the class states a member type that a declaration or a
        // call reaches by name alone, so the alias answers like a field would.
        "alias_declaration" => {
            let name = node.child_by_field_name("name")?;
            let target = node.child_by_field_name("type")?;
            let type_name = cpp_type_name_of(target, source)?;
            Some((node_text(name, source).to_owned(), type_name))
        }
        "type_definition" => {
            let target = node.child_by_field_name("type")?;
            let declarator = node.child_by_field_name("declarator")?;
            let name = peel_declarator_name(declarator, source)?;
            Some((name, cpp_type_name_of(target, source)?))
        }
        // `using google::protobuf::Descriptor;` puts that type's own name into
        // this scope: a declaration writing `Descriptor` here means the type
        // the clause qualifies, not a namesake another file declares. The
        // grammar reads `using namespace rocksdb;` as the same statement, with
        // a bare name for the namespace, which binds nothing.
        "using_declaration" => {
            let target = node.named_child(0)?;
            if target.kind() != "qualified_identifier" {
                return None;
            }
            let type_name = cpp_type_name_of(target, source)?;
            let name = type_name.rsplit("::").next()?.to_owned();
            (name != type_name).then_some((name, type_name))
        }
        _ => None,
    }
}

/// The receiver path an expression names, for a call site or a range's element.
///
/// Only a path of plain names and element reads qualifies (`logs_`, `vs->logs_`,
/// `statuses[0]`, `(*it).second`). A path that depends on a call's return value
/// (`*versions_->Set()`, `layout.optimized_order()`) stays unanswered rather
/// than guessed: the extractor holds no return types to answer it with.
///
/// `*it` and `&it` add no step: a pointer binding already spells its pointee,
/// and an iterator binding spells the iterator whose element the same name
/// denotes. A subscript reads one element of what the base holds, which the
/// resolver evaluates against the base's type the way it evaluates a loop
/// variable's.
fn cpp_receiver_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "this" => Some(node_text(node, source).to_owned()),
        "field_expression" => {
            let base = cpp_receiver_path(node.child_by_field_name("argument")?, source)?;
            let field = node.child_by_field_name("field")?;
            Some(format!("{base}.{}", node_text(field, source)))
        }
        "pointer_expression" => cpp_receiver_path(node.child_by_field_name("argument")?, source),
        "parenthesized_expression" => cpp_receiver_path(node.named_child(0)?, source),
        "subscript_expression" => {
            let base = cpp_receiver_path(node.child_by_field_name("argument")?, source)?;
            Some(cpp_index_marker(&base))
        }
        _ => None,
    }
}

/// Appends the decorations the declarator adds, so the resolver can read the
/// value the name holds one step at a time: one `[]` per array level (`Status
/// statuses[2]` yields a `Status`, `std::vector<Buffer> pools[2]` a vector) and
/// one `*` per pointer level (`Status* statuses` answers `statuses[i]` with a
/// `Status`).
fn cpp_array_suffix(type_name: &str, declarator: Node<'_>) -> String {
    let mut levels = 0;
    let mut pointers = 0;
    let mut current = declarator;
    loop {
        match current.kind() {
            "array_declarator" => {
                levels += 1;
                let Some(inner) = current.child_by_field_name("declarator") else {
                    break;
                };
                current = inner;
            }
            "pointer_declarator" => {
                pointers += 1;
                let Some(inner) = current.child_by_field_name("declarator") else {
                    break;
                };
                current = inner;
            }
            "reference_declarator" | "parenthesized_declarator" => {
                let Some(inner) = current.child_by_field_name("declarator") else {
                    break;
                };
                current = inner;
            }
            _ => break,
        }
    }
    format!("{type_name}{}{}", "[]".repeat(levels), "*".repeat(pointers))
}

/// The binding a loop variable takes from the range it walks: the element of
/// the range's type, asked where the range is declared rather than here.
fn cpp_element_marker(range: &str) -> String {
    format!("{ELEMENT_MARKER}({range})")
}

/// The binding a placeholder declaration takes from an iterator-returning
/// member call: `auto it = values.begin()` holds a position over `values`, so
/// the name denotes that range's element.
///
/// Only the members whose result walks the receiver's own elements qualify.
/// Any other member answers with a type only a return-type index could name,
/// and the call stays unanswered rather than guessed.
fn cpp_iterator_marker_of(function: Node<'_>, source: &[u8]) -> Option<String> {
    if function.kind() != "field_expression" {
        return None;
    }
    let field = function.child_by_field_name("field")?;
    if !ITERATOR_MEMBERS.contains(&node_text(field, source)) {
        return None;
    }
    let receiver = cpp_receiver_path(function.child_by_field_name("argument")?, source)?;
    Some(cpp_position_marker(&receiver))
}

/// The members whose result walks the receiver's own elements.
const ITERATOR_MEMBERS: &[&str] = &[
    "begin",
    "end",
    "cbegin",
    "cend",
    "rbegin",
    "rend",
    "crbegin",
    "crend",
    "find",
    "lower_bound",
    "upper_bound",
    "equal_range",
];

/// Prefix of a binding that names something derived from another binding
/// rather than a type the source spells (`for (auto& kv : buffers)`).
const ELEMENT_MARKER: &str = "element:";

/// The binding an iterator-returning call gives a placeholder declaration:
/// `auto it = values.begin()` holds a position over `values`, which answers the
/// element it walks rather than the container itself.
fn cpp_position_marker(range: &str) -> String {
    format!("{POSITION_MARKER}({range})")
}

/// Prefix of a binding that names a position in another binding's type. An
/// iterator answers the element it walks; a call written through it reaches
/// that element, not the container.
const POSITION_MARKER: &str = "position:";

/// The binding a placeholder declaration takes from the path it is
/// initialized with: `auto& v = it->second` holds exactly what that member
/// holds, wherever the member is declared.
fn cpp_type_of_marker(path: &str) -> String {
    format!("{TYPE_OF_MARKER}({path})")
}

/// Prefix of a binding that repeats the type of another path (`auto& v = it->second`).
const TYPE_OF_MARKER: &str = "type_of:";

/// The binding a subscript takes from the value it indexes: `buffers[k]`.
fn cpp_index_marker(base: &str) -> String {
    format!("{INDEX_MARKER}({base})")
}

/// Prefix of a binding that names one value a container read by subscript
/// answers: a map answers the value it pairs with the key, a vector or array
/// the element it holds.
const INDEX_MARKER: &str = "index:";

/// The type a C++ declaration names, kept in the spelling the source wrote.
///
/// The text keeps what qualifies it — the enclosing namespace or class
/// (`MemTableRep::Iterator`, `BaseHyperClockCache<Table>::Shard`), the template
/// arguments (`std::deque<Slice>`, `autovector<std::pair<int, FileMetaData*>>`)
/// — because the resolver decides from it which class answers a call: a
/// container answers with its own members, its element answers a subscript or a
/// loop variable, an iterator member answers as the element it walks, and a
/// holder (`std::unique_ptr<Widget>`) answers as the type it holds.
///
/// Pointer and reference wrappers are not part of the spelling: they arrive
/// through the declarator, which already peels them to reach the name.
fn cpp_type_name_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        // A qualified or template spelling states the whole name a call through
        // it is looked up by, so the text is kept as written.
        "type_identifier"
        | "primitive_type"
        | "sized_type_specifier"
        | "qualified_identifier"
        | "template_type" => Some(collapse_whitespace(node_text(node, source))),
        "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => node
            .child_by_field_name("name")
            .map(|name| collapse_whitespace(node_text(name, source))),
        // `const Widget&`: the qualifier sits beside the type descriptor.
        "type_descriptor" => {
            let inner = node
                .child_by_field_name("type")
                .or_else(|| node.named_child(0))?;
            cpp_type_name_of(inner, source)
        }
        // `typename SkipList<Key, Comparator>::Iterator`: `typename` says the
        // name is a type, not which type it is.
        "dependent_type" => {
            let inner = node
                .child_by_field_name("type")
                .or_else(|| node.named_child(0))?;
            cpp_type_name_of(inner, source)
        }
        _ => None,
    }
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
pub(crate) fn module_path_of(node: Node<'_>, source: &[u8], format: FileFormat) -> Option<String> {
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
    let mut clauses = match vocabulary {
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
    // `class X : public decltype(f(...))` parses the base as an ERROR node:
    // the grammar takes a base name there, not an expression. The decltype is
    // still the base the source states, and its answer is `f`'s result type.
    if format == FileFormat::Cpp
        && let Some(base) = named_children(entity_node)
            .into_iter()
            .find(|child| child.kind() == "ERROR")
            .and_then(|error| {
                descendants_within(error, 3)
                    .into_iter()
                    .find(|node| node.kind() == "decltype")
            })
    {
        clauses.push((GraphEdgeKind::Extends, base));
    }
    for (ref_kind, clause) in clauses {
        let dbg_names = type_names_from_clause(clause, source, &*context);
        for name in dbg_names {
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
fn type_names_from_clause(clause: Node<'_>, source: &[u8], context: &WalkContext) -> Vec<String> {
    let mut names = Vec::new();
    collect_type_names(clause, source, context, &mut names);
    names
}

fn collect_type_names(
    node: Node<'_>,
    source: &[u8],
    context: &WalkContext,
    names: &mut Vec<String>,
) {
    if CLAUSE_DECORATION_KINDS.contains(&node.kind()) {
        return;
    }
    // `class FlatAllocator : public decltype(ApplyTypeList<char, int>(0))`:
    // the base is what that call returns, stated by the callee's own
    // signature. Reading the callee's name as the base would link the class
    // to a function instead of the class the call builds.
    if node.kind() == "decltype"
        && let Some(result) = decltype_result_type(node, source, context)
    {
        names.push(result);
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
        collect_type_names(child, source, context, names);
    }
}

/// The result type of the call a `decltype(...)` expression names, read from
/// the file's own signature for that callee.
fn decltype_result_type(node: Node<'_>, source: &[u8], context: &WalkContext) -> Option<String> {
    if node.kind() == "call_expression"
        && let Some(function) = node.child_by_field_name("function")
    {
        // `f(...)` and `f<int>(...)` name the same callee; the template
        // arguments qualify it, they do not change what it returns.
        let callee = match function.kind() {
            "identifier" => Some(function),
            "template_function" => function.child_by_field_name("name"),
            _ => None,
        };
        if let Some(callee) = callee
            && let Some(result) = context.factory_return_of(node_text(callee, source))
        {
            return Some(result.to_owned());
        }
    }
    named_children(node)
        .into_iter()
        .find_map(|child| decltype_result_type(child, source, context))
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

    /// Every name a source binds to a spelled type, as `(name, type, line)`.
    fn bindings(
        format: FileFormat,
        lang: &tree_sitter::Language,
        source: &str,
    ) -> Vec<(String, String, usize)> {
        name_edges(format, lang, source)
            .into_iter()
            .map(|(name, bound_type, line, _)| (name, bound_type, line))
            .collect()
    }

    /// Every name a source binds, with the last line a loop-scoped binding
    /// holds for.
    fn name_edges(
        format: FileFormat,
        lang: &tree_sitter::Language,
        source: &str,
    ) -> Vec<(String, String, usize, Option<usize>)> {
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
                    edge.bound_until,
                )
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn collects_creation_and_parameter_bindings() {
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
    fn collects_declared_type_and_member_bindings() {
        let cpp = || tree_sitter_cpp::LANGUAGE.into();
        // A cv-qualifier and an attribute macro sit beside the name rather
        // than wrapping it, so the spelling must come from the declarator.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "void m(Widget const& w, Slice* const s) { w.Draw(); }",
            ),
            // The pointer the declarator states stays in the spelling: a
            // subscript through `s` reads the `Slice` it points at, while a
            // member call reads through the pointer to the same type.
            vec![
                ("w".to_owned(), "Widget".to_owned(), 1),
                ("s".to_owned(), "Slice*".to_owned(), 1),
            ]
        );
        // A member field states the receiver its class's methods call through.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "class A {\n  Slice* blocks_;\n  InstrumentedMutex mutex_;\n};",
            ),
            vec![
                ("blocks_".to_owned(), "Slice*".to_owned(), 2),
                ("mutex_".to_owned(), "InstrumentedMutex".to_owned(), 3),
            ]
        );
        // A holder keeps its spelling and a container keeps its own: the
        // resolver unwraps the holder and reads the container's element where
        // the call is written, so the extractor states what the source says.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "void m() {\n  std::unique_ptr<VersionSet> versions_;\n  std::shared_ptr<DB> db;\n  std::vector<DB> dbs;\n}",
            ),
            vec![
                (
                    "versions_".to_owned(),
                    "std::unique_ptr<VersionSet>".to_owned(),
                    2
                ),
                ("db".to_owned(), "std::shared_ptr<DB>".to_owned(), 3),
                ("dbs".to_owned(), "std::vector<DB>".to_owned(), 4),
            ]
        );
        // A loop variable holds what the loop states or, for a placeholder
        // type, the element of what the loop walks.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "void m(std::vector<Engine*>& engines, std::vector<Meta>& metas) {\n  for (Engine* car : engines) car->Start();\n  for (const auto& meta : metas) meta.Get();\n}",
            ),
            vec![
                ("engines".to_owned(), "std::vector<Engine*>".to_owned(), 1),
                ("metas".to_owned(), "std::vector<Meta>".to_owned(), 1),
                ("car".to_owned(), "Engine*".to_owned(), 2),
                ("meta".to_owned(), "element:(metas)".to_owned(), 3),
            ]
        );
        // The loop's own header scopes its variable; nothing outside it sees
        // that binding.
        let loop_scoped = name_edges(
            FileFormat::Cpp,
            &cpp(),
            "void m(std::vector<Engine*>& engines) {\n  for (Engine* car : engines) {\n    car->Start();\n  }\n}",
        );
        assert_eq!(
            loop_scoped,
            vec![
                (
                    "engines".to_owned(),
                    "std::vector<Engine*>".to_owned(),
                    1,
                    None
                ),
                ("car".to_owned(), "Engine*".to_owned(), 2, Some(4)),
            ]
        );
        // A member type a class states by name answers like a field would.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "class A {\n  using Shard = ClockCacheShard<Table>;\n  typedef Iter<Element> iterator;\n};",
            ),
            vec![
                ("Shard".to_owned(), "ClockCacheShard<Table>".to_owned(), 2),
                ("iterator".to_owned(), "Iter<Element>".to_owned(), 3),
            ]
        );
        // Array levels live in the declarator, not the type spelling.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "void m() {\n  Status statuses[2];\n  Status grid[2][3];\n}",
            ),
            vec![
                ("statuses".to_owned(), "Status[]".to_owned(), 2),
                ("grid".to_owned(), "Status[][]".to_owned(), 3),
            ]
        );
        // A qualified type keeps the class it is nested in.
        assert_eq!(
            bindings(
                FileFormat::Cpp,
                &cpp(),
                "class A {\n  MemTableRep::Iterator* iter_;\n};",
            ),
            vec![("iter_".to_owned(), "MemTableRep::Iterator*".to_owned(), 2)]
        );
        // A function declarator names a callable, not a receiver.
        assert!(bindings(FileFormat::Cpp, &cpp(), "class A {\n  void Run();\n};").is_empty());
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
