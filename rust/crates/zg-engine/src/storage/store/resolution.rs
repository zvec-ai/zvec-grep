//! Per-operation resolver access; no store lock escapes these methods.
//!
//! The indexing service must recover unfinished files and retain its workspace
//! write lock from the first reference/candidate read through the last writeback.
//! Individual calls do not protect a resolution from intervening file updates.
//!
//! Entity names, ownership and paths are read from zvec, the canonical store;
//! graph.sqlite holds edges alone. One sweep reads them into
//! [`ResolutionSnapshot`] instead of maintaining duplicate SQLite tables.

use std::collections::{HashMap, HashSet};

use super::{IndexStore, StoreState, graph_error};
use crate::{
    EngineError, EngineResult,
    domain::{EntityId, FileId},
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
pub(crate) struct ResolutionSnapshot {
    names: HashMap<String, Vec<NameIndexEntry>>,
    records: HashMap<String, EntityRecord>,
    paths: PathIndex,
    imports: HashMap<u32, Vec<u32>>,
}

impl ResolutionSnapshot {
    /// Name-index hits for `name`, or `None` while any same-named entity's
    /// file is not currently Indexed. A hidden definition could change
    /// uniqueness, so the resolver defers instead of resolving.
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
    fn read(state: &StoreState) -> EngineResult<Self> {
        let mut index = Self::default();
        for (entity_id, file_id, metadata) in state.entities.list_symbol_projection()? {
            index.owners.insert(entity_id.clone(), file_id.get());
            // Unnamed entities cannot be referenced by name, so they stay out
            // of both views, matching the file graph's name index.
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
                    is_exported: metadata.is_exported,
                    arity,
                    is_declaration: metadata.is_declaration,
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
        for candidates in index.names.values_mut() {
            candidates.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
            // Any definition beats every same-named declaration (C/C++
            // ODR-style preference), so a bodyless declaration never reaches
            // the rules while a definition exists.
            if candidates.iter().any(|candidate| !candidate.is_declaration) {
                candidates.retain(|candidate| !candidate.is_declaration);
            }
        }
        Ok(index)
    }
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
    /// graph. Build it once per sweep; writeback revalidates visibility.
    pub(crate) fn resolution_snapshot(&self) -> EngineResult<ResolutionSnapshot> {
        self.read(|state| {
            let paths = PathIndex::read(state)?;
            let symbols = SymbolIndex::read(state)?;
            let imports = import_scopes(state, &symbols.owners, &paths.indexed)?;
            Ok(ResolutionSnapshot {
                names: symbols.names,
                records: symbols.records,
                paths,
                imports,
            })
        })
    }

    /// Read one bounded page, hiding refs whose source file is not Indexed.
    /// A filtered-out page may be empty while `next_cursor` still advances.
    /// Requires writable storage and the outer workspace write lock.
    pub(crate) fn list_pending_refs(
        &self,
        limit: usize,
        cursor: i64,
    ) -> EngineResult<PendingRefPage> {
        self.write(|state| {
            let mut page = state
                .graph
                .as_ref()
                .expect("writable graph")
                .list_pending_refs(limit, cursor)
                .map_err(graph_error)?;
            let ids = page
                .refs
                .iter()
                .map(|r| r.reference.owner_id.clone())
                .collect::<Vec<_>>();
            let visible = visible_endpoints(state, &ids)?;
            page.refs.retain(|r| {
                visible
                    .get(&r.reference.owner_id)
                    .is_some_and(|file| file.get() == r.file_id)
            });
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
    /// must retain the outer workspace write lock throughout lookup and computation.
    pub(crate) fn apply_resolutions(
        &self,
        resolutions: &[Resolution],
    ) -> EngineResult<ResolutionStats> {
        self.write(|state| {
            let eligible = eligible_ref_ids(
                state,
                &resolutions.iter().map(|r| r.ref_id).collect::<Vec<_>>(),
            )?;
            let endpoints = resolutions
                .iter()
                .map(|r| r.target_id.clone())
                .collect::<Vec<_>>();
            let visible = visible_endpoints(state, &endpoints)?;
            let valid = resolutions
                .iter()
                .filter(|r| eligible.contains(&r.ref_id) && visible.contains_key(&r.target_id))
                .cloned()
                .collect::<Vec<_>>();
            let mut stats = state
                .graph
                .as_mut()
                .expect("writable graph")
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
    pub(crate) fn apply_failures(&self, ref_ids: &[i64]) -> EngineResult<usize> {
        self.write(|state| {
            let eligible = eligible_ref_ids(state, ref_ids)?;
            let valid = ref_ids
                .iter()
                .copied()
                .filter(|id| eligible.contains(id))
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
    pub(crate) fn record_candidates(&self, entries: &[(i64, Vec<String>)]) -> EngineResult<usize> {
        self.write(|state| {
            let ids = entries.iter().map(|(id, _)| *id).collect::<Vec<_>>();
            let eligible = eligible_ref_ids(state, &ids)?;
            let targets = entries
                .iter()
                .flat_map(|(_, candidates)| candidates.iter().cloned())
                .collect::<Vec<_>>();
            let visible = visible_endpoints(state, &targets)?;
            let valid = entries
                .iter()
                .filter(|(id, candidates)| {
                    eligible.contains(id)
                        && !candidates.is_empty()
                        && candidates.iter().all(|id| visible.contains_key(id))
                })
                .cloned()
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
    state
        .graph
        .as_ref()
        .ok_or_else(|| EngineError::invalid_argument("graph storage is not writable"))
}

fn is_indexed(state: &StoreState, file_id: u32) -> EngineResult<bool> {
    let id = FileId::new(file_id);
    let mut files = state.files.fetch(&[id])?;
    Ok(files
        .remove(&id)
        .is_some_and(|file| file.index_status().is_indexed()))
}

/// Pending rows that still exist and whose owning file is Indexed.
fn eligible_ref_ids(state: &StoreState, ids: &[i64]) -> EngineResult<HashSet<i64>> {
    let refs = graph(state)?.get_pending_refs(ids).map_err(graph_error)?;
    let owners = refs
        .iter()
        .map(|r| r.reference.owner_id.clone())
        .collect::<Vec<_>>();
    let visible = visible_endpoints(state, &owners)?;
    Ok(refs
        .iter()
        .filter(|r| {
            visible
                .get(&r.reference.owner_id)
                .is_some_and(|file| file.get() == r.file_id)
        })
        .map(|r| r.id)
        .collect())
}

fn visible_endpoints(state: &StoreState, ids: &[String]) -> EngineResult<HashMap<String, FileId>> {
    let entity_ids = ids
        .iter()
        .cloned()
        .map(EntityId::from_string)
        .collect::<Vec<_>>();
    let mut owners = state
        .entities
        .fetch_file_ids(&entity_ids)?
        .into_iter()
        .map(|(id, file_id)| (id.as_str().to_owned(), file_id))
        .collect::<HashMap<_, _>>();
    for id in ids {
        if let Some(file_id) = id.strip_prefix('f').and_then(|id| id.parse::<u32>().ok())
            && *id == format!("f{file_id}")
        {
            owners.insert(id.clone(), FileId::new(file_id));
        }
    }
    let file_ids = owners
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = state.files.fetch(&file_ids)?;
    owners.retain(|_, file_id| {
        files
            .get(file_id)
            .is_some_and(|file| file.index_status().is_indexed())
    });
    Ok(owners)
}
