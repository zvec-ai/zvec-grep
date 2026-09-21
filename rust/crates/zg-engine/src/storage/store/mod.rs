//! Index lifecycle and operations that span multiple tables.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{Entity, FileId, FileIndexStatus, FileRecord, model::Metric},
    utils::{atomic_write as write_record, sync_directory},
};

use super::{
    directories::Directories,
    entities::{self, Entities},
    files::Files,
    fragments::{self, Fragments},
    types::{
        IndexedFragment, StorageSearchFilter, StorageSearchHit, StoredEntity, StoredFileAttributes,
        StoredSearchData, WorkspaceIndexStorageOptions,
    },
    zvec::{corrupt, initialize},
};
use crate::domain::model::EmbeddingModelInfo;

type StoreRegistry = Mutex<HashMap<PathBuf, Weak<SharedStore>>>;
static STORES: OnceLock<StoreRegistry> = OnceLock::new();

struct SharedStore {
    state: Mutex<StoreState>,
    schema: Vec<EmbeddingModelInfo>,
    read_only: bool,
    // The native handles must close before the operating-system lock is released.
    _lock: File,
}

struct StoreState {
    files: Files,
    directories: Directories,
    entities: Entities,
    fragments: Fragments,
    closed: bool,
}

pub(crate) struct IndexStore {
    shared: Mutex<Option<Arc<SharedStore>>>,
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRecord {
    embeddings: Vec<EmbeddingModelInfo>,
}

impl SchemaRecord {
    fn new(embeddings: &[EmbeddingModelInfo]) -> Self {
        Self {
            embeddings: embeddings.to_vec(),
        }
    }

    fn embeddings(self) -> EngineResult<Vec<EmbeddingModelInfo>> {
        validate_models(&self.embeddings).map_err(|error| {
            EngineError::storage_failure(format!(
                "invalid stored embedding model information: {error}"
            ))
        })?;
        Ok(self.embeddings)
    }
}

fn validate_models(embeddings: &[EmbeddingModelInfo]) -> EngineResult<()> {
    if embeddings.is_empty() {
        return Err(EngineError::invalid_argument(
            "at least one embedding model is required",
        ));
    }
    let mut models = std::collections::HashSet::new();
    for embedding in embeddings {
        embedding.validate()?;
        if !(1..=20_000).contains(&embedding.dimension) {
            return Err(EngineError::invalid_argument(
                "embedding dimension must be in 1..=20,000",
            ));
        }
        if !models.insert(embedding.model.reference()) {
            return Err(EngineError::invalid_argument(
                "embedding model references must be unique",
            ));
        }
    }
    Ok(())
}

impl IndexStore {
    pub(crate) fn open(options: WorkspaceIndexStorageOptions) -> EngineResult<Self> {
        if let WorkspaceIndexStorageOptions::ReadWrite { embeddings, .. } = &options {
            validate_models(embeddings)?;
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
        if !read_only {
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(error) = builder.create(home)
                && !(error.kind() == std::io::ErrorKind::AlreadyExists && home.is_dir())
            {
                return Err(io_error("create index directory", home, &error));
            }
        }
        let home = fs::canonicalize(home)
            .map_err(|error| io_error("locate index directory", home, &error))?;
        let path = home.join("storage");
        let mut registry = registry()?;
        if let Some(shared) = registry.get(&path).and_then(Weak::upgrade) {
            if read_only && shared.read_only {
                return Ok(Self {
                    shared: Mutex::new(Some(shared)),
                    read_only,
                });
            }
            return Err(EngineError::resource_busy(
                "workspace storage is already open",
            ));
        }
        let (lock, schema) = prepare_storage(&home, &path, options)?;
        let files = Files::open(&path, read_only)?;
        let directories = Directories::open(&path, read_only)?;
        let entities = Entities::open(&path, read_only)?;
        let fragments = Fragments::open(&path, &schema, read_only)?;
        if !read_only {
            directories.load()?;
        }
        if !read_only {
            // Repeat the same range after failures that leave directories in place.
            sync_directory(&path)?;
            sync_directory(&home)?;
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
                closed: false,
            }),
            schema,
            read_only,
            _lock: lock,
        });
        registry.insert(path, Arc::downgrade(&shared));
        Ok(Self {
            shared: Mutex::new(Some(shared)),
            read_only,
        })
    }

    pub(crate) fn exists(storage_path: &Path) -> EngineResult<bool> {
        match fs::metadata(storage_path.join("storage")) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect storage", storage_path, &error)),
        }
    }

    pub(crate) fn delete(storage_path: &Path) -> EngineResult<()> {
        if !Self::exists(storage_path)? {
            return Ok(());
        }
        let home = fs::canonicalize(storage_path)
            .map_err(|error| io_error("locate storage", storage_path, &error))?;
        let path = home.join("storage");
        let mut registry = registry()?;
        if registry.get(&path).and_then(Weak::upgrade).is_some() {
            return Err(EngineError::resource_busy(
                "cannot delete open workspace storage",
            ));
        }
        let _lock = acquire_storage_lock(&home, false)?;
        fs::remove_dir_all(&path)
            .map_err(|error| io_error("delete workspace storage", &path, &error))?;
        sync_directory(&home)?;
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

    pub(crate) fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        self.read(|state| state.files.list_paths())
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
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.read(|state| {
            if limit == 0 || fragments::empty_filter(filter) {
                return Ok(Vec::new());
            }
            let filter = fragments::build_filter(filter, &|path| state.directories.get(path))?;
            state.fragments.search_fts(query, limit, filter.as_deref())
        })
    }

    pub(crate) fn search_vector(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let shared = self.shared()?;
        validate_vector(vector, model_schema(&shared.schema, model)?)?;
        self.read(|state| {
            if limit == 0 || fragments::empty_filter(filter) {
                return Ok(Vec::new());
            }
            let filter = fragments::build_filter(filter, &|path| state.directories.get(path))?;
            state
                .fragments
                .search_vector(model, vector, limit, filter.as_deref())
        })
    }

    pub(crate) fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        let shared = self.shared()?;
        validate_batch(file, entities, entries, &shared.schema)?;
        let mut file = file.clone();
        file.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: now_epoch_ms()?,
            entity_count: u64::try_from(entities.len())
                .map_err(|_| EngineError::invalid_argument("entity count exceeds u64"))?,
        };
        file.validate()?;
        self.write(|state| replace_file(state, &file, entities, entries))
    }

    pub(crate) fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
        file.validate()?;
        let mut file = file.clone();
        file.index_status = FileIndexStatus::Failed {
            error: error.to_owned(),
        };
        file.validate()?;
        self.write(|state| replace_file(state, &file, &[], &[]))
    }

    pub(crate) fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        self.write(|state| {
            state.files.mark_deleting(file_id)?;
            state.fragments.delete_file(file_id)?;
            state.entities.delete_file(file_id)?;
            state.files.delete(file_id)
        })
    }

    /// Flush accepted writes, including any unfinished file's retry state.
    pub(crate) fn checkpoint(&self) -> EngineResult<()> {
        if self.read_only {
            return Ok(());
        }
        self.write(|state| flush(state))
    }

    pub(crate) fn close(&self) -> EngineResult<()> {
        // Serialize concurrent closes through the final checkpoint as well as
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
        flush(&state)
    }
}

// Cross-table writes stay under the store's single writer lock.
fn replace_file(
    state: &mut StoreState,
    file: &FileRecord,
    entities: &[Entity],
    entries: &[IndexedFragment],
) -> EngineResult<()> {
    state.files.validate(file)?;
    state.entities.validate_ownership(entities, file.id)?;
    state.fragments.validate_ownership(entries, file.id)?;
    let directories = state.directories.ensure(&file.relative_path)?;
    // Encode every projection before removing any of the previous file's data.
    let entity_docs = Entities::prepare(entities)?;
    let fragment_docs = Fragments::prepare(file, entities, entries, &directories)?;
    let mut unfinished = file.clone();
    unfinished.index_status = FileIndexStatus::NotIndexed;
    state.files.put(&unfinished, &directories)?;
    state.fragments.delete_file(file.id)?;
    state.entities.delete_file(file.id)?;
    state.entities.write(&entity_docs)?;
    state.fragments.write(&fragment_docs)?;
    state.files.put(file, &directories)
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
        let Some(file) = files.get(&entity.file_id) else {
            continue;
        };
        for fragment in &entity.fragments {
            if fragments
                .insert(fragment.id.as_str().to_owned(), fragment.clone())
                .is_some()
            {
                return Err(corrupt("duplicate canonical fragment identity"));
            }
        }
        entities.insert(
            entity.id.clone(),
            StoredEntity {
                entity,
                file: file.clone(),
            },
        );
    }
    for hit in hits {
        if let Some(owner) = entities.get(&hit.entity_id)
            && owner.file.id != hit.file_id
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
    schema: &[EmbeddingModelInfo],
) -> EngineResult<()> {
    file.validate()?;
    let mut entity_ids = HashSet::new();
    let mut fragment_ids = HashSet::new();
    for entity in entities {
        if entity.file_id != file.id {
            return Err(EngineError::invalid_argument(
                "entity belongs to a different file",
            ));
        }
        if !entity_ids.insert(&entity.id) {
            return Err(EngineError::invalid_argument("duplicate entity id"));
        }
        entity.validate()?;
        entities::validate_content(&entity.content)?;
        for fragment in &entity.fragments {
            if !fragment_ids.insert(&fragment.id) {
                return Err(EngineError::invalid_argument("duplicate fragment id"));
            }
        }
    }
    fragments::validate_projections(entities, entries)?;
    for entry in entries {
        validate_vector(&entry.vector, model_schema(schema, &entry.model)?)?;
    }
    Ok(())
}

fn model_schema<'a>(
    schemas: &'a [EmbeddingModelInfo],
    model: &str,
) -> EngineResult<&'a EmbeddingModelInfo> {
    schemas
        .iter()
        .find(|schema| schema.model.reference() == model)
        .ok_or_else(|| EngineError::invalid_argument(format!("unknown embedding model {model:?}")))
}

fn validate_vector(vector: &[f32], schema: &EmbeddingModelInfo) -> EngineResult<()> {
    if vector.len() != schema.dimension || vector.iter().any(|value| !value.is_finite()) {
        return Err(EngineError::invalid_argument(format!(
            "expected a finite {}-dimensional vector, got {} values",
            schema.dimension,
            vector.len()
        )));
    }
    if schema.metric == Metric::Cosine && vector.iter().all(|value| *value == 0.0) {
        return Err(EngineError::invalid_argument(
            "cosine vectors must have a non-zero norm",
        ));
    }
    Ok(())
}

fn load_schema(
    path: &Path,
    options: WorkspaceIndexStorageOptions,
) -> EngineResult<Vec<EmbeddingModelInfo>> {
    let descriptor = path.join("schema.json");
    if descriptor.exists() {
        let schema = read_json::<SchemaRecord>(&descriptor)?.embeddings()?;
        if let WorkspaceIndexStorageOptions::ReadWrite { embeddings, .. } = &options {
            if schema.len() != embeddings.len() {
                return Err(EngineError::invalid_argument(
                    "embedding model set changed; rebuild the index",
                ));
            }
            for stored in &schema {
                let current =
                    model_schema(embeddings, &stored.model.reference()).map_err(|_| {
                        EngineError::invalid_argument(
                            "embedding model set changed; rebuild the index",
                        )
                    })?;
                stored.ensure_index_compatible(current)?;
            }
        }
        return Ok(schema);
    }
    if fs::read_dir(path)
        .map_err(|error| io_error("inspect storage directory", path, &error))?
        .next()
        .transpose()
        .map_err(|error| io_error("inspect storage entry", path, &error))?
        .is_some()
    {
        return Err(EngineError::storage_failure(
            "existing workspace storage is missing its schema; rebuild the index",
        ));
    }
    let WorkspaceIndexStorageOptions::ReadWrite { embeddings, .. } = options else {
        return Err(EngineError::not_found(
            "workspace storage schema does not exist",
        ));
    };
    validate_models(&embeddings)?;
    write_record(
        &descriptor,
        &serde_json::to_vec(&SchemaRecord::new(&embeddings)).map_err(|error| json_error(&error))?,
    )?;
    Ok(embeddings)
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
    home: &Path,
    path: &Path,
    options: WorkspaceIndexStorageOptions,
) -> EngineResult<(File, Vec<EmbeddingModelInfo>)> {
    let lock = acquire_storage_lock(home, options.is_read_only())?;
    if !options.is_read_only() {
        let mut builder = DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if let Err(error) = builder.create(path)
            && !(error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir())
        {
            return Err(io_error("create storage directory", path, &error));
        }
    }
    Ok((lock, load_schema(path, options)?))
}

fn acquire_storage_lock(home: &Path, shared: bool) -> EngineResult<File> {
    let path = home.join("storage.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| io_error("open storage lock", &path, &error))?;
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    result.map_err(|error| storage_lock_error(home, error))?;
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
