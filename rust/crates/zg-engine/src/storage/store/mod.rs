//! Index lifecycle and operations that span multiple tables.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{
        ContentIndex, ContentKind, Entity, EntityId, EntityMetadata, FileId, FileIndexStatus,
        FileRecord, IndexLayout, Language, Range, WorkspaceLayout,
    },
    utils::{
        atomic_write as write_record, directory_exists, ensure_directory, file_exists,
        sync_directory,
    },
};

use super::{
    directories::Directories,
    entities::Entities,
    files::Files,
    fragments::{self, Fragments},
    graph::{self, OpenMode, SqliteGraphStorage},
    types::{
        IndexedFragment, StorageSearchFilter, StorageSearchHit, StoredEntity, StoredFileAttributes,
        StoredSearchData, WorkspaceIndexStorageOptions,
    },
    zvec::{WAL_RECOVERY_REQUIRED, corrupt, initialize, recover_collection},
};
use crate::domain::FileGraphResult;
use crate::domain::model::EmbeddingModelInfo;

mod read_view;
mod resolution;
pub(crate) use read_view::ReadView;
pub(crate) use resolution::ResolutionSnapshot;

type StoreRegistry = Mutex<HashMap<PathBuf, Weak<SharedStore>>>;
static STORES: OnceLock<StoreRegistry> = OnceLock::new();

struct SharedStore {
    state: Mutex<StoreState>,
    schema: Vec<ContentIndex>,
    read_only: bool,
    graph_enabled: bool,
    // The native handles must close before the operating-system lock is released.
    _lock: File,
}

struct StoreState {
    files: Files,
    directories: Directories,
    entities: Entities,
    fragments: Fragments,
    // A failed graph reader must not prevent canonical text/vector reads.
    graph: Result<SqliteGraphStorage, crate::ErrorReport>,
    optimize_marker: PathBuf,
    needs_optimize: bool,
    closed: bool,
}

pub(crate) struct IndexStore {
    shared: Mutex<Option<Arc<SharedStore>>>,
    read_only: bool,
}

pub(crate) struct IndexedSourceFile {
    pub(crate) id: FileId,
    pub(crate) path: PathBuf,
    pub(crate) entities: Vec<IndexedSourceEntity>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct IndexedSourceEntity {
    pub(crate) id: EntityId,
    pub(crate) range: Range,
    pub(crate) source_bytes: Option<(u64, u64)>,
    pub(crate) searchable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRecord {
    path_encoding: PathEncoding,
    indexes: Vec<ContentIndex>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PathEncoding {
    Components,
}

impl SchemaRecord {
    fn new(indexes: &[ContentIndex]) -> Self {
        Self {
            path_encoding: PathEncoding::Components,
            indexes: indexes.to_vec(),
        }
    }

    fn indexes(self) -> EngineResult<Vec<ContentIndex>> {
        validate_indexes(&self.indexes).map_err(|error| {
            EngineError::storage_failure(format!("invalid stored content index: {error}"))
        })?;
        Ok(self.indexes)
    }
}

fn validate_indexes(indexes: &[ContentIndex]) -> EngineResult<()> {
    if indexes.is_empty() {
        return Err(EngineError::invalid_argument(
            "at least one content index is required",
        ));
    }
    let mut kinds = HashSet::new();
    for index in indexes {
        if !kinds.insert(index.kind) {
            return Err(EngineError::invalid_argument(format!(
                "duplicate content index for {}",
                index.kind.as_str()
            )));
        }
        if !index.embedding.model().supports_content(index.kind) {
            return Err(EngineError::invalid_argument(format!(
                "embedding model {} does not support {}",
                index.embedding.model().reference(),
                index.kind.as_str()
            )));
        }
        if !(1..=20_000).contains(&index.embedding.dimension()) {
            return Err(EngineError::invalid_argument(
                "embedding dimension must be in 1..=20,000",
            ));
        }
    }
    Ok(())
}

impl IndexStore {
    /// Whether indexing can persist extracted graphs in this store.
    pub(crate) fn graph_enabled(&self) -> bool {
        self.read(|state| Ok(state.graph.is_ok())).unwrap_or(false)
    }

    pub(crate) fn open(options: WorkspaceIndexStorageOptions) -> EngineResult<Self> {
        Self::open_with(options, false)
    }

    /// Read the published state for consent preflight without creating files or recovering WAL.
    /// A busy index is inspected again by the operation holding workspace admission.
    pub(crate) fn inspect_published<T>(
        layout: &WorkspaceLayout,
        inspect: impl FnOnce(&Self) -> EngineResult<T>,
    ) -> EngineResult<Option<T>> {
        let inspect_state = || {
            let access = crate::workspace::lock::try_lock_existing_workspace_read(layout)?;
            let storage = if access.is_some() {
                if crate::workspace::index::needs_recovery(layout)? {
                    return Ok(None);
                }
                Self::open_with(
                    WorkspaceIndexStorageOptions::ReadOnly {
                        storage_path: layout.index_dir().to_owned(),
                        graph_enabled: false,
                    },
                    true,
                )?
            } else {
                // A daemon writer can already own workspace admission. Borrow only its
                // live store; never race it by opening native collections from disk.
                let path = fs::canonicalize(layout.index_dir()).map_err(|error| {
                    io_error("locate index directory", layout.index_dir(), &error)
                })?;
                let Some(shared) = registry()?
                    .get(&path)
                    .and_then(Weak::upgrade)
                    .filter(|s| !s.read_only)
                else {
                    return Ok(None);
                };
                Self {
                    shared: Mutex::new(Some(shared)),
                    read_only: true,
                }
            };
            inspect(&storage).map(Some)
        };
        match inspect_state() {
            Err(error)
                if matches!(
                    error.code(),
                    EngineError::NOT_FOUND
                        | EngineError::RESOURCE_BUSY
                        | EngineError::RESOURCE_CLOSED
                        | WAL_RECOVERY_REQUIRED
                ) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    fn open_with(options: WorkspaceIndexStorageOptions, inspect_only: bool) -> EngineResult<Self> {
        let graph_enabled = options.graph_enabled();
        if let WorkspaceIndexStorageOptions::ReadWrite { indexes, .. } = &options {
            validate_indexes(indexes)?;
        }
        initialize()?;
        let home = options.storage_path();
        if !home.is_absolute() {
            return Err(EngineError::invalid_argument(
                "storage path must be absolute",
            ));
        }
        let read_only = options.is_read_only();
        let outer = home
            .file_name()
            .and_then(|_| home.parent())
            .map(Path::to_path_buf);
        if read_only {
            directory_exists(home)?;
        } else {
            ensure_directory(home)?;
        }
        let path = fs::canonicalize(home)
            .map_err(|error| io_error("locate index directory", home, &error))?;
        let mut registry = registry()?;
        if let Some(shared) = registry.get(&path).and_then(Weak::upgrade) {
            if inspect_only
                || (read_only && shared.read_only && graph_enabled == shared.graph_enabled)
            {
                return Ok(Self {
                    shared: Mutex::new(Some(shared)),
                    read_only,
                });
            }
            return Err(EngineError::resource_busy(
                "workspace storage is already open",
            ));
        }
        let layout = IndexLayout::new(&path);
        let (mut lock, mut schema, new_storage) =
            prepare_storage(layout, options.clone(), !inspect_only)?;
        let optimize_marker = layout.optimize_marker_path();
        let needs_optimize = file_exists(&optimize_marker)?;
        let graph = if !graph_enabled {
            Err(graph_disabled())
        } else if read_only {
            // Graph failures do not prevent canonical text/vector reads.
            open_graph(layout, OpenMode::ReadOnly)
        } else if new_storage {
            let graph = open_graph(layout, OpenMode::Create)?;
            sync_directory(&layout.graph_dir())?;
            Ok(graph)
        } else {
            // Unchanged files skip extraction and cannot restore a missing graph.
            Ok(open_graph(layout, OpenMode::ReadWrite)?)
        }
        .map_err(EngineError::into_report);
        let tables = open_tables(layout, &schema, read_only);
        let (files, directories, entities, fragments) = match tables {
            Err(error) if read_only && !inspect_only && error.code() == WAL_RECOVERY_REQUIRED => {
                // open_tables has dropped every partial native reader.
                (lock, schema) = recover_wal(layout, options, lock)?;
                open_tables(layout, &schema, true)?
            }
            result => result?,
        };
        if !read_only {
            directories.load()?;
            // Repeat the same range after failures that leave directories in place.
            sync_directory(&path)?;
            if let Some(outer) = outer {
                sync_directory(&outer)?;
            }
        }
        let shared = Arc::new(SharedStore {
            state: Mutex::new(StoreState {
                files,
                directories,
                entities,
                fragments,
                graph,
                optimize_marker,
                needs_optimize,
                closed: false,
            }),
            schema,
            read_only,
            graph_enabled,
            _lock: lock,
        });
        // An inspection holds workspace read admission itself. Leaving this short-lived,
        // graph-independent reader out of the registry lets normal readers open concurrently.
        if !inspect_only {
            registry.insert(path, Arc::downgrade(&shared));
        }
        Ok(Self {
            shared: Mutex::new(Some(shared)),
            read_only,
        })
    }

    pub(crate) fn delete(storage_path: &Path) -> EngineResult<()> {
        if !directory_exists(storage_path)? {
            return Ok(());
        }
        let path = fs::canonicalize(storage_path)
            .map_err(|error| io_error("locate storage", storage_path, &error))?;
        let mut registry = registry()?;
        if registry.get(&path).and_then(Weak::upgrade).is_some() {
            return Err(EngineError::resource_busy(
                "cannot delete open workspace storage",
            ));
        }
        // The caller's workspace write lock prevents new opens while this inner lock is closed.
        drop(acquire_storage_lock(IndexLayout::new(&path), false, true)?);
        fs::remove_dir_all(&path)
            .map_err(|error| io_error("delete workspace storage", &path, &error))?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        registry.remove(&path);
        Ok(())
    }

    fn shared(&self) -> EngineResult<Arc<SharedStore>> {
        self.shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| EngineError::resource_closed("workspace storage is closed"))
    }

    pub(crate) fn validate_vector_targets(&self, targets: &[ContentIndex]) -> EngineResult<()> {
        ensure_targets_compatible(&self.shared()?.schema, targets)
    }

    fn read<T>(&self, operation: impl FnOnce(&StoreState) -> EngineResult<T>) -> EngineResult<T> {
        let shared = self.shared()?;
        let state = lock_state(&shared)?;
        assert_usable(&state)?;
        operation(&state)
    }

    fn write<T>(
        &self,
        operation: impl FnOnce(&mut StoreState) -> EngineResult<T>,
    ) -> EngineResult<T> {
        if self.read_only {
            return Err(EngineError::permission_denied(
                "cannot write read-only workspace storage",
            ));
        }
        let shared = self.shared()?;
        let mut state = lock_state(&shared)?;
        assert_usable(&state)?;
        operation(&mut state)
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub(crate) fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        self.read(|state| state.files.list())
    }

    pub(crate) fn populated_kinds(
        &self,
        excluded_files: &[FileId],
    ) -> EngineResult<BTreeSet<ContentKind>> {
        self.read(|state| state.fragments.populated_kinds(excluded_files))
    }

    pub(crate) fn count_fragments(&self) -> EngineResult<u64> {
        self.read(|state| state.fragments.count())
    }

    pub(crate) fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        self.read(|state| state.files.list_paths())
    }

    pub(crate) fn indexed_sources(
        &self,
        paths: &HashSet<PathBuf>,
    ) -> EngineResult<Vec<IndexedSourceFile>> {
        self.read(|state| {
            let mut watched = Vec::new();
            for (id, path) in state.files.lookup_paths(paths)? {
                let ids = state.entities.list_ids_for_file(id)?;
                let ids = ids
                    .into_iter()
                    .map(EntityId::from_string)
                    .collect::<Vec<_>>();
                let entities = state.entities.fetch(&ids)?;
                if entities.len() != ids.len() {
                    return Err(corrupt("diagnostic entity lookup was incomplete"));
                }
                let keys = entities
                    .values()
                    .flat_map(Entity::fragments)
                    .map(|fragment| fragment.id.as_str())
                    .collect::<Vec<_>>();
                let projected = state.fragments.projected_fragments(&keys)?;
                let entities = entities
                    .into_values()
                    .map(|entity| {
                        let searchable = entity
                            .fragments()
                            .iter()
                            .any(|fragment| projected.contains(fragment.id.as_str()));
                        IndexedSourceEntity {
                            id: entity.id().clone(),
                            range: entity.source_range(),
                            source_bytes: entity.source_byte_offsets(),
                            searchable,
                        }
                    })
                    .collect();
                watched.push(IndexedSourceFile { id, path, entities });
            }
            Ok(watched)
        })
    }

    pub(crate) fn list_file_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>> {
        self.read(|state| state.files.list_attributes())
    }

    pub(crate) fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        self.write(|state| state.files.resolve_ids(paths))
    }

    pub(crate) fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        self.read(|state| state.files.has_non_unicode_file_names())
    }

    pub(crate) fn load_search_hits(
        &self,
        hits: &[StorageSearchHit],
    ) -> EngineResult<StoredSearchData> {
        self.read(|state| load_search_hits(state, hits))
    }

    pub(crate) fn search_fts(
        &self,
        kind: ContentKind,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.read_snapshot(|view| view.search_fts(kind, query, limit, filter))
    }

    pub(crate) fn search_vector(
        &self,
        kind: ContentKind,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let shared = self.shared()?;
        validate_vector(vector, index_embedding(&shared.schema, kind)?)?;
        self.read(|state| {
            if limit == 0 || fragments::empty_filter(filter) {
                return Ok(Vec::new());
            }
            let filter = fragments::build_filter(filter, &|path| state.directories.get(path))?;
            state
                .fragments
                .search_vector(kind, vector, limit, filter.as_deref())
        })
    }

    #[cfg(test)]
    pub(crate) fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        self.replace_file_with_graph(file, entities, entries, &FileGraphResult::default())
    }

    /// Replace canonical content and relationships under the same store lock.
    pub(crate) fn replace_file_with_graph(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        graph: &FileGraphResult,
    ) -> EngineResult<()> {
        let shared = self.shared()?;
        validate_batch(file, entities, entries, &shared.schema)?;
        SqliteGraphStorage::validate_snapshot(file.id().get(), graph).map_err(graph_error)?;
        let ids: HashSet<_> = entities.iter().map(|entity| entity.id().as_str()).collect();
        if graph
            .nodes
            .iter()
            .any(|node| !ids.contains(node.id.as_str()))
        {
            return Err(EngineError::invalid_argument(
                "graph entity is absent from canonical file entities",
            ));
        }
        let mut file = file.clone();
        file.set_index_status(FileIndexStatus::Indexed {
            indexed_epoch_ms: now_epoch_ms()?,
            entity_count: u64::try_from(entities.len())
                .map_err(|_| EngineError::invalid_argument("entity count exceeds u64"))?,
        })?;
        self.write(|state| replace_file(state, &file, entities, entries, graph))
    }

    pub(crate) fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
        let mut file = file.clone();
        file.set_index_status(FileIndexStatus::Failed {
            error: error.to_owned(),
        })?;
        self.write(|state| replace_file(state, &file, &[], &[], &FileGraphResult::default()))
    }

    pub(crate) fn update_file_snapshot(&self, file: &FileRecord) -> EngineResult<()> {
        self.write(|state| {
            let existing = state.files.fetch(&[file.id()])?.remove(&file.id());
            let valid = existing.as_ref().is_some_and(|existing| {
                existing.index_status().is_indexed()
                    && existing.index_status() == file.index_status()
                    && existing.relative_path() == file.relative_path()
                    && existing.snapshot().size_bytes == file.snapshot().size_bytes
                    && existing.snapshot().content_hash == file.snapshot().content_hash
            });
            if !valid {
                return Err(EngineError::invalid_argument(
                    "snapshot refresh requires the same indexed file, content hash and size",
                ));
            }
            let directories = state.directories.ensure(file.relative_path())?;
            state.files.put(file, &directories)
        })
    }

    pub(crate) fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        self.write(|state| {
            state.needs_optimize = true;
            let old_ids = invalidation_entity_ids(state, file_id)?;
            state.files.mark_deleting(file_id)?;
            if let Ok(graph) = state.graph.as_mut() {
                graph
                    .delete_file_graph(file_id.get(), &old_ids)
                    .map_err(graph_error)?;
            }
            state.fragments.delete_file(file_id)?;
            state.entities.delete_file(file_id)?;
            state.files.delete(file_id)
        })
    }

    /// Requeues references whose evidence changed during this pass.
    ///
    /// The resolution sweep only reads `pending` rows, so a resolved reference
    /// never sees a changed candidate set or module path again unless a pass
    /// explicitly requeues it. Endpoint invalidation covers references that
    /// pointed at a replaced file; this covers the workspace-wide sets that
    /// endpoint identity cannot express. The evidence lives in the graph
    /// database, recorded with the file writes that produced it, so a pass that
    /// fails or is cancelled before this call requeues on the next one. A pass
    /// without graph storage has nothing to requeue.
    ///
    /// # Errors
    /// Rejects SQLite failures. The caller degrades like a failed sweep: the
    /// canonical index stays published, the stale resolutions survive, and the
    /// evidence stays recorded.
    pub(crate) fn requeue_stale_resolutions(&self) -> EngineResult<usize> {
        self.write(|state| {
            let Ok(graph) = state.graph.as_mut() else {
                return Ok(0);
            };
            graph.requeue_stale_resolutions().map_err(graph_error)
        })
    }

    /// Build indexes and compact accepted writes when needed.
    pub(crate) fn finalize_writes(&self) -> EngineResult<()> {
        self.finalize_writes_with(|state| {
            optimize_collections([
                &|| state.directories.optimize(),
                &|| state.entities.optimize(),
                &|| state.files.optimize(),
                &|| state.fragments.optimize(),
            ])
        })
    }

    fn finalize_writes_with(
        &self,
        optimize: impl FnOnce(&mut StoreState) -> EngineResult<()>,
    ) -> EngineResult<()> {
        if self.read_only {
            return Ok(());
        }
        self.write(|state| {
            // Also finish indexes created by versions that only flushed writes.
            if state.needs_optimize || state.fragments.needs_optimize()? {
                state.needs_optimize = true;
                // Fragments can finish before another collection fails. Persist retry
                // intent before starting any worker so reopening retains that work.
                write_record(&state.optimize_marker, b"")?;
                optimize(state)?;
                fs::remove_file(&state.optimize_marker).map_err(|error| {
                    io_error("clear pending optimization", &state.optimize_marker, &error)
                })?;
                // A crash before this removal persists can only cause a safe extra retry.
                state.needs_optimize = false;
            }
            Ok(())
        })
    }

    pub(crate) fn close(&self) -> EngineResult<()> {
        // Serialize concurrent closes through the final flush as well as
        // taking the lease, so none can return while its writes are persisting.
        let mut lease = self
            .shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?;
        let Some(shared) = lease.take() else {
            return Ok(());
        };
        if self.read_only {
            return Ok(());
        }
        let mut state = lock_state(&shared)?;
        // Reject a write that acquired its Arc before close but is still waiting
        // for this lock. A successful close commits every accepted write.
        state.closed = true;
        let flush_result = flush(&state);
        let graph_close = std::mem::replace(
            &mut state.graph,
            Err(EngineError::resource_closed("workspace graph is closed").into_report()),
        )
        .map_or(Ok(()), |graph| graph.close().map_err(graph_error));
        flush_result.and(graph_close)
    }
}

fn open_tables(
    layout: IndexLayout<'_>,
    schema: &[ContentIndex],
    read_only: bool,
) -> EngineResult<(Files, Directories, Entities, Fragments)> {
    Ok((
        Files::open(layout, read_only)?,
        Directories::open(layout, read_only)?,
        Entities::open(layout, read_only)?,
        Fragments::open(layout, schema, read_only)?,
    ))
}

fn recover_wal(
    layout: IndexLayout<'_>,
    options: WorkspaceIndexStorageOptions,
    shared_lock: File,
) -> EngineResult<(File, Vec<ContentIndex>)> {
    // Keep the caller's workspace read lock to prevent publication, but release
    // shared storage access before taking the exclusive recovery lock.
    drop(shared_lock);
    let recovery_lock = acquire_storage_lock(layout, false, true)?;
    let (schema, _) = load_schema(layout, options.clone())?;
    for path in collection_paths(layout, &schema) {
        recover_collection(&path)?;
    }
    drop(recovery_lock);
    // Do not retain an exclusive lock in a long-lived cached read session.
    let (lock, schema, _) = prepare_storage(layout, options, true)?;
    Ok((lock, schema))
}

fn optimize_collections(
    operations: [&(dyn Fn() -> EngineResult<()> + Sync); 4],
) -> EngineResult<()> {
    std::thread::scope(|scope| {
        let workers = operations.map(|optimize| scope.spawn(optimize));
        let mut result = Ok(());
        // Keep the store locked until every collection settles, even after an error.
        for worker in workers {
            let next = worker.join().unwrap_or_else(|_| {
                Err(EngineError::internal("collection optimize worker panicked"))
            });
            result = result.and(next);
        }
        result
    })
}

// Cross-table writes stay under the store's single writer lock.
fn replace_file(
    state: &mut StoreState,
    file: &FileRecord,
    entities: &[Entity],
    entries: &[IndexedFragment],
    graph: &FileGraphResult,
) -> EngineResult<()> {
    if state.graph.is_err()
        && (!graph.nodes.is_empty() || !graph.edges.is_empty() || !graph.pending_refs.is_empty())
    {
        return Err(graph_disabled());
    }
    state.files.validate(file)?;
    state.entities.validate_ownership(entities, file.id())?;
    state.fragments.validate_ownership(entries, file.id())?;
    // Endpoint invalidation below only revisits references that pointed at this
    // file's old entities. A file that enters or leaves the indexed set, or one
    // that introduces a name another entity already uses, changes the
    // workspace-wide sets resolution reads without touching those endpoints, so
    // the graph records that evidence with the rows it can stale and the pass
    // requeues them once before its sweep.
    //
    // Membership is what an import specifier reads, not record existence: the
    // snapshot's path lookup skips `Failed` and `NotIndexed` records, so a path
    // that failed one pass and succeeded the next enters the set while its
    // record already exists — and one that breaks leaves it the same way.
    let previous_indexed = state
        .files
        .fetch(&[file.id()])?
        .remove(&file.id())
        .is_some_and(|previous| previous.index_status().is_indexed());
    let paths_changed = previous_indexed != file.index_status().is_indexed();
    state.needs_optimize = true;
    let directories = state.directories.ensure(file.relative_path())?;
    // Encode every projection before removing any of the previous file's data.
    let entity_docs = Entities::prepare(entities)?;
    let fragment_docs = Fragments::prepare(file, entities, entries, &directories)?;
    let old_ids = invalidation_entity_ids(state, file.id())?;
    let mut unfinished = file.clone();
    unfinished.set_index_status(FileIndexStatus::NotIndexed)?;
    state.files.put(&unfinished, &directories)?;
    // The language labels this file's pending references for the resolver's
    // language gate; the path itself stays canonical in zvec.
    let relative_path = file
        .relative_path()
        .as_path()
        .to_string_lossy()
        .replace('\\', "/");
    let formats = crate::domain::formats_for_path(std::path::Path::new(&relative_path));
    // Shared extensions such as .h must use the grammar that produced the
    // entities, rather than the first extension candidate (which can be C).
    let language = entities
        .iter()
        .find_map(|entity| match entity.metadata() {
            Some(EntityMetadata::Code(metadata)) => metadata.language.map(Language::as_str),
            _ => None,
        })
        .or_else(|| formats.first().map(|format| format.as_str()))
        .unwrap_or("unknown");
    // zvec writes are durable on success: record retry intent before advancing SQLite.
    if let Ok(storage) = state.graph.as_mut() {
        storage
            .write_file_graph(
                file.id().get(),
                Some(language),
                graph,
                &old_ids,
                paths_changed,
            )
            .map_err(graph_error)?;
    }
    state.fragments.delete_file(file.id())?;
    state.entities.delete_file(file.id())?;
    state.entities.write(&entity_docs)?;
    state.fragments.write(&fragment_docs)?;
    // Indexed is the publication marker: all content must already be durable.
    state.files.put(file, &directories)
}

fn invalidation_entity_ids(state: &StoreState, file_id: FileId) -> EngineResult<Vec<String>> {
    let Ok(graph) = state.graph.as_ref() else {
        return Ok(Vec::new());
    };
    // An empty graph has no incoming relationships to invalidate. Avoid a
    // canonical entity scan on every file when the graph is empty or absent.
    if graph.is_empty().map_err(graph_error)? {
        return Ok(Vec::new());
    }
    state.entities.list_ids_for_file(file_id)
}

fn graph_disabled() -> EngineError {
    EngineError::unsupported("graph support is disabled")
        .with_help("Rebuild the index with `zg --index --rebuild --graph`.")
}

fn graph_missing() -> EngineError {
    EngineError::not_found(
        "workspace graph is missing; this index has no persisted relationship data",
    )
}

fn open_graph(layout: IndexLayout<'_>, mode: OpenMode) -> EngineResult<SqliteGraphStorage> {
    let path = layout.graph_db_path();
    let graph = if mode == OpenMode::Create {
        SqliteGraphStorage::open(&path, mode).map_err(graph_error)
    } else {
        match fs::metadata(&path) {
            Ok(_) => SqliteGraphStorage::open(&path, mode).map_err(graph_error),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(graph_missing()),
            Err(error) => Err(io_error("inspect workspace graph", &path, &error)),
        }
    };
    graph.map_err(|error| error.with_help("Rebuild the index with `zg --index --rebuild --graph`."))
}

fn graph_error(error: graph::Error) -> EngineError {
    match error {
        graph::Error::InvalidInput(message) => EngineError::invalid_argument(message),
        error => EngineError::storage_failure(format!("workspace graph operation failed: {error}")),
    }
}

fn load_search_hits(
    state: &StoreState,
    hits: &[StorageSearchHit],
) -> EngineResult<StoredSearchData> {
    let entity_ids = hits
        .iter()
        .map(|hit| hit.entity_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let file_ids = hits
        .iter()
        .map(|hit| hit.file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let stored_entities = state.entities.fetch(&entity_ids)?;
    let files = state.files.fetch(&file_ids)?;
    let mut entities = HashMap::new();
    let mut fragments = HashMap::new();
    for entity in stored_entities.into_values() {
        // A replacement or deletion can stop between collection writes.
        let Some(file) = files.get(&entity.file_id()) else {
            continue;
        };
        for fragment in entity.fragments() {
            if fragments
                .insert(fragment.id.as_str().to_owned(), fragment.clone())
                .is_some()
            {
                return Err(corrupt("duplicate canonical fragment identity"));
            }
        }
        entities.insert(
            entity.id().clone(),
            StoredEntity {
                entity,
                file: file.clone(),
            },
        );
    }
    for hit in hits {
        if let Some(owner) = entities.get(&hit.entity_id)
            && owner.file.id() != hit.file_id
        {
            return Err(corrupt("search identities differ from their stored entity"));
        }
    }
    Ok(StoredSearchData {
        entities,
        fragments,
    })
}

fn flush(state: &StoreState) -> EngineResult<()> {
    state.directories.flush()?;
    state.entities.flush()?;
    state.fragments.flush()?;
    state.files.flush()
}

fn assert_usable(state: &StoreState) -> EngineResult<()> {
    if state.closed {
        return Err(EngineError::resource_closed("workspace storage is closed"));
    }
    Ok(())
}

fn validate_batch(
    file: &FileRecord,
    entities: &[Entity],
    entries: &[IndexedFragment],
    schema: &[ContentIndex],
) -> EngineResult<()> {
    let mut entity_ids = HashSet::new();
    let mut fragment_ids = HashSet::new();
    for entity in entities {
        if entity.file_id() != file.id() {
            return Err(EngineError::invalid_argument(
                "entity belongs to a different file",
            ));
        }
        if !entity_ids.insert(entity.id()) {
            return Err(EngineError::invalid_argument("duplicate entity id"));
        }
        for fragment in entity.fragments() {
            if !fragment_ids.insert(&fragment.id) {
                return Err(EngineError::invalid_argument("duplicate fragment id"));
            }
        }
    }
    fragments::validate_projections(entities, entries)?;
    for entry in entries {
        validate_vector(&entry.vector, index_embedding(schema, entry.kind)?)?;
    }
    Ok(())
}

fn index_embedding(
    schema: &[ContentIndex],
    kind: ContentKind,
) -> EngineResult<&EmbeddingModelInfo> {
    schema
        .iter()
        .find(|index| index.kind == kind)
        .map(|index| &index.embedding)
        .ok_or_else(|| {
            EngineError::invalid_argument(format!("missing {} fragment collection", kind.as_str()))
        })
}

fn validate_vector(vector: &[f32], schema: &EmbeddingModelInfo) -> EngineResult<()> {
    if vector.len() != schema.dimension() || vector.iter().any(|value| !value.is_finite()) {
        return Err(EngineError::invalid_argument(format!(
            "expected a finite {}-dimensional vector, got {} values",
            schema.dimension(),
            vector.len()
        )));
    }
    Ok(())
}

fn ensure_targets_compatible(
    stored: &[ContentIndex],
    targets: &[ContentIndex],
) -> EngineResult<()> {
    for index in targets {
        index_embedding(stored, index.kind)
            .and_then(|embedding| embedding.ensure_index_compatible(&index.embedding))
            .map_err(|error| error.with_help("Rebuild the index with `zg --index --rebuild`."))?;
    }
    Ok(())
}

/// Returns the content-index schema and whether this call initialized an empty store.
fn load_schema(
    layout: IndexLayout<'_>,
    options: WorkspaceIndexStorageOptions,
) -> EngineResult<(Vec<ContentIndex>, bool)> {
    let descriptor = layout.schema_path();
    if file_exists(&descriptor)? {
        let schema = read_json::<SchemaRecord>(&descriptor)
            .map_err(|error| {
                EngineError::storage_failure(format!(
                    "unsupported or invalid storage schema: {error}; rebuild the index"
                ))
            })?
            .indexes()?;
        if let WorkspaceIndexStorageOptions::ReadWrite { indexes, .. } = &options {
            if schema.len() != indexes.len() {
                return Err(EngineError::invalid_argument(
                    "content index kinds changed; rebuild the index",
                ));
            }
            ensure_targets_compatible(&schema, indexes)?;
        }
        for collection_path in collection_paths(layout, &schema) {
            if !directory_exists(&collection_path)? {
                let collection = collection_path
                    .strip_prefix(layout.root())
                    .unwrap_or(&collection_path)
                    .display();
                return Err(EngineError::storage_failure(format!(
                    "index collection {collection} is missing; rebuild the index"
                )));
            }
        }
        return Ok((schema, false));
    }
    let path = layout.root();
    let lock_path = layout.lock_path();
    if fs::read_dir(path)
        .map_err(|error| io_error("inspect storage directory", path, &error))?
        .find(|entry| {
            entry
                .as_ref()
                .map_or(true, |entry| entry.path() != lock_path)
        })
        .transpose()
        .map_err(|error| io_error("inspect storage entry", path, &error))?
        .is_some()
    {
        return Err(EngineError::storage_failure(
            "existing workspace storage is missing its schema; rebuild the index",
        ));
    }
    let WorkspaceIndexStorageOptions::ReadWrite { indexes, .. } = options else {
        return Err(EngineError::not_found(
            "workspace storage schema does not exist",
        ));
    };
    write_record(
        &descriptor,
        &serde_json::to_vec(&SchemaRecord::new(&indexes)).map_err(|error| json_error(&error))?,
    )?;
    Ok((indexes, true))
}

fn collection_paths(
    layout: IndexLayout<'_>,
    schema: &[ContentIndex],
) -> impl Iterator<Item = PathBuf> {
    [
        layout.files_dir(),
        layout.directories_dir(),
        layout.entities_dir(),
    ]
    .into_iter()
    .chain(
        schema
            .iter()
            .map(move |index| layout.content_dir(index.kind)),
    )
}

fn registry() -> EngineResult<MutexGuard<'static, HashMap<PathBuf, Weak<SharedStore>>>> {
    let mut stores = STORES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| EngineError::internal("storage registry lock was poisoned"))?;
    stores.retain(|_, store| store.strong_count() != 0);
    Ok(stores)
}

fn lock_state(shared: &SharedStore) -> EngineResult<MutexGuard<'_, StoreState>> {
    shared
        .state
        .lock()
        .map_err(|_| EngineError::internal("workspace storage lock was poisoned"))
}

fn prepare_storage(
    layout: IndexLayout<'_>,
    options: WorkspaceIndexStorageOptions,
    create_lock: bool,
) -> EngineResult<(File, Vec<ContentIndex>, bool)> {
    let lock = acquire_storage_lock(layout, options.is_read_only(), create_lock)?;
    let (schema, new_storage) = load_schema(layout, options)?;
    Ok((lock, schema, new_storage))
}

fn acquire_storage_lock(layout: IndexLayout<'_>, shared: bool, create: bool) -> EngineResult<File> {
    let path = layout.lock_path();
    file_exists(&path)?;
    let file = OpenOptions::new()
        .create(create)
        .truncate(false)
        .read(true)
        .write(create)
        .open(&path)
        .map_err(|error| io_error("open storage lock", &path, &error))?;
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    result.map_err(|error| storage_lock_error(layout.root(), error))?;
    Ok(file)
}

fn storage_lock_error(home: &Path, error: std::fs::TryLockError) -> EngineError {
    match error {
        std::fs::TryLockError::WouldBlock => EngineError::resource_busy(format!(
            "workspace storage is locked by another reader or writer: {}",
            home.display()
        )),
        std::fs::TryLockError::Error(error) => io_error("lock workspace storage", home, &error),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> EngineResult<T> {
    let bytes = fs::read(path).map_err(|error| io_error("read storage record", path, &error))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::storage_failure(format!(
            "invalid storage record {}: {error}",
            path.display()
        ))
    })
}

pub(super) fn io_error(action: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!("cannot {action} {}: {error}", path.display()),
        error,
    )
}

fn json_error(error: &serde_json::Error) -> EngineError {
    EngineError::storage_failure(format!("cannot encode storage record: {error}"))
}

fn now_epoch_ms() -> EngineResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(|| EngineError::internal("system clock cannot represent the index timestamp"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tables_tests;

#[cfg(test)]
mod graph_tests;

#[cfg(test)]
impl IndexStore {
    pub(crate) fn set_test_file_status(
        &self,
        id: FileId,
        status: FileIndexStatus,
    ) -> EngineResult<()> {
        self.write(|state| {
            let mut file = state.files.fetch(&[id])?.remove(&id).expect("fixture file");
            file.set_index_status(status)?;
            let directories = state.directories.ensure(file.relative_path())?;
            state.files.put(&file, &directories)
        })
    }
}
