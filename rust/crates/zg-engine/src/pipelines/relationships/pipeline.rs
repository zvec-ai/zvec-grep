//! Match canonical symbols, read selected edges, and assemble bounded result groups.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use super::{rust_modules::RustModules, storage::RelationshipStorage};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineResult,
    api::relationships::{
        MAX_SYMBOL_MATCHES, RelationshipEdge, RelationshipOptions, RelationshipResult,
        SymbolDefinition, SymbolRelationships,
    },
    domain::{
        CodeMetadata, Entity, EntityId, EntityMetadata, FileEdge, FileRecord, GraphEdgeKind,
        Language, Range,
    },
    pipelines::indexed_search::path_filter::resolve_glob_filter,
    storage::{
        ReadView,
        graph::Direction,
        parameter_evidence,
        types::{StorageSearchFilter, StoredEntity},
    },
};

const MAX_SYMBOL_SUGGESTIONS: usize = 5;
const ENDPOINT_BATCH_SIZE: usize = 256;
/// Inheritance is workspace data, so the override walk is bounded: a hierarchy
/// deeper than this, or wider than this many bases, answers with what it read.
const MAX_OVERRIDE_DEPTH: usize = 8;
const MAX_OVERRIDE_BASES: usize = 32;

pub(super) fn normalize(options: &RelationshipOptions) -> EngineResult<(String, Option<String>)> {
    options.validate()?;
    Ok(split_symbol(options.symbol.trim()))
}

fn split_symbol(symbol: &str) -> (String, Option<String>) {
    let (absolute, path) = symbol
        .strip_prefix("::")
        .map_or((false, symbol), |rest| (true, rest));
    let (scope, name) = match path.rsplit_once("::") {
        Some((scope, name)) => (Some(scope), name),
        // `::main` names a top-level definition; plain `main` matches any scope.
        None if absolute => (Some(""), path),
        None => (None, path),
    };
    (name.to_owned(), scope.map(|scope| scope.replace('.', "::")))
}

pub(super) fn query_symbols(
    storage: &impl RelationshipStorage,
    name: &str,
    scope: Option<&str>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    check_cancelled(options.signal.as_ref())?;
    let filter = resolve_glob_filter(
        std::path::Path::new(""),
        &options.globs,
        || storage.has_non_unicode_file_names(),
        false,
        || storage.list_file_paths(),
    )?;
    let mut result =
        collect_relationships(storage, name, scope, Some(&filter), options, direction)?;
    result.matches.sort_by(|a, b| {
        a.symbol
            .file_path
            .cmp(&b.symbol.file_path)
            .then_with(|| a.symbol.start_line.cmp(&b.symbol.start_line))
            .then_with(|| a.symbol.name.cmp(&b.symbol.name))
    });
    Ok(result)
}

fn collect_relationships(
    storage: &impl RelationshipStorage,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
    options: &RelationshipOptions,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    check_cancelled(options.signal.as_ref())?;
    // Selection reads metadata, file records and bodies together: file_id alone
    // cannot detect a same-ID rewrite between these steps.
    // Preserve literal dots (e.g. api::user.get) before trying dotted qualification.
    let dotted = split_symbol(&options.symbol.trim().replace('.', "::"));
    let (mut name, mut scope) = (name, scope);
    let definitions = storage.read_snapshot(|view| {
        check_cancelled(options.signal.as_ref())?;
        view.ensure_graph()?;
        let definitions = find_symbols(view, name, scope, filter)?;
        if definitions.is_empty()
            && name
                .rsplit_once('.')
                .is_some_and(|(scope, name)| !scope.is_empty() && !name.is_empty())
        {
            name = &dotted.0;
            scope = dotted.1.as_deref();
            find_symbols(view, name, scope, filter)
        } else {
            Ok(definitions)
        }
    })?;
    let definitions = prefer_definitions(definitions);
    let mut matches = Vec::new();
    for stored in &definitions {
        check_cancelled(options.signal.as_ref())?;
        // Keep the root recheck, edges, every visibility batch and endpoint
        // descriptions together. Writers may run between definitions, never
        // between reading an edge and checking its endpoints.
        if let Some(group) = storage.read_snapshot(|view| {
            check_cancelled(options.signal.as_ref())?;
            collect_match(
                view,
                stored,
                options.limit.unwrap_or(20),
                direction,
                options.kinds.as_deref(),
                options.signal.as_ref(),
            )
        })? {
            matches.push(group);
        }
    }
    // Stored definitions may lack text coordinates and be impossible to describe.
    // Only usable matches should suppress suggestions, including exact-name suggestions.
    let mut suggestions = Vec::new();
    if matches.is_empty() {
        if scope.is_some() {
            suggestions = storage
                .read_snapshot(|view| suggest_exact_symbols(view, name, filter))
                .unwrap_or_default()
                .iter()
                .filter_map(describe)
                .collect();
        }
        if suggestions.is_empty() {
            // Suggestions are optional: FTS syntax/storage errors must not turn
            // a successful exact lookup with no usable matches into a failed query.
            suggestions = storage
                .read_snapshot(|view| suggest_symbols(view, name, filter))
                .unwrap_or_default()
                .iter()
                .filter_map(describe)
                .collect();
        }
    }
    Ok(RelationshipResult {
        matches,
        suggestions,
    })
}

fn check_cancelled(signal: Option<&CancellationToken>) -> EngineResult<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(EngineError::cancelled("relationship query was cancelled"));
    }
    Ok(())
}

/// Drops the pure declarations a definition of the same symbol supersedes.
///
/// A C++ member is a prototype in the header and a bounded body in the
/// implementation file, so the workspace holds two entities under one name.
/// A declaration states no calls of its own — its node would answer every edge
/// list with an empty one — and the repository already treats it as the
/// weaker location for name-based binding, so a query reports the definition
/// and leaves the prototype out. A workspace that holds only the declaration
/// still answers, since that is the one known location.
///
/// The qualified name is the symbol, not the statement: an in-class prototype
/// and the out-of-class body written in one file are one implementation, and
/// reporting them apart would answer one implementation twice. The scope
/// belongs to that spelling: a pure virtual `S0::v5` and the `S2::v5` that
/// overrides it name different members, so the base declaration stays an
/// answer of its own even while a body of the same bare name exists.
fn prefer_definitions(definitions: Vec<StoredEntity>) -> Vec<StoredEntity> {
    /// One definition's evidence: the symbol its qualified name spells, the
    /// parameters it counts and the list its signature states.
    type Evidence = (String, Option<usize>, Option<String>);
    let defined: Vec<Evidence> = definitions
        .iter()
        .filter(|stored| !is_declaration(stored))
        .filter_map(|stored| {
            let metadata = code_metadata(stored)?;
            Some((
                qualified_symbol(stored)?,
                metadata.arity,
                parameter_evidence(metadata.signature.as_deref()),
            ))
        })
        .collect();
    definitions
        .into_iter()
        .filter(|stored| {
            if !is_declaration(stored) {
                return true;
            }
            // A qualified name does not identify a C++ overload, so the pair
            // also matches the parameters: `void f(int);` and `void f() {}`
            // share a scope without naming one member.
            let (Some(metadata), Some(symbol)) = (code_metadata(stored), qualified_symbol(stored))
            else {
                return true;
            };
            let parameters = parameter_evidence(metadata.signature.as_deref());
            !defined.iter().any(|(defined, arity, list)| {
                *defined == symbol
                    && same_parameters(
                        metadata.arity,
                        parameters.as_deref(),
                        *arity,
                        list.as_deref(),
                    )
            })
        })
        .collect()
}

fn code_metadata(stored: &StoredEntity) -> Option<&CodeMetadata> {
    match stored.entity.metadata() {
        Some(EntityMetadata::Code(metadata)) => Some(metadata),
        _ => None,
    }
}

fn is_declaration(stored: &StoredEntity) -> bool {
    code_metadata(stored).is_some_and(|metadata| metadata.is_declaration)
}

fn qualified_symbol(stored: &StoredEntity) -> Option<String> {
    code_metadata(stored).map(|metadata| {
        let name = metadata.symbol_name.as_deref().unwrap_or_default();
        metadata
            .scope
            .as_deref()
            .map_or_else(|| name.to_owned(), |scope| format!("{scope}::{name}"))
    })
}

fn collect_match(
    storage: &ReadView<'_>,
    stored: &StoredEntity,
    limit: usize,
    direction: Direction,
    kinds: Option<&[GraphEdgeKind]>,
    signal: Option<&CancellationToken>,
) -> EngineResult<Option<SymbolRelationships>> {
    let id = stored.entity.id();
    let current = get_entities(storage, std::slice::from_ref(id))?
        .remove(id)
        .filter(|current| current.file.index_status().is_indexed());
    // Entity IDs do not encode metadata or the containing file's revision.
    // Skip roots changed since selection rather than mix their old description
    // with newer edges. A later query can select the updated definition.
    if current.as_ref() != Some(stored) {
        return Ok(None);
    }
    let Some(symbol) = describe(stored) else {
        return Ok(None);
    };
    // Cache only within this definition's snapshot, including self-loops.
    let mut symbols = HashMap::from([(id.clone(), Some(symbol.clone()))]);
    let mut edges = neighborhood(
        storage,
        id.as_str(),
        direction,
        kinds,
        limit.saturating_add(1),
        &[],
        signal,
    )?;
    // A call through a base reference dispatches into the body that overrides
    // the base member, so those callers answer for this definition too.
    if direction == Direction::In && edges.len() <= limit {
        let inherited = inherited_callers(
            storage,
            stored,
            limit.saturating_add(1) - edges.len(),
            kinds,
            signal,
        )?;
        for (edge, file_path) in inherited {
            if !edges.iter().any(|(seen, _)| same_call_site(seen, &edge)) {
                edges.push((edge, file_path));
            }
        }
    }
    let truncated = edges.len() > limit;
    edges.truncate(limit);
    let targets: Vec<_> = edges
        .iter()
        .map(|(edge, _)| {
            EntityId::from_string(if edge.source == id.as_str() {
                edge.target.clone()
            } else {
                edge.source.clone()
            })
        })
        .collect();
    let missing: Vec<_> = targets
        .iter()
        .filter(|target| !symbols.contains_key(*target))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    check_cancelled(signal)?;
    let endpoints = get_entities(storage, &missing)?;
    check_cancelled(signal)?;
    for id in missing {
        symbols.insert(id.clone(), endpoints.get(&id).and_then(describe));
    }
    Ok(Some(SymbolRelationships {
        symbol,
        truncated,
        edges: edges
            .into_iter()
            .zip(targets)
            .map(|((edge, file_path), target)| RelationshipEdge {
                kind: edge.kind,
                direction: if edge.source == edge.target {
                    Direction::Both
                } else if edge.source == id.as_str() {
                    Direction::Out
                } else {
                    Direction::In
                },
                file_path,
                symbol: symbols.get(&target).cloned().flatten(),
                line: edge.line,
                column: edge.column,
            })
            .collect(),
    }))
}

/// A call through a base reference is stored at the base member the file can
/// see, but it dispatches into the body that overrides it, so those callers
/// answer for the overriding body too.
fn inherited_callers(
    storage: &ReadView<'_>,
    stored: &StoredEntity,
    limit: usize,
    kinds: Option<&[GraphEdgeKind]>,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<(FileEdge, PathBuf)>> {
    if limit == 0 || !kinds.is_some_and(|kinds| kinds.contains(&GraphEdgeKind::Calls)) {
        return Ok(Vec::new());
    }
    let members = overridden_members(storage, stored, signal)?;
    // The body that overrides a base member may name that base explicitly to
    // reach the implementation it replaced (`Derived::Func() { Base::Func(); }`).
    // The row lands on the base member like any other call, but the body it is
    // written in is the one being queried, so it answers nothing and must not
    // take a slot: the limit counts callers, not rows.
    let excluded = [stored.entity.id().as_str()];
    let mut edges = Vec::new();
    for member in &members {
        let Some(spare) = limit.checked_sub(edges.len()).filter(|spare| *spare > 0) else {
            break;
        };
        edges.extend(neighborhood(
            storage,
            member.as_str(),
            Direction::In,
            Some(&[GraphEdgeKind::Calls]),
            spare,
            &excluded,
            signal,
        )?);
    }
    Ok(edges)
}

/// The members a body overrides: written in the scope of a class on its own
/// base chain, spelled with the same name and parameters, and virtual there.
///
/// Only C++ binds a receiver to a base declaration that a derived body later
/// overrides, so other languages have none. The walk is bounded because a
/// hierarchy is workspace data, not a property of the query.
fn overridden_members(
    storage: &ReadView<'_>,
    stored: &StoredEntity,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<EntityId>> {
    let Some(metadata) = code_metadata(stored) else {
        return Ok(Vec::new());
    };
    if metadata.language != Some(Language::Cpp) {
        return Ok(Vec::new());
    }
    // A scope-less symbol is free-standing: no class holds it and nothing dispatches to it.
    let (Some(member), Some(scope)) = (
        metadata
            .symbol_name
            .as_deref()
            .filter(|name| !name.is_empty()),
        metadata.scope.as_deref().filter(|scope| !scope.is_empty()),
    ) else {
        return Ok(Vec::new());
    };
    let classes = member_classes(storage, scope, stored.entity.id(), signal)?;
    let bases = base_chain(storage, classes, signal)?;
    if bases.is_empty() {
        return Ok(Vec::new());
    }
    // Each base's same-named member is where a call through that base is stored;
    // a base that does not declare the member contributes nothing.
    let base_metadata = storage.fetch_code_metadata(&bases)?;
    let mut base_scopes: Vec<String> = Vec::new();
    for base in &bases {
        if let Some((_, metadata)) = base_metadata.get(base)
            && let Some(scope) = qualified_metadata(metadata)
            && !base_scopes.contains(&scope)
        {
            base_scopes.push(scope);
        }
    }
    let mut candidates: Vec<EntityId> = Vec::new();
    for scope in base_scopes {
        check_cancelled(signal)?;
        for (id, _) in select_symbols(storage, member, Some(&scope), None, MAX_SYMBOL_MATCHES)? {
            if id.as_str() != stored.entity.id().as_str() && !candidates.contains(&id) {
                candidates.push(id);
            }
        }
    }
    // A call dispatches only through a virtual base member: a non-virtual one
    // is merely hidden by the same-named body, and the call stays the base's.
    // The specifier is written once, though, and not always on the entity the
    // call reaches: `virtual void Func();` stands in the header while the body
    // that carries the call sites writes plain `void Base::Func()`, so the two
    // spellings are one member and either one marks it. An override that
    // repeats only `override` marks its own member the same way.
    let declared = storage.fetch_code_metadata(&candidates)?;
    let marked: Vec<&CodeMetadata> = candidates
        .iter()
        .filter_map(|id| declared.get(id).map(|(_, metadata)| metadata))
        .filter(|base| is_virtual(base) && same_arity(metadata.arity, base.arity))
        .collect();
    Ok(candidates
        .into_iter()
        .filter(|id| {
            declared.get(id).is_some_and(|(_, base)| {
                same_arity(metadata.arity, base.arity)
                    && marked.iter().any(|other| same_member(base, other))
            })
        })
        .collect())
}

/// Whether two spellings name one member: the class both are written in and
/// the parameters they state.
fn same_member(left: &CodeMetadata, right: &CodeMetadata) -> bool {
    left.scope == right.scope
        && left.symbol_name == right.symbol_name
        && same_parameters(
            left.arity,
            parameter_evidence(left.signature.as_deref()).as_deref(),
            right.arity,
            parameter_evidence(right.signature.as_deref()).as_deref(),
        )
}

/// Whether two spellings state one symbol's parameters: the lists their
/// signatures state when both are readable, and the counted parameters
/// otherwise. A signature that states no list the reader can place leaves the
/// count as the only evidence, which is what the index holds of such an entity
/// anyway.
fn same_parameters(
    left: Option<usize>,
    left_parameters: Option<&str>,
    right: Option<usize>,
    right_parameters: Option<&str>,
) -> bool {
    match (left_parameters, right_parameters) {
        (Some(left), Some(right)) => left == right,
        _ => same_arity(left, right),
    }
}

/// The class bodies a member's scope names. Two files may declare one qualified
/// class; the body that holds the member names the bases it overrides, and the
/// rest are namesakes.
fn member_classes(
    storage: &ReadView<'_>,
    scope: &str,
    member: &EntityId,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<EntityId>> {
    let (class_name, class_scope) = split_symbol(scope);
    // A scope that names no class (`A::`) holds the member in no body.
    if class_name.is_empty() {
        return Ok(Vec::new());
    }
    let classes: Vec<_> = select_symbols(
        storage,
        &class_name,
        class_scope.as_deref(),
        None,
        MAX_SYMBOL_MATCHES,
    )?
    .into_iter()
    .map(|(id, _)| id)
    .filter(|id| id.as_str() != member.as_str())
    .collect();
    if classes.len() <= 1 {
        return Ok(classes);
    }
    let owning = containing_classes(storage, &classes, member, signal)?;
    Ok(if owning.is_empty() { classes } else { owning })
}

/// Every base class reachable from `classes`, breadth first and bounded.
fn base_chain(
    storage: &ReadView<'_>,
    classes: Vec<EntityId>,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<EntityId>> {
    let mut bases = Vec::new();
    let mut visited = HashSet::new();
    let mut frontier = classes;
    for _ in 0..MAX_OVERRIDE_DEPTH {
        check_cancelled(signal)?;
        let mut level = Vec::new();
        for class in &frontier {
            for base in supertypes(storage, class, signal)? {
                if visited.insert(base.clone()) {
                    level.push(base);
                }
            }
        }
        if level.is_empty() {
            break;
        }
        let spare = MAX_OVERRIDE_BASES.saturating_sub(bases.len());
        bases.extend(level.iter().take(spare).cloned());
        if bases.len() == MAX_OVERRIDE_BASES {
            break;
        }
        frontier = level;
    }
    Ok(bases)
}

/// The class bodies among `classes` that declare `member`.
fn containing_classes(
    storage: &ReadView<'_>,
    classes: &[EntityId],
    member: &EntityId,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<EntityId>> {
    let mut owning = Vec::new();
    for class in classes {
        check_cancelled(signal)?;
        let contains: Vec<FileEdge> = storage.neighborhood(
            class.as_str(),
            Direction::Out,
            Some(&[GraphEdgeKind::Contains]),
            |rows| rows.collect(),
        )?;
        if contains.iter().any(|edge| edge.target == member.as_str()) {
            owning.push(class.clone());
        }
    }
    Ok(owning)
}

/// The direct base classes of one class, in recorded order.
fn supertypes(
    storage: &ReadView<'_>,
    class: &EntityId,
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<EntityId>> {
    check_cancelled(signal)?;
    let bases: Vec<FileEdge> = storage.neighborhood(
        class.as_str(),
        Direction::Out,
        Some(&[GraphEdgeKind::Extends, GraphEdgeKind::Implements]),
        |rows| rows.collect(),
    )?;
    Ok(bases
        .into_iter()
        .map(|edge| EntityId::from_string(edge.target))
        .collect())
}

/// The qualified name of one stored symbol, as a scope spells it.
fn qualified_metadata(metadata: &CodeMetadata) -> Option<String> {
    let name = metadata
        .symbol_name
        .as_deref()
        .filter(|name| !name.is_empty())?;
    Some(
        match metadata.scope.as_deref().filter(|scope| !scope.is_empty()) {
            Some(scope) => format!("{scope}::{name}"),
            None => name.to_owned(),
        },
    )
}

/// Whether one base member dispatches into a derived body. Extraction keeps the
/// declaration syntax up to the body, so the specifier a base spells is the
/// evidence that the same-named body under a derived class overrides it: the
/// introducer writes `virtual`, and an override repeats the decision with
/// `override` even where it never repeats the base's `virtual`.
fn is_virtual(metadata: &CodeMetadata) -> bool {
    metadata.signature.as_deref().is_some_and(|signature| {
        signature
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .any(|word| matches!(word, "virtual" | "override"))
    })
}

/// An override repeats the base member's parameters; a body of another arity
/// only shares the name. Unknown counts never rule a member out.
fn same_arity(derived: Option<usize>, base: Option<usize>) -> bool {
    match (derived, base) {
        (Some(derived), Some(base)) => derived == base,
        _ => true,
    }
}

/// One recorded call site, however many hierarchies report it.
fn same_call_site(seen: &FileEdge, edge: &FileEdge) -> bool {
    seen.source == edge.source
        && seen.target == edge.target
        && seen.line == edge.line
        && seen.column == edge.column
}

fn describe(stored: &StoredEntity) -> Option<SymbolDefinition> {
    let Some(EntityMetadata::Code(metadata)) = stored.entity.metadata() else {
        return None;
    };
    let name = metadata
        .symbol_name
        .as_deref()
        .filter(|name| !name.is_empty())?;
    let Range::Text(range) = stored.entity.source_range() else {
        return None;
    };
    Some(SymbolDefinition {
        name: match metadata.scope.as_deref().filter(|scope| !scope.is_empty()) {
            Some(scope) => format!("{scope}::{name}"),
            None => name.to_owned(),
        },
        file_path: stored.file.relative_path().to_path_buf(),
        start_line: range.start_line(),
        end_line: range.end_line(),
    })
}

/// Bounded best-effort suggestions, never relationship query roots.
/// Do not require the requested scope: a misspelled scope is a useful retry case.
fn suggest_symbols(
    storage: &ReadView<'_>,
    name: &str,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let query = name.to_lowercase();
    let hits = storage.search_fts(&query, 100, filter)?;
    let mut seen = HashSet::new();
    let ids: Vec<_> = hits
        .into_iter()
        .map(|hit| hit.entity_id)
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let mut metadata = storage.fetch_code_metadata(&ids)?;
    let mut candidates: Vec<_> = ids
        .into_iter()
        .filter_map(|id| {
            let (file_id, metadata) = metadata.remove(&id)?;
            let rank = suggestion_rank(&query, metadata.symbol_name.as_deref()?)?;
            Some((id, file_id, rank))
        })
        .collect();
    // Stable sort preserves FTS relevance within each name-match category.
    // Rank before limiting, and hydrate only the selected visible definitions.
    candidates.sort_by_key(|(_, _, rank)| *rank);
    let file_ids: Vec<_> = candidates
        .iter()
        .map(|(_, file_id, _)| *file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let files = storage.fetch_files(&file_ids)?;
    let selected: Vec<_> = candidates
        .into_iter()
        .filter_map(|(id, file_id, _)| {
            let file = files.get(&file_id)?;
            file.index_status().is_indexed().then(|| (id, file.clone()))
        })
        .take(MAX_SYMBOL_SUGGESTIONS)
        .collect();
    load_symbols(storage, selected)
}

/// Resolve bounded roots and reuse their checked file records while hydrating.
pub(super) fn find_symbols(
    storage: &ReadView<'_>,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let selected = select_symbols(storage, name, scope, filter, MAX_SYMBOL_MATCHES + 1)?;
    if selected.len() > MAX_SYMBOL_MATCHES {
        return Err(EngineError::invalid_argument(format!(
            "symbol query matches more than {MAX_SYMBOL_MATCHES} indexed definitions; use a narrower scope::name or glob filter",
        )));
    }
    load_symbols(storage, selected)
}

/// Exact bare-name candidates for a wrong scope, bounded without an overflow error.
fn suggest_exact_symbols(
    storage: &ReadView<'_>,
    name: &str,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Vec<StoredEntity>> {
    let selected = select_symbols(storage, name, None, filter, MAX_SYMBOL_SUGGESTIONS)?;
    load_symbols(storage, selected)
}

fn select_symbols(
    storage: &ReadView<'_>,
    name: &str,
    scope: Option<&str>,
    filter: Option<&StorageSearchFilter>,
    limit: usize,
) -> EngineResult<Vec<(EntityId, FileRecord)>> {
    if name.is_empty() {
        return Err(EngineError::invalid_argument(
            "symbol name must not be empty",
        ));
    }
    let ids = storage.find_symbol_entity_ids(name, filter)?;
    let mut selected = Vec::new();
    let mut modules = RustModules::default();
    'batches: for batch in ids.chunks(128) {
        let mut owners: HashMap<_, _> = storage
            .fetch_code_metadata(batch)?
            .into_iter()
            .filter_map(|(id, (file_id, metadata))| {
                (metadata.symbol_name.as_deref() == Some(name)
                    && scope.is_none_or(|scope| {
                        metadata.scope.as_deref().unwrap_or("").replace('.', "::") == scope
                            || metadata.language == Some(Language::Rust)
                    }))
                .then_some((id, (file_id, metadata)))
            })
            .collect();
        let file_ids: Vec<_> = owners
            .values()
            .map(|(file_id, _)| *file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let files = storage.fetch_files(&file_ids)?;
        for id in batch {
            if let Some((file_id, metadata)) = owners.remove(id)
                && let Some(file) = files
                    .get(&file_id)
                    .filter(|file| file.index_status().is_indexed())
            {
                if let Some(scope) = scope
                    && metadata.scope.as_deref().unwrap_or("").replace('.', "::") != scope
                    && !modules.matches_scope(storage, file, &metadata, scope)?
                {
                    continue;
                }
                selected.push((id.clone(), file.clone()));
                if selected.len() == limit {
                    break 'batches;
                }
            }
        }
    }
    Ok(selected)
}

fn load_symbols(
    storage: &ReadView<'_>,
    selected: Vec<(EntityId, FileRecord)>,
) -> EngineResult<Vec<StoredEntity>> {
    let ids: Vec<_> = selected.iter().map(|(id, _)| id.clone()).collect();
    let mut entities = storage.fetch_entities(&ids)?;
    // Selection already read and checked these records under the same store lock.
    Ok(selected
        .into_iter()
        .filter_map(|(id, file)| {
            let entity = entities.remove(&id)?;
            (entity.file_id() == file.id()).then_some(StoredEntity { entity, file })
        })
        .collect())
}

/// Filter resolved edges by endpoint visibility before applying the limit.
///
/// `excluded` names the sources whose rows are read but never counted: a body
/// that names the member it overrides reaches that implementation directly, so
/// its call site is not a caller of anything the query asks about. Skipping the
/// rows while reading keeps the limit describing the answer rather than the
/// table.
fn neighborhood(
    storage: &ReadView<'_>,
    id: &str,
    direction: Direction,
    kinds: Option<&[GraphEdgeKind]>,
    limit: usize,
    excluded: &[&str],
    signal: Option<&CancellationToken>,
) -> EngineResult<Vec<(FileEdge, PathBuf)>> {
    check_cancelled(signal)?;
    storage.neighborhood(id, direction, kinds, |rows| {
        let mut edges = Vec::new();
        let mut hydration_size = limit.clamp(1, ENDPOINT_BATCH_SIZE);
        let mut checked = HashSet::new();
        let mut visible = HashMap::new();
        'read: while edges.len() < limit {
            check_cancelled(signal)?;
            let mut batch = Vec::with_capacity(hydration_size);
            let mut drained = false;
            for _ in 0..hydration_size {
                check_cancelled(signal)?;
                let Some(edge) = rows.next() else {
                    drained = true;
                    break;
                };
                let edge = edge?;
                if !excluded.contains(&edge.source.as_str()) {
                    batch.push(edge);
                }
                check_cancelled(signal)?;
            }
            if batch.is_empty() {
                // Only rows the caller excluded so far: what follows may still
                // answer, so read further instead of stopping here.
                if drained {
                    break 'read;
                }
                hydration_size = hydration_size.saturating_mul(2).min(ENDPOINT_BATCH_SIZE);
                continue;
            }
            // SQLite can be ahead of zvec after a failed write. Filter both
            // endpoints before counting a row toward the requested limit.
            let ids: Vec<_> = batch
                .iter()
                .flat_map(|edge| [&edge.source, &edge.target])
                .map(|id| EntityId::from_string(id.clone()))
                .filter(|id| checked.insert(id.clone()))
                .collect();
            visible.extend(endpoint_paths(storage, &ids)?);
            check_cancelled(signal)?;
            let before = edges.len();
            let remaining = limit - before;
            edges.extend(
                batch
                    .into_iter()
                    .filter_map(|edge| {
                        let path = visible.get(&EntityId::from_string(edge.source.clone()))?;
                        visible.get(&EntityId::from_string(edge.target.clone()))?;
                        Some((edge, path.clone()))
                    })
                    .take(remaining),
            );
            // Avoid reading unused endpoints, but grow batches across invisible stretches.
            hydration_size = if edges.len() == before {
                hydration_size.saturating_mul(2).min(ENDPOINT_BATCH_SIZE)
            } else {
                (limit - edges.len()).clamp(1, ENDPOINT_BATCH_SIZE)
            };
        }
        Ok(edges)
    })
}

/// Smaller ranks are better; reverse containment needs a meaningful name length.
fn suggestion_rank(query: &str, candidate: &str) -> Option<u8> {
    let candidate = candidate.to_lowercase();
    if candidate.is_empty() {
        None
    } else if candidate == query {
        Some(0)
    } else if candidate.starts_with(query) {
        Some(1)
    } else if candidate.contains(query) {
        Some(2)
    } else if candidate.chars().count() >= 3 && query.contains(&candidate) {
        Some(3)
    } else {
        None
    }
}

fn get_entities(
    storage: &ReadView<'_>,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, StoredEntity>> {
    let entities = storage.fetch_entities(ids)?;
    let file_ids = entities
        .values()
        .map(Entity::file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = storage.fetch_files(&file_ids)?;
    Ok(entities
        .into_iter()
        .filter_map(|(id, entity)| {
            let file = files.get(&entity.file_id())?;
            Some((
                id,
                StoredEntity {
                    entity,
                    file: file.clone(),
                },
            ))
        })
        .collect())
}

fn endpoint_paths(
    storage: &ReadView<'_>,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, PathBuf>> {
    Ok(storage
        .visible_endpoint_files(ids)?
        .into_iter()
        .map(|(id, file)| (id, file.relative_path().to_path_buf()))
        .collect())
}
