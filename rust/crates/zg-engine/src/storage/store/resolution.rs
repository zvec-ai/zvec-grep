//! Per-operation resolver access; no store lock escapes these methods.
//!
//! The indexing service must recover unfinished files and retain its workspace
//! write lock from the first reference/candidate read through the last writeback.
//! Individual calls do not protect a resolution from intervening file updates.
//!
//! Entity names, ownership and paths are read from zvec, the canonical store;
//! graph.sqlite holds edges alone. One sweep reads them into
//! [`ResolutionSnapshot`] instead of maintaining duplicate SQLite tables, and
//! answers every writeback visibility check from that same snapshot: the
//! caller's workspace write lock freezes visibility for the whole sweep, so
//! re-reading endpoints from zvec per page would only repeat the snapshot.

use std::collections::{HashMap, HashSet};

use tokio_util::sync::CancellationToken;

use super::{IndexStore, StoreState, graph_disabled, graph_error};
use crate::{
    EngineError, EngineResult,
    domain::FileId,
    storage::{
        graph::{
            DeclaredBinding, EntityRecord, ImportReference, NameIndexEntry, PendingRefPage,
            Resolution, ResolutionStats, parse_file_endpoint,
        },
        types::ResolutionCandidate,
    },
};

/// Resolver-facing state of one workspace instant, read from zvec.
///
/// A sweep builds this once and answers every name, ownership, path and
/// import-scope question from memory, so all of its rules reason over one
/// consistent view. Visibility keeps its contract: a name whose candidates
/// include an unfinished file stays hidden rather than resolving against a
/// partial snapshot.
#[derive(Default)]
pub(crate) struct ResolutionSnapshot {
    names: HashMap<String, Vec<NameIndexEntry>>,
    records: HashMap<String, EntityRecord>,
    /// Owning file of every entity, metadata-bearing or not: writeback
    /// visibility must answer for any endpoint, not only named symbols.
    owners: HashMap<String, u32>,
    paths: PathIndex,
    imports: HashMap<u32, Vec<u32>>,
}

impl ResolutionSnapshot {
    /// Name-index hits for `name`, or `None` while any same-named entity's
    /// file is not currently Indexed. A hidden definition could change
    /// uniqueness, so the resolver defers instead of resolving. Bodyless
    /// C/C++ declarations stay in the set: which candidate they name depends
    /// on the reference's language, which only the resolver knows.
    pub(crate) fn lookup_candidates(&self, name: &str) -> Option<&[NameIndexEntry]> {
        let Some(candidates) = self.names.get(name) else {
            return Some(&[]);
        };
        candidates
            .iter()
            .all(|candidate| self.paths.indexed.contains(&candidate.file_id))
            .then_some(candidates.as_slice())
    }

    /// The name-index record of one entity, when it has a symbol name.
    pub(crate) fn entity_record(&self, entity_id: &str) -> Option<&EntityRecord> {
        self.records.get(entity_id)
    }

    /// The workspace-relative path of one Indexed file, forward slashes on
    /// every platform. Hidden files have no path: their specifiers must not
    /// anchor imports.
    pub(crate) fn file_path(&self, file_id: u32) -> Option<&str> {
        self.paths.indexed_by_id.get(&file_id).map(String::as_str)
    }

    /// The Indexed file owning `relative_path`, or `None` when absent or hidden.
    pub(crate) fn file_id_by_path(&self, path: &str) -> Option<u32> {
        self.paths.indexed_by_path.get(path).copied()
    }

    /// Whether the workspace snapshot contains a module rooted at `name`:
    /// a file matching one of the dot-prefixed `extensions`, or any path
    /// inside a directory named `name`. Files that are not yet Indexed count:
    /// an unfinished local module must not be classified as external.
    pub(crate) fn path_has_module(&self, name: &str, extensions: &[&str]) -> bool {
        if extensions
            .iter()
            .any(|extension| self.paths.known.contains(&format!("{name}{extension}")))
        {
            return true;
        }
        let directory = format!("{name}/");
        self.paths
            .known
            .iter()
            .any(|path| path.starts_with(&directory))
    }

    /// Files whose resolved imports one Indexed file points at, deduplicated.
    pub(crate) fn imported_file_ids(&self, file_id: u32) -> &[u32] {
        if !self.paths.indexed.contains(&file_id) {
            return &[];
        }
        self.imports.get(&file_id).map_or(&[], Vec::as_slice)
    }

    /// The Indexed file owning one endpoint, or `None` when the endpoint is
    /// absent or its file is hidden. File endpoints use only the canonical
    /// `f{file_id}` spelling, including files without entities, matching the
    /// zvec-backed visibility check this answers from the snapshot.
    pub(crate) fn visible_file_id(&self, endpoint: &str) -> Option<u32> {
        if let Some(file_id) = parse_file_endpoint(endpoint) {
            // Canonical spelling rejects leading zeros the parse would fold
            // away (`f01` is an entity ID, not file 1).
            let digits = &endpoint[1..];
            if digits == "0" || !digits.starts_with('0') {
                return self.paths.indexed.contains(&file_id).then_some(file_id);
            }
        }
        self.owners
            .get(endpoint)
            .copied()
            .filter(|file_id| self.paths.indexed.contains(file_id))
    }
}

/// Path spellings of every file snapshot, partitioned by publication state.
#[derive(Default)]
struct PathIndex {
    /// IDs of files that finished indexing.
    indexed: HashSet<u32>,
    /// Path of every Indexed file, by ID.
    indexed_by_id: HashMap<u32, String>,
    /// ID of every Indexed file, by path.
    indexed_by_path: HashMap<String, u32>,
    /// Path of every file snapshot, including unfinished ones.
    known: HashSet<String>,
}

impl PathIndex {
    fn read(state: &StoreState) -> EngineResult<Self> {
        let mut index = Self::default();
        for file in state.files.list()? {
            let path = file
                .relative_path()
                .as_path()
                .to_string_lossy()
                .replace('\\', "/");
            if file.index_status().is_indexed() {
                let id = file.id().get();
                index.indexed.insert(id);
                index.indexed_by_id.insert(id, path.clone());
                index.indexed_by_path.insert(path.clone(), id);
            }
            index.known.insert(path);
        }
        Ok(index)
    }
}

/// The workspace name index: named entities by name, one record by ID, and
/// every metadata-bearing entity's owning file.
#[derive(Default)]
struct SymbolIndex {
    names: HashMap<String, Vec<NameIndexEntry>>,
    records: HashMap<String, EntityRecord>,
    owners: HashMap<String, u32>,
}

impl SymbolIndex {
    fn read(state: &StoreState, signal: Option<&CancellationToken>) -> EngineResult<Self> {
        let mut index = Self::default();
        for (entity_id, file_id, metadata) in state.entities.list_symbol_projection(signal)? {
            index.owners.insert(entity_id.clone(), file_id.get());
            // Unnamed entities cannot be referenced by name, so they stay out
            // of both views, matching the file graph's name index. Entities
            // without code metadata keep their ownership entry above.
            let Some(metadata) = metadata else {
                continue;
            };
            let Some(name) = metadata.symbol_name else {
                continue;
            };
            let qualified_name = match &metadata.scope {
                Some(scope) => format!("{scope}::{name}"),
                None => name.clone(),
            };
            let arity = metadata.arity.and_then(|arity| u32::try_from(arity).ok());
            index
                .names
                .entry(name.clone())
                .or_default()
                .push(NameIndexEntry {
                    entity_id: entity_id.clone(),
                    file_id: file_id.get(),
                    qualified_name: qualified_name.clone(),
                    symbol_type: metadata.symbol_type,
                    is_exported: metadata.is_exported,
                    arity,
                    is_declaration: metadata.is_declaration,
                    parameters: parameter_evidence(metadata.signature.as_deref()),
                    package: metadata.package.clone(),
                });
            index.records.insert(
                entity_id.clone(),
                EntityRecord {
                    entity_id,
                    name,
                    qualified_name,
                    is_exported: metadata.is_exported,
                    arity,
                    is_declaration: metadata.is_declaration,
                },
            );
        }
        // Candidate order stays stable. The set stays whole, bodyless
        // declarations included: preferring a definition needs the reference's
        // language, and this index is shared by every language. The resolver
        // applies that preference per reference, after its language gate, over
        // candidates spelling the same qualified name.
        for candidates in index.names.values_mut() {
            candidates.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        }
        Ok(index)
    }
}

/// The parameter list a signature states, normalized for comparison: the
/// function declarator's own parentheses, one uniform spelling for whitespace
/// and for the spacing around `*` and `&`, and each parameter's type only.
///
/// A trailing qualifier's parentheses — `noexcept(true)`, `throw(...)`,
/// `requires(...)` — are not the parameter list, and a list this function
/// cannot place returns `None`, so an entity without that evidence never
/// stands in for one that has it.
fn parameter_evidence(signature: Option<&str>) -> Option<String> {
    let signature = signature?;
    let (start, end) = outer_parameter_group(signature)?;
    let mut parameters = Vec::new();
    let mut depth = 0usize;
    let mut begin = start;
    for (offset, character) in signature[start..end].char_indices() {
        match character {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parameters.push(normalize_parameter(&signature[begin..start + offset]));
                begin = start + offset + character.len_utf8();
            }
            _ => {}
        }
    }
    parameters.push(normalize_parameter(&signature[begin..end]));
    Some(parameters.join(","))
}

/// The span between the parentheses of the function declarator's parameter
/// list: `void f(int)` and `int g() const` both name their parameter list
/// there, `void operator()(int)` skips the operator's own empty pair, and
/// `void f(int) noexcept(true)` does not mistake the qualifier's parentheses
/// for the parameters.
///
/// The last group that a declarator can own wins, so a function-pointer
/// parameter or a template argument in the prefix cannot shadow it; a
/// signature whose only groups are qualifiers yields `None`, leaving the
/// declaration unpaired rather than guessed.
fn outer_parameter_group(signature: &str) -> Option<(usize, usize)> {
    let mut group = None;
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, character) in signature.char_indices() {
        match character {
            '(' => {
                if depth == 0 {
                    start = offset;
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && declares_parameters(&signature[..start], &signature[start + 1..offset])
                {
                    group = Some((start + 1, offset));
                }
            }
            _ => {}
        }
    }
    group
}

/// Whether the text before a parenthesis and its body spell a function
/// declarator's parameter list rather than a trailing qualifier.
///
/// A qualifier keyword never does. An empty body belongs to the function only
/// when the text before it ends with its name; `operator()`'s own pair does
/// not, which is what makes `operator()(int)` read one parameter.
fn declares_parameters(preceding: &str, body: &str) -> bool {
    let Some(word) = preceding.split_whitespace().last() else {
        return false;
    };
    if matches!(
        word,
        "noexcept"
            | "throw"
            | "requires"
            | "decltype"
            | "alignas"
            | "sizeof"
            | "if"
            | "for"
            | "while"
            | "switch"
            | "catch"
            | "static_assert"
            | "__attribute__"
            | "__declspec"
    ) {
        return false;
    }
    !body.trim().is_empty() || word != "operator"
}

/// One parameter without its default argument, without the name it
/// establishes, whitespace collapsed, and pointer/reference spacing made
/// uniform: `const char * value`, `const char* value` and `const char*` all
/// read `const char*`, and `int count = 0` reads `int`, because a header may
/// state a default or omit the name the definition carries.
fn normalize_parameter(parameter: &str) -> String {
    let parameter = parameter.split('=').next().unwrap_or(parameter);
    let collapsed = parameter.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut normalized = String::with_capacity(collapsed.len());
    let mut characters = collapsed.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            ' ' => {
                if normalized.ends_with(['*', '&']) {
                    // Keep the space that separates a pointer from the name
                    // it declares; drop the one before the `*` itself.
                    if characters
                        .peek()
                        .is_some_and(|next| next.is_alphanumeric() || *next == '_')
                    {
                        normalized.push(' ');
                    }
                } else if !normalized.is_empty() && !normalized.ends_with(' ') {
                    normalized.push(' ');
                }
            }
            '*' | '&' => {
                while normalized.ends_with(' ') {
                    normalized.pop();
                }
                normalized.push(character);
                // `char *value` glues the name to the pointer; the space that
                // a name cannot be read without is restored here so the name
                // stays separable on both spellings.
                if characters
                    .peek()
                    .is_some_and(|next| next.is_alphanumeric() || *next == '_')
                {
                    normalized.push(' ');
                }
            }
            _ => normalized.push(character),
        }
    }
    strip_parameter_name(normalized)
}

/// One parameter's type, with the declarator name it establishes removed:
/// `int value` reads `int`, `const char* path` reads `const char*`, and
/// `Token value` reads `Token`.
///
/// The trailing identifier comes off only when the prefix before it leaves no
/// room for the type. `unsigned long` and `struct Foo` keep every word, because
/// reading `long` or `Foo` as a name would merge two different parameters.
/// `const Foo` keeps its own too: a qualifier leaves the type open, so `Foo` is
/// the type of an unnamed parameter, and reading it as a name would merge
/// `const Foo` with `const Bar`.
fn strip_parameter_name(parameter: String) -> String {
    let Some((head, tail)) = parameter.rsplit_once(' ') else {
        return parameter;
    };
    if !is_parameter_name(tail) || leaves_type_open(head) {
        return parameter;
    }
    head.to_owned()
}

/// Whether the token before a trailing identifier leaves the type open, which
/// makes that identifier part of the type rather than the parameter's name.
///
/// A cv qualifier or an elaborated type specifier cannot be followed by a name
/// without a type in between, so `const Foo` and `struct Foo` are complete
/// unnamed types and `const Bar` beside them names a different one. A language
/// that spells `name: Type` puts its name first, so the trailing word there is
/// the type. Every other prefix establishes a name, including a bare type:
/// `int count`, `const char* path`, `Token value` and `uint32_t input` all name
/// their parameter, and the unnamed `Token` keeps its single word so the two
/// spellings still pair.
fn leaves_type_open(prefix: &str) -> bool {
    let last = prefix.rsplit(' ').next().unwrap_or(prefix);
    last.ends_with(':')
        || matches!(
            last,
            "const" | "volatile" | "struct" | "class" | "enum" | "union" | "typename"
        )
}

/// Whether a token can be a parameter's name rather than part of its type.
fn is_parameter_name(token: &str) -> bool {
    let mut characters = token.chars();
    characters
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_')
        && !matches!(
            token,
            "auto"
                | "bool"
                | "char"
                | "char8_t"
                | "char16_t"
                | "char32_t"
                | "class"
                | "const"
                | "double"
                | "enum"
                | "float"
                | "int"
                | "long"
                | "register"
                | "restrict"
                | "short"
                | "signed"
                | "size_t"
                | "struct"
                | "typename"
                | "union"
                | "unsigned"
                | "void"
                | "volatile"
                | "wchar_t"
        )
}

/// Files whose modules one file's resolved imports point at, deduplicated:
/// `f{file_id}` endpoints from import-scoped resolution plus the owning files
/// of name-unique entity targets. This is the import scope that anchors
/// bare-name resolution.
fn import_scopes(
    state: &StoreState,
    owners: &HashMap<String, u32>,
    indexed: &HashSet<u32>,
) -> EngineResult<HashMap<u32, Vec<u32>>> {
    let mut scopes: HashMap<u32, Vec<u32>> = HashMap::new();
    for (file_id, target) in graph(state)?.resolved_imports().map_err(graph_error)? {
        let target = parse_file_endpoint(&target).or_else(|| owners.get(&target).copied());
        let Some(target) = target.filter(|target| indexed.contains(target)) else {
            continue;
        };
        let targets = scopes.entry(file_id).or_default();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    Ok(scopes)
}

impl IndexStore {
    /// One consistent read of every resolver input that lives outside the
    /// graph. Build it once per sweep; writeback revalidates visibility
    /// against this same snapshot. The entity projection dominates the read
    /// and honors `signal`, so a pass cancelled while the snapshot builds
    /// returns the cancellation instead of filling an index it will discard.
    pub(crate) fn resolution_snapshot(
        &self,
        signal: Option<&CancellationToken>,
    ) -> EngineResult<ResolutionSnapshot> {
        self.read(|state| {
            let paths = PathIndex::read(state)?;
            let symbols = SymbolIndex::read(state, signal)?;
            let imports = import_scopes(state, &symbols.owners, &paths.indexed)?;
            Ok(ResolutionSnapshot {
                names: symbols.names,
                records: symbols.records,
                owners: symbols.owners,
                paths,
                imports,
            })
        })
    }

    /// Read one bounded page, hiding refs whose source file is not Indexed.
    /// A filtered-out page may be empty while `next_cursor` still advances.
    /// Visibility is answered from the sweep's snapshot: the caller's
    /// workspace write lock keeps it current for the whole pass.
    /// Requires writable storage and the outer workspace write lock.
    pub(crate) fn list_pending_refs(
        &self,
        limit: usize,
        cursor: i64,
        snapshot: &ResolutionSnapshot,
    ) -> EngineResult<PendingRefPage> {
        self.write(|state| {
            let mut page = state
                .graph
                .as_ref()
                .map_err(|_| graph_disabled())?
                .list_pending_refs(limit, cursor)
                .map_err(graph_error)?;
            page.refs
                .retain(|r| snapshot.visible_file_id(&r.reference.owner_id) == Some(r.file_id));
            Ok(page)
        })
    }

    /// Return every visible exact-name definition, deduplicated across fragments/models.
    /// The resolver selects scope, language and other constraints from metadata.
    /// No search-result cap may be used to infer uniqueness.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn find_resolution_candidates(
        &self,
        name: &str,
    ) -> EngineResult<Vec<ResolutionCandidate>> {
        if name.trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "reference name must not be empty",
            ));
        }
        self.read(|state| {
            let ids = state.fragments.find_symbol_entity_ids(name, None)?;
            let mut result = Vec::new();
            for batch in ids.chunks(128) {
                let mut metadata = state.entities.fetch_code_metadata(batch)?;
                let file_ids = metadata
                    .values()
                    .map(|(file_id, _)| *file_id)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let files = state.files.fetch(&file_ids)?;
                for id in batch {
                    if let Some((file_id, metadata)) = metadata.remove(id)
                        && metadata.symbol_name.as_deref() == Some(name)
                        && let Some(file) = files.get(&file_id)
                        && file.index_status().is_indexed()
                    {
                        result.push(ResolutionCandidate {
                            entity_id: id.clone(),
                            file: file.clone(),
                            metadata,
                        });
                    }
                }
            }
            Ok(result)
        })
    }

    /// Recheck current pending rows and both endpoints before one atomic graph writeback.
    /// This catches missing/hidden endpoints, not changes to name uniqueness: callers
    /// must retain the outer workspace write lock throughout lookup and computation,
    /// which also keeps the snapshot's visibility answers current.
    pub(crate) fn apply_resolutions(
        &self,
        resolutions: &[Resolution],
        snapshot: &ResolutionSnapshot,
    ) -> EngineResult<ResolutionStats> {
        self.write(|state| {
            let ids = resolutions.iter().map(|r| r.ref_id).collect::<Vec<_>>();
            let refs = state
                .graph
                .as_ref()
                .map_err(|_| graph_disabled())?
                .get_pending_refs(&ids)
                .map_err(graph_error)?;
            let eligible = refs
                .iter()
                .filter(|r| snapshot.visible_file_id(&r.reference.owner_id) == Some(r.file_id))
                .map(|r| r.id)
                .collect::<HashSet<_>>();
            let valid = resolutions
                .iter()
                .filter(|r| {
                    eligible.contains(&r.ref_id) && snapshot.visible_file_id(&r.target_id).is_some()
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut stats = state
                .graph
                .as_mut()
                .map_err(|_| graph_disabled())?
                .apply_resolutions(&valid)
                .map_err(graph_error)?;
            stats.stale += resolutions.len() - valid.len();
            Ok(stats)
        })
    }

    /// Import statements of one Indexed file, pending or resolved.
    pub(crate) fn import_references(&self, file_id: u32) -> EngineResult<Vec<ImportReference>> {
        self.read(|state| {
            if !is_indexed(state, file_id)? {
                return Ok(Vec::new());
            }
            graph(state)?
                .import_references(file_id)
                .map_err(graph_error)
        })
    }

    /// Type bindings one entity declares, the resolver's receiver evidence.
    pub(crate) fn declared_bindings(&self, owner_id: &str) -> EngineResult<Vec<DeclaredBinding>> {
        self.read(|state| {
            graph(state)?
                .declared_bindings(owner_id)
                .map_err(graph_error)
        })
    }

    /// Inheritance evidence of one class entity: the base entity IDs whose
    /// extends/implements edges have resolved, plus the base names still
    /// pending. The resolver merges both views against its snapshot, so a
    /// pending base name binds only when it is workspace-unique there.
    pub(crate) fn inheritance_bases(
        &self,
        entity_id: &str,
    ) -> EngineResult<(Vec<String>, Vec<String>)> {
        self.read(|state| {
            graph(state)?
                .inheritance_bases(entity_id)
                .map_err(graph_error)
        })
    }

    /// Terminate pending references whose specifier addresses no workspace
    /// module. Only visible rows change, so hidden files stay queued.
    pub(crate) fn apply_failures(
        &self,
        failures: &[i64],
        snapshot: &ResolutionSnapshot,
    ) -> EngineResult<usize> {
        self.write(|state| {
            let eligible = eligible_ref_ids(state, failures, snapshot)?;
            let valid = failures
                .iter()
                .filter(|id| eligible.contains(id))
                .copied()
                .collect::<Vec<_>>();
            state
                .graph
                .as_mut()
                .expect("writable graph")
                .apply_failures(&valid)
                .map_err(graph_error)
        })
    }

    /// Record ambiguity candidates for still-pending visible rows. Entries
    /// whose candidates are hidden are dropped: they describe a workspace the
    /// resolver could not see completely.
    pub(crate) fn record_candidates(
        &self,
        entries: &[(i64, Vec<String>)],
        snapshot: &ResolutionSnapshot,
    ) -> EngineResult<usize> {
        self.write(|state| {
            let ids = entries.iter().map(|(id, _)| *id).collect::<Vec<_>>();
            let eligible = eligible_ref_ids(state, &ids, snapshot)?;
            let valid = entries
                .iter()
                .filter(|(id, candidates)| {
                    eligible.contains(id)
                        && !candidates.is_empty()
                        && candidates
                            .iter()
                            .all(|id| snapshot.visible_file_id(id).is_some())
                })
                .map(|(id, candidates)| (*id, candidates.as_slice()))
                .collect::<Vec<_>>();
            state
                .graph
                .as_mut()
                .expect("writable graph")
                .record_candidates(&valid)
                .map_err(graph_error)
        })
    }
}

fn graph(state: &StoreState) -> EngineResult<&super::super::graph::SqliteGraphStorage> {
    state.graph.as_ref().map_err(|_| graph_disabled())
}

fn is_indexed(state: &StoreState, file_id: u32) -> EngineResult<bool> {
    let id = FileId::new(file_id);
    let mut files = state.files.fetch(&[id])?;
    Ok(files
        .remove(&id)
        .is_some_and(|file| file.index_status().is_indexed()))
}

/// Pending rows that still exist and whose owning file is Indexed.
fn eligible_ref_ids(
    state: &StoreState,
    ids: &[i64],
    snapshot: &ResolutionSnapshot,
) -> EngineResult<HashSet<i64>> {
    let refs = graph(state)?.get_pending_refs(ids).map_err(graph_error)?;
    Ok(refs
        .iter()
        .filter(|r| snapshot.visible_file_id(&r.reference.owner_id) == Some(r.file_id))
        .map(|r| r.id)
        .collect())
}

#[cfg(test)]
mod parameter_evidence_tests {
    use super::parameter_evidence;

    fn evidence(signature: &str) -> String {
        parameter_evidence(Some(signature)).expect("signature states a parameter list")
    }

    #[test]
    fn trailing_qualifier_parentheses_are_not_the_parameter_list() {
        // `noexcept(true)` and `throw(...)` sit where a parameter list could
        // be; reading them would give two different overloads the same
        // parameters and let one stand in for the other.
        assert_eq!(
            evidence("void compute(const char*) noexcept(true);"),
            "const char*"
        );
        assert_eq!(evidence("void compute(int) noexcept(true) {}"), "int");
        assert_eq!(
            evidence("void compute(const char*) throw();"),
            "const char*"
        );
        assert_eq!(evidence("int f(int) requires (C<T>)"), "int");
        // An operator name's own empty pair precedes the parameters.
        assert_eq!(evidence("void operator()() const"), "");
        assert_eq!(evidence("void operator()(int) const"), "int");
    }

    #[test]
    fn pointer_and_reference_spacing_never_hides_the_type() {
        // A prototype may name the parameter or leave it out, and the
        // definition may spell the spacing differently; both sides must read
        // the same type.
        assert_eq!(
            evidence("void f(const char*);"),
            evidence("void f(const char* path);")
        );
        assert_eq!(
            evidence("void f(const char *);"),
            evidence("void f(const char *path);")
        );
        assert_eq!(
            evidence("void f(const char * value);"),
            evidence("void f(const char* value);")
        );
        assert_eq!(evidence("void f(char** argv);"), "char**");
        assert_eq!(
            evidence("void f(const std::string&);"),
            evidence("void f(const std::string& value);")
        );
        assert_eq!(evidence("void f(int count);"), evidence("void f(int n);"));
    }

    #[test]
    fn default_arguments_and_keywords_stay_distinct() {
        // A header states the default the definition cannot repeat.
        assert_eq!(evidence("void f(int count = 0);"), evidence("void f(int);"));
        // `unsigned` and `unsigned long`, or `struct Foo` and `Foo`, are
        // different types: the last word of each is a type, not a name.
        assert_eq!(evidence("void f(unsigned);"), "unsigned");
        assert_eq!(evidence("void f(unsigned long);"), "unsigned long");
        assert_eq!(evidence("void f(struct Foo);"), "struct Foo");
        assert_eq!(
            evidence("void f(unsigned)"),
            "unsigned",
            "no trailing name to strip"
        );
    }

    #[test]
    fn a_user_defined_type_still_yields_its_name() {
        // Two identifiers in a row make the last one the name, so an unnamed
        // `Token` pairs with the `Token value` a definition spells, and a
        // typedef name pairs two renamed parameters.
        assert_eq!(evidence("void consume(Token);"), "Token");
        assert_eq!(evidence("void consume(Token value) {}"), "Token");
        assert_eq!(evidence("void f(uint32_t input);"), "uint32_t");
        assert_eq!(evidence("void f(uint32_t value) {}"), "uint32_t");
        // The type stays whole where the prefix opens it, and a `name: Type`
        // spelling keeps its type at the end.
        assert_eq!(evidence("void f(const Foo);"), "const Foo");
        assert_eq!(evidence("void f(struct Foo);"), "struct Foo");
        assert_eq!(evidence("void f(a: i32);"), "a: i32");
    }

    #[test]
    fn a_qualifier_leaves_the_type_open() {
        // `const Foo` and `const Bar` are two complete types, not one type
        // followed by two parameter names: nothing before `Foo` completes a
        // type, so the qualifier's operand stays part of the parameter.
        assert_eq!(evidence("void f(const Foo);"), "const Foo");
        assert_eq!(evidence("void f(const Bar) {}"), "const Bar");
        assert_ne!(
            evidence("void f(const Foo);"),
            evidence("void f(const Bar) {}")
        );
        assert_eq!(evidence("void f(volatile Foo);"), "volatile Foo");
        // A completed type still yields its name: the pointer, the qualifier
        // after it, and the qualified name all end the type.
        assert_eq!(evidence("void f(const Foo* bar);"), "const Foo*");
        assert_eq!(evidence("void f(std::string s);"), "std::string");
        assert_eq!(evidence("void f(Foo<Bar> baz);"), "Foo<Bar>");
    }
}
