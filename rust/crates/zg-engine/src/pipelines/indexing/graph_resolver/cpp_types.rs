//! Resolve C++ types from declarations, aliases and explicit return types.
//! Template arguments identify wrappers; project members require one class body.
//! These per-sweep caches never outlive the workspace resolution snapshot.

use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

use super::{
    ClassScope, DeclaredBinding, FileFormat, GraphEdgeKind, IndexStore, NameIndexEntry, Outcome,
    ResolutionSnapshot, Rule, RuleCaches, StoredPendingRef, SymbolType, collect_base_scopes,
    formats_for_path, is_type_scope, language_compatible_formats, member_resolution,
    prefer_definitions, unique_arity_match,
};

#[derive(Clone, Eq, Hash, PartialEq)]
struct Context {
    file: u32,
    owner: String,
    scope: String,
    position: (usize, usize),
}

impl Context {
    fn reference(snapshot: &ResolutionSnapshot, stored: &StoredPendingRef) -> Self {
        Self {
            file: stored.file_id,
            owner: stored.reference.owner_id.clone(),
            scope: snapshot
                .entity_record(&stored.reference.owner_id)
                .map_or_else(String::new, |owner| owner.qualified_name.clone()),
            position: (stored.reference.line, stored.reference.column),
        }
    }
}

#[derive(Clone, Debug)]
enum Type {
    Class(NameIndexEntry),
    Pointer(Box<Self>),
    SmartPointer(Box<Self>),
}

#[derive(Default)]
pub(super) struct TypeCache {
    types: HashMap<(Context, String), Option<Type>>,
}

fn declarations<'a>(
    store: &IndexStore,
    caches: &'a mut RuleCaches,
    owner: &str,
) -> crate::EngineResult<&'a [DeclaredBinding]> {
    if !caches.declared_bindings.contains_key(owner) {
        caches
            .declared_bindings
            .insert(owner.to_owned(), store.declared_bindings(owner)?);
    }
    Ok(&caches.declared_bindings[owner])
}

fn binding(
    store: &IndexStore,
    caches: &mut RuleCaches,
    context: &Context,
    name: &str,
) -> crate::EngineResult<Option<DeclaredBinding>> {
    Ok(declarations(store, caches, &context.owner)?
        .iter()
        .rev()
        .find(|binding| {
            binding.name == name
                && (binding.line as usize, binding.column as usize) <= context.position
                && binding.scope_end.is_none_or(|end| context.position < end)
        })
        .cloned())
}

fn scopes(scope: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut current = scope;
    loop {
        result.push(current);
        if current.is_empty() {
            break;
        }
        current = current.rsplit_once("::").map_or("", |(parent, _)| parent);
    }
    result
}

fn compatible(snapshot: &ResolutionSnapshot, hit: &NameIndexEntry) -> bool {
    let formats = snapshot
        .file_path(hit.file_id)
        .map(|path| formats_for_path(std::path::Path::new(path)));
    language_compatible_formats(Some(FileFormat::Cpp), formats.as_deref())
}

/// Keep nested template arguments while erasing them from the name index key.
/// No specialization is selected when several class bodies share that key.
fn template_name(ty: &str) -> Option<(String, Vec<String>)> {
    let mut name = String::new();
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (position, ch) in ty.char_indices() {
        match ch {
            '<' => {
                if depth == 0 {
                    start = position + 1;
                }
                depth += 1;
            }
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    arguments.push(ty[start..position].trim().to_owned());
                }
            }
            ',' if depth == 1 => {
                arguments.push(ty[start..position].trim().to_owned());
                start = position + 1;
            }
            _ if depth == 0 && !ch.is_whitespace() => name.push(ch),
            _ => {}
        }
    }
    (depth == 0 && !name.is_empty()).then_some((name, arguments))
}

fn materialize(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    ty: &str,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    if remaining == 0 {
        return Ok(None);
    }
    let key = (context.clone(), ty.to_owned());
    if let Some(found) = caches.cpp_types.types.get(&key) {
        return Ok(found.clone());
    }
    caches.cpp_types.types.insert(key.clone(), None);
    let found = materialize_inner(store, snapshot, caches, context, ty, remaining - 1)?;
    caches.cpp_types.types.insert(key, found.clone());
    Ok(found)
}

fn materialize_inner(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    ty: &str,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    let mut ty = ty.trim();
    while let Some(rest) = ["const ", "volatile ", "typename "]
        .iter()
        .find_map(|prefix| ty.strip_prefix(prefix))
    {
        ty = rest.trim();
    }
    while let Some(rest) = [" const", " volatile"]
        .iter()
        .find_map(|suffix| ty.strip_suffix(suffix))
    {
        ty = rest.trim();
    }
    if let Some(ty) = ty.strip_suffix('&') {
        return materialize(store, snapshot, caches, context, ty, remaining);
    }
    if let Some(ty) = ty.strip_suffix('*') {
        return Ok(
            materialize(store, snapshot, caches, context, ty, remaining)?
                .map(|ty| Type::Pointer(Box::new(ty))),
        );
    }
    if let Some(expression) = ty
        .strip_prefix("decltype(")
        .and_then(|ty| ty.strip_suffix(')'))
    {
        let source = format!("auto __zg_value = {expression};");
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("C++ grammar");
        let Some(tree) = parser
            .parse(&source, None)
            .filter(|tree| !tree.root_node().has_error())
        else {
            return Ok(None);
        };
        let value = tree
            .root_node()
            .named_child(0)
            .and_then(|node| node.child_by_field_name("declarator"))
            .and_then(|node| node.child_by_field_name("value"));
        return match value {
            Some(value) => expression_type(
                store,
                snapshot,
                caches,
                context,
                value,
                source.as_bytes(),
                remaining,
            ),
            None => Ok(None),
        };
    }
    named_type(store, snapshot, caches, context, ty, remaining)
}

fn named_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    ty: &str,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    let Some((name, arguments)) = template_name(ty) else {
        return Ok(None);
    };
    let absolute = name.starts_with("::");
    let name = name.trim_start_matches("::");
    let tail = name.rsplit("::").next().unwrap_or(name);
    let Some(candidates) = snapshot.lookup_candidates(tail) else {
        return Ok(None);
    };
    for scope in if absolute {
        vec![""]
    } else {
        scopes(&context.scope)
    } {
        let qualified = if scope.is_empty() {
            name.to_owned()
        } else {
            format!("{scope}::{name}")
        };
        let aliases = type_aliases(store, snapshot, caches, context, &qualified)?;
        if let Some((file, alias)) = aliases.first() {
            if aliases
                .iter()
                .any(|(_, other)| other.bound_type != alias.bound_type)
            {
                return Ok(None);
            }
            let origin = Context {
                file: *file,
                owner: format!("f{file}"),
                scope: qualified.clone(),
                position: (alias.line as usize, alias.column as usize),
            };
            return materialize(
                store,
                snapshot,
                caches,
                &origin,
                &alias.bound_type,
                remaining,
            );
        }
        let hits = candidates
            .iter()
            .filter(|hit| {
                hit.qualified_name == qualified
                    && hit.symbol_type.is_some_and(is_type_scope)
                    && compatible(snapshot, hit)
            })
            .collect::<Vec<_>>();
        match hits.as_slice() {
            [] => {}
            [hit] => return Ok(Some(Type::Class((*hit).clone()))),
            _ => {
                // One qualified name, two declarations: the reference reaches
                // the class its translation unit includes, and only that one.
                let visible =
                    super::unit_hits(store, snapshot, caches, "cpp", context.file, &hits)?;
                return match visible.as_slice() {
                    [hit] => Ok(Some(Type::Class((*hit).clone()))),
                    _ => Ok(None),
                };
            }
        }
    }
    if matches!(name, "std::unique_ptr" | "std::shared_ptr") && arguments.len() == 1 {
        return Ok(
            materialize(store, snapshot, caches, context, &arguments[0], remaining)?
                .map(|element| Type::SmartPointer(Box::new(element))),
        );
    }
    // Preserve the previous rule's bare-name support when there is one class
    // identity and no nearer alias. A qualified path never falls back by tail.
    if !absolute && !name.contains("::") {
        let hits = candidates
            .iter()
            .filter(|hit| hit.symbol_type.is_some_and(is_type_scope) && compatible(snapshot, hit))
            .collect::<Vec<_>>();
        if let [hit] = hits.as_slice() {
            return Ok(Some(Type::Class((*hit).clone())));
        }
    }
    Ok(None)
}

fn type_aliases(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    qualified: &str,
) -> crate::EngineResult<Vec<(u32, DeclaredBinding)>> {
    let mut files = vec![context.file];
    if let Some(candidates) =
        snapshot.lookup_candidates(qualified.rsplit("::").next().unwrap_or(qualified))
    {
        files.extend(
            candidates
                .iter()
                .filter(|hit| {
                    hit.qualified_name == qualified
                        && hit.symbol_type == Some(SymbolType::Alias)
                        && compatible(snapshot, hit)
                })
                .map(|hit| hit.file_id),
        );
    }
    files.sort_unstable();
    files.dedup();
    let mut aliases = Vec::new();
    for file in files {
        aliases.extend(
            declarations(store, caches, &format!("f{file}"))?
                .iter()
                .filter(|binding| {
                    binding.name == qualified
                        && (file != context.file
                            || (binding.line as usize, binding.column as usize) <= context.position)
                })
                .map(|binding| (file, binding.clone())),
        );
    }
    Ok(aliases)
}

fn variable_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    name: &str,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    let Some(binding) = binding(store, caches, context, name)? else {
        return Ok(None);
    };
    let mut origin = context.clone();
    origin.position = (binding.line as usize, binding.column as usize);
    materialize(
        store,
        snapshot,
        caches,
        &origin,
        &binding.bound_type,
        remaining,
    )
}

fn class_for_member(ty: &Type, arrow: bool) -> Option<&NameIndexEntry> {
    match (ty, arrow) {
        (Type::Class(owner), _) => Some(owner),
        (Type::Pointer(element) | Type::SmartPointer(element), true) => match element.as_ref() {
            Type::Class(owner) => Some(owner),
            _ => None,
        },
        _ => None,
    }
}

/// The member one class declares, spelled as the unit reads it.
///
/// A class read through a header declares its members there while their
/// bodies are written out-of-line, so the two spellings are one symbol: the
/// declaration the unit can see decides which member the call names, and the
/// body of that class and signature stands for it.
fn member(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    owner: &NameIndexEntry,
    name: &str,
    arity: Option<usize>,
) -> crate::EngineResult<Option<NameIndexEntry>> {
    let Some(hits) = snapshot.lookup_candidates(name) else {
        return Ok(None);
    };
    let hits: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| compatible(snapshot, hit) && hit.symbol_type == Some(SymbolType::Function))
        .collect();
    let root = ClassScope {
        name: owner.qualified_name.clone(),
        declared_in: Some(owner.file_id),
        entity: Some(owner.entity_id.clone()),
    };
    let mut scopes = vec![root.clone()];
    let branched =
        collect_base_scopes(store, snapshot, &root, &mut scopes, &mut HashSet::new(), 8)?;
    let mut inherited = None;
    for scope in scopes {
        // A member is written in the class body the unit reads, so only the
        // spellings that class declares are its members: a namesake class in a
        // file the unit never pulls in declares members of its own under the
        // very same qualified name.
        let members: Vec<&NameIndexEntry> = super::declarations_in(snapshot, &hits, &scope);
        let hit = match members.as_slice() {
            [] => continue,
            _ => member_spelling(store, snapshot, caches, context, &members, arity)?,
        };
        if scope.name == owner.qualified_name || !branched {
            return Ok(hit.cloned());
        }
        let Some(hit) = hit else { return Ok(None) };
        if inherited.is_some() {
            return Ok(None);
        }
        inherited = Some(hit.clone());
    }
    Ok(inherited)
}

/// The spelling of one member a class declares, paired with the body that
/// implements it.
///
/// Visibility decides on the spellings before the pairing: the declaration
/// written in the class body is what the unit reads, while the implementation
/// it supersedes usually sits in a `.cc` file outside the include closure.
/// Pairing first would drop the only spelling the unit can see and leave the
/// call with nothing to bind. The body of the same signature is what the
/// declaration stands for; two of them are two implementations this rule
/// cannot tell apart, and the declaration answers for the member.
fn member_spelling<'a>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    members: &[&'a NameIndexEntry],
    arity: Option<usize>,
) -> crate::EngineResult<Option<&'a NameIndexEntry>> {
    // A member belongs to the class body the unit read, so a same-named member
    // of a namesake class in a file the unit never pulls in is not what the
    // call reaches.
    let visible = super::unit_hits(store, snapshot, caches, "cpp", context.file, members)?;
    let seen = if visible.is_empty() {
        members
    } else {
        visible.as_slice()
    };
    // A class read through a header offers two spellings of one member — the
    // declaration and the body implementing it — and the pair is one symbol.
    // It is settled before the overload count, which would otherwise see two
    // members accepting the same call and decline to choose.
    let paired = super::prefer_definitions(snapshot, seen.to_vec());
    let spelling = match paired.as_slice() {
        [hit] => Some(*hit),
        _ => arity.and_then(|arity| unique_arity_match(paired.iter().copied(), arity)),
    };
    let Some(spelling) = spelling else {
        return Ok(None);
    };
    Ok(Some(paired_body(
        store, snapshot, caches, members, spelling,
    )?))
}

/// The body implementing one member spelling, when the class holds exactly one
/// body of that signature in this translation unit.
///
/// The declaration the unit reads may sit in a header shared by two independent
/// targets that both declare the class, while the only indexed body belongs to
/// the other target: the body's own unit is what says which declaration it
/// implements, so a body whose class cannot be placed pairs only when its unit
/// includes the declaring file. Otherwise the declaration answers the call,
/// because a namesake's implementation is not this class's member.
fn paired_body<'a>(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    members: &[&'a NameIndexEntry],
    spelling: &'a NameIndexEntry,
) -> crate::EngineResult<&'a NameIndexEntry> {
    if !spelling.is_declaration {
        return Ok(spelling);
    }
    let declared = super::member_class_file(snapshot, spelling);
    let mut bodies = Vec::new();
    for hit in members {
        if hit.is_declaration
            || !super::same_symbol(
                hit.arity,
                hit.parameters.as_deref(),
                spelling.arity,
                spelling.parameters.as_deref(),
            )
        {
            continue;
        }
        let pairs = match (declared, super::member_class_file(snapshot, hit)) {
            (Some(declared), Some(body_class)) => declared == body_class,
            (Some(declared), None) => {
                super::unit_includes(store, snapshot, caches, "cpp", hit.file_id, declared)?
            }
            // A spelling whose class the workspace cannot place states nothing
            // about where its body is written, so the body still stands for it.
            (None, _) => true,
        };
        if pairs {
            bodies.push(*hit);
        }
    }
    Ok(match bodies.as_slice() {
        [body] => body,
        _ => spelling,
    })
}

fn return_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    function: &NameIndexEntry,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    let returns = declarations(store, caches, &function.entity_id)?
        .iter()
        .filter(|binding| binding.name == "return")
        .cloned()
        .collect::<Vec<_>>();
    let [binding] = returns.as_slice() else {
        return Ok(None);
    };
    let context = Context {
        file: function.file_id,
        owner: function.entity_id.clone(),
        scope: function.qualified_name.clone(),
        position: (binding.line as usize, binding.column as usize),
    };
    materialize(
        store,
        snapshot,
        caches,
        &context,
        &binding.bound_type,
        remaining,
    )
}

fn expression_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    node: Node<'_>,
    source: &[u8],
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    if remaining == 0 {
        return Ok(None);
    }
    let text = |node: Node<'_>| node.utf8_text(source).unwrap_or_default();
    match node.kind() {
        "identifier" => variable_type(store, snapshot, caches, context, text(node), remaining - 1),
        "parenthesized_expression" => match node.named_child(0) {
            Some(value) => expression_type(
                store,
                snapshot,
                caches,
                context,
                value,
                source,
                remaining - 1,
            ),
            None => Ok(None),
        },
        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return Ok(None);
            };
            let arity = node.child_by_field_name("arguments").map(|arguments| {
                let mut cursor = arguments.walk();
                arguments
                    .named_children(&mut cursor)
                    .filter(|node| node.kind() != "comment")
                    .count()
            });
            if function.kind() == "field_expression" {
                let Some(receiver) = function.child_by_field_name("argument") else {
                    return Ok(None);
                };
                let Some(ty) = expression_type(
                    store,
                    snapshot,
                    caches,
                    context,
                    receiver,
                    source,
                    remaining - 1,
                )?
                else {
                    return Ok(None);
                };
                let arrow = function
                    .child_by_field_name("operator")
                    .is_some_and(|operator| text(operator) == "->");
                let Some(owner) = class_for_member(&ty, arrow) else {
                    return Ok(None);
                };
                let Some(name) = function.child_by_field_name("field") else {
                    return Ok(None);
                };
                return match member(store, snapshot, caches, context, owner, text(name), arity)? {
                    Some(function) => {
                        return_type(store, snapshot, caches, &function, remaining - 1)
                    }
                    None => Ok(None),
                };
            }
            call_type(
                store,
                snapshot,
                caches,
                context,
                text(function),
                arity,
                remaining,
            )
        }
        _ => Ok(None),
    }
}

fn call_type(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    caches: &mut RuleCaches,
    context: &Context,
    name: &str,
    arity: Option<usize>,
    remaining: usize,
) -> crate::EngineResult<Option<Type>> {
    if let Some(ty) = materialize(store, snapshot, caches, context, name, remaining - 1)? {
        return Ok(Some(ty));
    }
    let Some((name, _)) = template_name(name) else {
        return Ok(None);
    };
    let tail = name.rsplit("::").next().unwrap_or(&name);
    let Some(hits) = snapshot.lookup_candidates(tail) else {
        return Ok(None);
    };
    for scope in if name.starts_with("::") {
        vec![""]
    } else {
        scopes(&context.scope)
    } {
        let name = name.trim_start_matches("::");
        let qualified = if scope.is_empty() {
            name.to_owned()
        } else {
            format!("{scope}::{name}")
        };
        let hits = prefer_definitions(
            snapshot,
            hits.iter()
                .filter(|hit| {
                    hit.qualified_name == qualified
                        && hit.symbol_type == Some(SymbolType::Function)
                        && compatible(snapshot, hit)
                })
                .collect::<Vec<_>>(),
        );
        if hits.is_empty() {
            continue;
        }
        let function = match hits.as_slice() {
            [hit] => Some(*hit),
            _ => arity.and_then(|arity| unique_arity_match(hits.into_iter(), arity)),
        };
        return match function {
            Some(function) => return_type(store, snapshot, caches, function, remaining - 1),
            None => Ok(None),
        };
    }
    Ok(None)
}

pub(super) fn resolve_receiver(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if stored.language != "cpp" || stored.reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(receiver) = stored.reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    let context = Context::reference(snapshot, stored);
    let active = binding(store, caches, &context, receiver)?;
    if active.is_none() {
        return Ok(None);
    }
    let Some(ty) = variable_type(store, snapshot, caches, &context, receiver, 16)? else {
        return Ok(Some(Outcome::Deferred));
    };
    let arrow = stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|raw| raw.contains("->"));
    let Some(owner) = class_for_member(&ty, arrow) else {
        return Ok(Some(Outcome::Deferred));
    };
    let target = member(
        store,
        snapshot,
        caches,
        &context,
        owner,
        &stored.name_tail,
        stored.reference.arity,
    )?;
    Ok(Some(target.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: member_resolution(stored, &hit),
            rule: Rule::Receiver,
        }
    })))
}

pub(super) fn resolve_constructor(
    store: &IndexStore,
    snapshot: &ResolutionSnapshot,
    stored: &StoredPendingRef,
    caches: &mut RuleCaches,
) -> crate::EngineResult<Option<Outcome>> {
    if stored.language != "cpp" || stored.reference.ref_kind != GraphEdgeKind::Calls {
        return Ok(None);
    }
    let Some(raw) = stored
        .reference
        .metadata
        .get("rawText")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let Some(ty) = stored.reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    let allocated = raw.starts_with("new ");
    if !allocated && raw != format!("{ty}{{}}") {
        return Ok(None);
    }
    let Some(Type::Class(owner)) = materialize(
        store,
        snapshot,
        caches,
        &Context::reference(snapshot, stored),
        ty,
        16,
    )?
    else {
        return Ok(Some(Outcome::Deferred));
    };
    let name = owner
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&owner.qualified_name);
    let qualified = format!("{}::{name}", owner.qualified_name);
    let Some(hits) = snapshot.lookup_candidates(name) else {
        return Ok(Some(Outcome::Deferred));
    };
    let constructors = prefer_definitions(
        snapshot,
        hits.iter()
            .filter(|hit| {
                hit.qualified_name == qualified
                    && hit.symbol_type == Some(SymbolType::Function)
                    && compatible(snapshot, hit)
            })
            .collect::<Vec<_>>(),
    );
    let target = if constructors.is_empty() && allocated {
        Some(&owner)
    } else {
        stored
            .reference
            .arity
            .and_then(|arity| unique_arity_match(constructors.into_iter(), arity))
    };
    Ok(Some(target.map_or(Outcome::Deferred, |hit| {
        Outcome::Resolved {
            resolution: member_resolution(stored, hit),
            rule: Rule::Receiver,
        }
    })))
}
