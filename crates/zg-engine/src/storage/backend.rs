use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{EntityId, FileId, validate_fragments},
    models::EmbeddingMetric,
    utils,
};

use super::{
    codec, dictionary,
    spi::{
        FileIndexDiagnostics, FileIndexStatus, IndexedFragment, StorageResult, StorageSearchFilter,
        StorageSearchHit, StoredEntity, StoredFile, WorkspaceIndexEmbeddingSchema,
        WorkspaceIndexStorage, WorkspaceIndexStorageFactory, WorkspaceIndexStorageOptions,
    },
    zvec::NativeStore,
};

const VERSION: u32 = 2;
const JOURNAL: &str = "pending.json";
type StoreRegistry = Mutex<HashMap<PathBuf, Weak<SharedStore>>>;
static STORES: OnceLock<StoreRegistry> = OnceLock::new();
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

pub(crate) struct ZvecStorageFactory;

impl ZvecStorageFactory {
    pub(crate) fn new() -> Self {
        Self
    }
}

struct SharedStore {
    state: Mutex<StoreState>,
    path: PathBuf,
    schema: WorkspaceIndexEmbeddingSchema,
    read_only: bool,
    // The native handles must close before the operating-system lock is released.
    _lock: File,
}

struct StoreState {
    native: NativeStore,
    needs_recovery: bool,
}

struct ZvecStorage {
    shared: Mutex<Option<Arc<SharedStore>>>,
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRecord {
    version: u32,
    provider: String,
    model: String,
    dimension: usize,
    metric: String,
}

impl SchemaRecord {
    fn new(schema: &WorkspaceIndexEmbeddingSchema) -> Self {
        Self {
            version: VERSION,
            provider: schema.provider.clone(),
            model: schema.model.clone(),
            dimension: schema.dimension,
            metric: match schema.metric {
                EmbeddingMetric::Cosine => "cosine",
                EmbeddingMetric::DotProduct => "dot",
                EmbeddingMetric::Euclidean => "euclidean",
            }
            .to_owned(),
        }
    }

    fn schema(self) -> EngineResult<WorkspaceIndexEmbeddingSchema> {
        if self.version != VERSION {
            return Err(EngineError::storage_failure(format!(
                "unsupported storage schema version {}; expected {VERSION}; rebuild the index",
                self.version
            )));
        }
        if !(1..=20_000).contains(&self.dimension) {
            return Err(EngineError::storage_failure(
                "stored embedding dimension must be in 1..=20,000",
            ));
        }
        let metric = match self.metric.as_str() {
            "cosine" => EmbeddingMetric::Cosine,
            "dot" => EmbeddingMetric::DotProduct,
            "euclidean" => EmbeddingMetric::Euclidean,
            _ => {
                return Err(EngineError::storage_failure(
                    "unknown stored embedding metric",
                ));
            }
        };
        Ok(WorkspaceIndexEmbeddingSchema {
            provider: self.provider,
            model: self.model,
            dimension: self.dimension,
            metric,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    version: u32,
    operation: Operation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Replace {
        source: String,
        status: Option<FileIndexStatus>,
        entries: Vec<EntryRecord>,
    },
    Delete {
        file_id: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryRecord {
    fragment: String,
    vector: Vec<f32>,
}

impl WorkspaceIndexStorageFactory for ZvecStorageFactory {
    fn open(
        &self,
        options: WorkspaceIndexStorageOptions,
    ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
        initialize()?;
        let home = options.storage_path();
        if !home.is_absolute() {
            return Err(EngineError::invalid_argument(
                "storage path must be absolute",
            ));
        }
        let read_only = options.is_read_only();
        if !read_only {
            utils::create_directories(home)
                .map_err(|error| io_error("create index directory", home, &error))?;
        }
        let home = fs::canonicalize(home)
            .map_err(|error| io_error("locate index directory", home, &error))?;
        let path = home.join("storage");
        let mut registry = registry()?;
        if let Some(shared) = registry.get(&path).and_then(Weak::upgrade) {
            if read_only && shared.read_only {
                return Ok(Box::new(ZvecStorage {
                    shared: Mutex::new(Some(shared)),
                    read_only,
                }));
            }
            return Err(EngineError::resource_busy(
                "workspace storage is already open",
            ));
        }
        let (lock, schema) = prepare_storage(&home, &path, &options)?;
        let native = NativeStore::open(&path, &schema, read_only)?;
        let shared = Arc::new(SharedStore {
            state: Mutex::new(StoreState {
                native,
                needs_recovery: false,
            }),
            path: path.clone(),
            schema,
            read_only,
            _lock: lock,
        });
        registry.insert(path, Arc::downgrade(&shared));
        Ok(Box::new(ZvecStorage {
            shared: Mutex::new(Some(shared)),
            read_only,
        }))
    }

    fn exists(&self, storage_path: &Path) -> StorageResult<bool> {
        match fs::metadata(storage_path.join("storage")) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect storage", storage_path, &error)),
        }
    }

    fn delete(&self, storage_path: &Path) -> StorageResult<()> {
        if !self.exists(storage_path)? {
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
}

impl ZvecStorage {
    fn shared(&self) -> EngineResult<Arc<SharedStore>> {
        self.shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| EngineError::resource_closed("workspace storage is closed"))
    }

    fn read<T>(&self, operation: impl FnOnce(&NativeStore) -> EngineResult<T>) -> EngineResult<T> {
        let shared = self.shared()?;
        let state = lock_state(&shared)?;
        if state.needs_recovery {
            return Err(recovery_required());
        }
        operation(&state.native)
    }

    fn commit(&self, operation: Operation) -> EngineResult<()> {
        if self.read_only {
            return Err(EngineError::invalid_argument(
                "cannot write read-only workspace storage",
            ));
        }
        let shared = self.shared()?;
        let record = JournalRecord {
            version: VERSION,
            operation,
        };
        let encoded = serde_json::to_vec(&record).map_err(|error| json_error(&error))?;
        let mut state = lock_state(&shared)?;
        if state.needs_recovery {
            return Err(recovery_required());
        }
        // A durable redo record precedes every native mutation. A failed commit
        // blocks reads until reopen replays it; partial collections are never served.
        state.needs_recovery = true;
        write_record(&shared.path.join(JOURNAL), &encoded)?;
        replay(&state.native, &shared.schema, record)?;
        state.native.flush()?;
        clear_journal(&shared.path)?;
        state.needs_recovery = false;
        Ok(())
    }
}

#[async_trait]
impl WorkspaceIndexStorage for ZvecStorage {
    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn list_files(&self) -> StorageResult<Vec<StoredFile>> {
        self.read(NativeStore::list_files)
    }

    fn get_entity(&self, id: &EntityId) -> StorageResult<Option<StoredEntity>> {
        self.read(|native| native.get_entity(id))
    }

    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>> {
        self.read(|native| native.search_fts(query, limit, filter))
    }

    fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>> {
        let shared = self.shared()?;
        validate_vector(vector, &shared.schema)?;
        self.read(|native| native.search_vector(vector, limit, filter))
    }

    fn replace_file(
        &self,
        file: &StoredFile,
        entries: &[IndexedFragment],
        diagnostics: Option<&FileIndexDiagnostics>,
    ) -> StorageResult<()> {
        let shared = self.shared()?;
        validate_batch(file, entries, &shared.schema)?;
        let mut file = file.clone();
        file.index_status = Some(FileIndexStatus {
            indexed_epoch_ms: Some(now_epoch_ms()?),
            entity_count: entries
                .iter()
                .filter(|entry| entry.fragment.as_entity().is_some())
                .count(),
            token_count: None,
            truncated_fragment_count: diagnostics.and_then(|value| value.truncated_fragment_count),
            error: None,
        });
        self.commit(replace_operation(&file, entries)?)
    }

    fn mark_file_failed(&self, file: &StoredFile, error: &str) -> StorageResult<()> {
        file.source.validate()?;
        let mut file = file.clone();
        file.index_status = Some(FileIndexStatus {
            indexed_epoch_ms: None,
            entity_count: 0,
            token_count: None,
            truncated_fragment_count: None,
            error: Some(error.to_owned()),
        });
        self.commit(replace_operation(&file, &[])?)
    }

    fn delete_file(&self, file_id: &FileId) -> StorageResult<()> {
        self.commit(Operation::Delete {
            file_id: file_id.as_str().to_owned(),
        })
    }

    async fn finalize_writes(&self) -> StorageResult<()> {
        if self.read_only {
            return Ok(());
        }
        self.read(NativeStore::flush)
    }

    fn close(&self) -> StorageResult<()> {
        self.shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?
            .take();
        Ok(())
    }
}

fn replace_operation(file: &StoredFile, entries: &[IndexedFragment]) -> EngineResult<Operation> {
    Ok(Operation::Replace {
        source: codec::encode_file(&file.source)?,
        status: file.index_status.clone(),
        entries: entries
            .iter()
            .map(|entry| {
                Ok(EntryRecord {
                    fragment: codec::encode_fragment(&entry.fragment)?,
                    vector: entry.vector.clone(),
                })
            })
            .collect::<EngineResult<_>>()?,
    })
}

fn replay(
    native: &NativeStore,
    schema: &WorkspaceIndexEmbeddingSchema,
    record: JournalRecord,
) -> EngineResult<()> {
    if record.version != VERSION {
        return Err(EngineError::storage_failure(format!(
            "unsupported storage journal version {}; expected {VERSION}; rebuild the index",
            record.version
        )));
    }
    match record.operation {
        Operation::Replace {
            source,
            status,
            entries,
        } => {
            let file = StoredFile {
                source: codec::decode_file(&source)?,
                index_status: status,
            };
            let entries = entries
                .into_iter()
                .map(|entry| {
                    Ok(IndexedFragment {
                        fragment: codec::decode_fragment(&entry.fragment)?,
                        vector: entry.vector,
                    })
                })
                .collect::<EngineResult<Vec<_>>>()?;
            validate_batch(&file, &entries, schema)?;
            native.apply_replace(&file, &entries)
        }
        Operation::Delete { file_id } => native.apply_delete(&FileId::new(file_id)?),
    }
}

fn validate_batch(
    file: &StoredFile,
    entries: &[IndexedFragment],
    schema: &WorkspaceIndexEmbeddingSchema,
) -> EngineResult<()> {
    file.source.validate()?;
    validate_fragments(&file.source.id, entries.iter().map(|entry| &entry.fragment))?;
    for entry in entries {
        validate_vector(&entry.vector, schema)?;
    }
    Ok(())
}

fn validate_vector(vector: &[f32], schema: &WorkspaceIndexEmbeddingSchema) -> EngineResult<()> {
    if vector.len() != schema.dimension || vector.iter().any(|value| !value.is_finite()) {
        return Err(EngineError::invalid_argument(format!(
            "expected a finite {}-dimensional vector, got {} values",
            schema.dimension,
            vector.len()
        )));
    }
    if schema.metric == EmbeddingMetric::Cosine && vector.iter().all(|value| *value == 0.0) {
        return Err(EngineError::invalid_argument(
            "cosine vectors must have a non-zero norm",
        ));
    }
    Ok(())
}

fn initialize() -> EngineResult<()> {
    match INITIALIZED.get_or_init(|| {
        let config = zvec_rust::ConfigBuilder::new()
            .num_threads(2)
            .memory_limit(512 * 1024 * 1024)
            .build();
        zvec_rust::initialize(Some(&config)).map_err(|error| error.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(EngineError::storage_failure(format!(
            "cannot initialize zvec: {message}"
        ))),
    }
}

fn load_schema(
    path: &Path,
    options: &WorkspaceIndexStorageOptions,
) -> EngineResult<WorkspaceIndexEmbeddingSchema> {
    let descriptor = path.join("schema.json");
    if descriptor.exists() {
        let schema = read_json::<SchemaRecord>(&descriptor)?.schema()?;
        if let WorkspaceIndexStorageOptions::ReadWrite { embedding, .. } = options
            && &schema != embedding
        {
            return Err(EngineError::invalid_argument(
                "stored embedding schema differs from the requested model; rebuild the index",
            ));
        }
        return Ok(schema);
    }
    let WorkspaceIndexStorageOptions::ReadWrite { embedding, .. } = options else {
        return Err(EngineError::not_found(
            "workspace storage schema does not exist",
        ));
    };
    if !(1..=20_000).contains(&embedding.dimension) {
        return Err(EngineError::invalid_argument(
            "embedding dimension must be in 1..=20,000",
        ));
    }
    utils::create_directories(path)
        .map_err(|error| io_error("create storage directory", path, &error))?;
    write_record(
        &descriptor,
        &serde_json::to_vec(&SchemaRecord::new(embedding)).map_err(|error| json_error(&error))?,
    )?;
    Ok(embedding.clone())
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
    options: &WorkspaceIndexStorageOptions,
) -> EngineResult<(File, WorkspaceIndexEmbeddingSchema)> {
    let read_only = options.is_read_only();
    let mut shared = read_only;
    loop {
        let lock = acquire_storage_lock(home, shared)?;
        let journal = path.join(JOURNAL);
        if shared && journal.exists() {
            // Release before changing lock modes; Windows does not convert held locks.
            shared = false;
            continue;
        }
        let schema = load_schema(path, options)?;
        if !read_only {
            utils::create_directories(path)
                .map_err(|error| io_error("create storage directory", path, &error))?;
        }
        dictionary::prepare(&path.join("dictionary"))?;
        if journal.exists() {
            let native = NativeStore::open(path, &schema, false)?;
            replay(&native, &schema, read_json(&journal)?)?;
            native.flush()?;
            clear_journal(path)?;
        }
        if read_only && !shared {
            // Recheck after reacquiring: another writer may run between locks.
            shared = true;
            continue;
        }
        return Ok((lock, schema));
    }
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

fn clear_journal(path: &Path) -> EngineResult<()> {
    let journal = path.join(JOURNAL);
    fs::remove_file(&journal)
        .map_err(|error| io_error("remove committed storage journal", &journal, &error))?;
    sync_directory(path)
}

fn write_record(path: &Path, bytes: &[u8]) -> EngineResult<()> {
    utils::atomic_write(path, bytes).map_err(|error| io_error("persist storage file", path, &error))
}

fn sync_directory(path: &Path) -> EngineResult<()> {
    utils::sync_directory(path).map_err(|error| io_error("sync storage directory", path, &error))
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

fn recovery_required() -> EngineError {
    EngineError::resource_busy("storage has an unfinished write; close and reopen it to recover")
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
