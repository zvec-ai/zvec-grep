use std::{
    env, fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;
use zg_host_native::NativeScanner;

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::{
            IndexOptions, IndexResult,
            options::{Device, DiscoveryOptions, EmbeddingModelSpec, RootPath},
        },
        info::{
            InfoOptions, InfoResult,
            result::{
                InfoSource, WorkspaceIndexEmbedding, WorkspaceIndexInfo, WorkspaceIndexPolicy,
            },
        },
    },
    models::{
        CreateEmbeddingModelOptions, EmbeddingMetric, ModelError, ModelRuntimeLease,
        ModelRuntimeManager, ModelRuntimeRequest, ResolveEmbeddingReferenceOptions,
        resolve_embedding_reference,
    },
    search::context::{NormalizedContextRequest, context_from_index},
    storage::spi::{
        WorkspaceIndexEmbeddingSchema, WorkspaceIndexStorageFactory, WorkspaceIndexStorageOptions,
    },
    workspace::{
        layout::{
            WorkspaceIndexLocation, find_nearest_workspace, reset_workspace_index,
            workspace_index_location,
        },
        lock::{LockMode, acquire_home_lock},
        manifest::{
            EmbeddingRuntimeConfig, WorkspaceManifest, read_workspace_manifest,
            write_workspace_manifest,
        },
    },
};

use super::pipeline::{IndexingContext, get_workspace_index_status, index_workspace};

const DEFAULT_LOCAL_EMBEDDING: &str = "local/potion-code-16m-v2";
const CURRENT_INDEX_VERSION: u32 = 1;

#[derive(Clone)]
pub(crate) struct WorkspaceIndexService {
    scanner: NativeScanner,
    storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            scanner: NativeScanner::default(),
            storage_factory: Arc::new(crate::storage::ZvecStorageFactory::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_storage_factory(
        storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
    ) -> Self {
        Self {
            scanner: NativeScanner::default(),
            storage_factory,
        }
    }

    pub(crate) async fn index(
        &self,
        models: &ModelRuntimeManager,
        mut options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        if let Some(cache_dir) = options
            .embedding
            .as_mut()
            .and_then(|embedding| embedding.cache_dir.as_mut())
        {
            *cache_dir = std::path::absolute(&*cache_dir).map_err(|error| {
                EngineError::from_io("failed to resolve model cache directory", &error)
            })?;
        }
        let factory = &self.storage_factory;
        let location = workspace_index_location_from_option(options.root.as_deref())?;
        let _lock = acquire_home_lock(
            &location.home,
            LockMode::Write,
            if options.rebuild {
                "index.rebuild"
            } else {
                "index"
            },
        )?;
        let existing = read_workspace_manifest(&location.home)?;
        let model = acquire_model(models, existing.as_ref(), &options)?;
        if !options.rebuild {
            assert_embedding_compatible(existing.as_ref(), &model)?;
        }

        if options.rebuild
            || existing
                .as_ref()
                .is_none_or(|manifest| !is_indexed(manifest))
        {
            reset_workspace_index(&location, factory.as_ref())?;
        }
        let existing_for_manifest = if options.rebuild {
            None
        } else {
            existing.as_ref()
        };
        let roots = resolve_root_paths(&location.root, existing.as_ref(), &options);
        let now = epoch_millis();
        let info = WorkspaceIndexInfo {
            id: existing_for_manifest.map_or_else(
                || Uuid::new_v4().to_string(),
                |manifest| manifest.id.clone(),
            ),
            name: existing_for_manifest.map_or_else(
                || workspace_name(&location.root),
                |manifest| manifest.name.clone(),
            ),
            path: location.home.clone(),
            roots,
            policy: WorkspaceIndexPolicy::Enabled,
            embedding: Some(embedding_schema(&model)),
            index_version: Some(CURRENT_INDEX_VERSION),
            generation: existing_for_manifest.and_then(|manifest| manifest.index_info().generation),
            created_epoch_ms: existing_for_manifest.map_or(now, |manifest| manifest.created_time),
            updated_epoch_ms: now,
        };
        let runtime = embedding_runtime(existing.as_ref(), &options, &model);
        let mut manifest = WorkspaceManifest::new(info.clone(), runtime)?;
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: location.home.clone(),
            embedding: storage_embedding_schema(&model),
        })?;
        if let Err(error) = write_workspace_manifest(&location.home, &manifest) {
            let _ = storage.close();
            return Err(error);
        }

        let result = index_workspace(&IndexingContext {
            workspace_index: &info,
            storage: storage.as_ref(),
            scanner: &self.scanner,
            embedding_model: &model,
            embedding_concurrency: options.embedding_concurrency,
            on_progress: options.on_progress,
            signal: options.signal,
            changes: &options.changes,
        })
        .await;
        manifest.updated_time = epoch_millis();
        if let Ok(indexed) = &result {
            manifest.generation = Some(indexed.generation);
        }
        let manifest_result = write_workspace_manifest(&location.home, &manifest);
        let close_result = storage.close();

        let indexed = result?;
        manifest_result?;
        close_result?;
        Ok(indexed)
    }

    pub(crate) async fn context(
        &self,
        models: &ModelRuntimeManager,
        options: &ContextOptions,
        request: &NormalizedContextRequest,
    ) -> Result<ContextResult, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let Some(location) = find_nearest_workspace(&requested_root)? else {
            return Err(workspace_index_unavailable(
                &requested_root,
                "no workspace manifest was found",
            ));
        };
        let factory = &self.storage_factory;
        if !factory.exists(&location.home)? {
            return Err(workspace_index_unavailable(
                &location.root,
                "index storage is missing",
            ));
        }
        if options.auto_update
            && self
                .workspace_needs_refresh(&location, factory.as_ref())
                .await?
        {
            self.index(
                models,
                IndexOptions {
                    root: Some(location.root.clone()),
                    embedding_concurrency: options.embedding_concurrency,
                    ..IndexOptions::default()
                },
            )
            .await?;
        }
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "context")?;
        let manifest = read_workspace_manifest(&location.home)?.ok_or_else(|| {
            workspace_index_unavailable(&location.root, "workspace manifest disappeared")
        })?;
        if manifest.index_policy == WorkspaceIndexPolicy::Disabled {
            return Err(EngineError::unsupported(format!(
                "workspace indexing is disabled at {}",
                location.root.display()
            )));
        }
        if !is_indexed(&manifest) {
            return Err(workspace_index_unavailable(
                &location.root,
                "workspace index has not been built",
            ));
        }
        let model = request
            .routes
            .iter()
            .any(|route| route.mode == crate::api::context::options::ContextRouteMode::Vector)
            .then(|| acquire_search_model(models, &manifest, options.embedding_concurrency))
            .transpose()?;
        if let Some(model) = &model {
            assert_embedding_compatible(Some(&manifest), model)?;
        }
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: location.home.clone(),
        })?;
        let result = context_from_index(
            &location.root,
            &manifest.index_info(),
            storage.as_ref(),
            model
                .as_ref()
                .map(|model| model as &dyn crate::search::SearchEmbeddingRuntime),
            options,
            request,
        )
        .await;
        let close = storage.close();
        let result = result?;
        close?;
        Ok(result)
    }

    async fn workspace_needs_refresh(
        &self,
        location: &WorkspaceIndexLocation,
        factory: &dyn WorkspaceIndexStorageFactory,
    ) -> Result<bool, EngineError> {
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "context.refresh")?;
        let Some(manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(false);
        };
        if !is_indexed(&manifest) || manifest.index_policy == WorkspaceIndexPolicy::Disabled {
            return Ok(false);
        }
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: location.home.clone(),
        })?;
        let status = get_workspace_index_status(
            &manifest.index_info(),
            storage.as_ref(),
            &self.scanner,
            None,
        )
        .await;
        let close = storage.close();
        let status = status?;
        close?;
        Ok(status.files_added > 0
            || status.files_modified > 0
            || status.files_deleted > 0
            || status.files_pending > 0
            || status.files_failed > 0)
    }

    pub(crate) async fn info(&self, options: InfoOptions) -> Result<InfoResult, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let requested_location = workspace_index_location(&requested_root)?;
        let Some(location) = find_nearest_workspace(&requested_root)? else {
            return Ok(unindexed_info(
                requested_location,
                WorkspaceIndexPolicy::Undecided,
            ));
        };
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "info")?;
        let Some(manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(unindexed_info(location, WorkspaceIndexPolicy::Undecided));
        };
        let metadata_indexed = is_indexed(&manifest);
        let storage_exists = self.storage_factory.exists(&location.home)?;
        let indexed = metadata_indexed && storage_exists;
        let status = if options.include_status && indexed {
            let factory = &self.storage_factory;
            let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: location.home.clone(),
            })?;
            let status = get_workspace_index_status(
                &manifest.index_info(),
                storage.as_ref(),
                &self.scanner,
                None,
            )
            .await;
            let close = storage.close();
            let status = status?;
            close?;
            Some(status)
        } else {
            None
        };

        Ok(InfoResult {
            root: location.root,
            indexed,
            index_policy: manifest.index_policy,
            home: location.home,
            index_path: location.index_path,
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(manifest.index_info()),
            status,
            suggestion: workspace_suggestion(&manifest, indexed),
        })
    }

    pub(crate) fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let location = workspace_index_location_from_option(options.root.as_deref())?;
        let factory = &self.storage_factory;
        if !location.manifest_path.exists() && !factory.exists(&location.home)? {
            return Ok(false);
        }
        let _lock = acquire_home_lock(&location.home, LockMode::Write, "index.drop")?;
        if !location.manifest_path.exists() && !factory.exists(&location.home)? {
            return Ok(false);
        }
        reset_workspace_index(&location, factory.as_ref())?;
        Ok(true)
    }
}

impl Default for WorkspaceIndexService {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WorkspaceIndexService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceIndexService")
            .field("scanner", &self.scanner)
            .finish_non_exhaustive()
    }
}

fn acquire_model(
    models: &ModelRuntimeManager,
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Result<ModelRuntimeLease, EngineError> {
    if options
        .embedding
        .as_ref()
        .and_then(|embedding| embedding.revision.as_ref())
        .is_some()
    {
        return Err(EngineError::unsupported(
            "embedding revision overrides are not supported by the catalog-backed runtime",
        ));
    }
    let reference = embedding_reference(existing, options.embedding.as_ref())?;
    let local = reference.starts_with("local/");
    let existing_runtime = existing.map(|manifest| &manifest.embedding_runtime);
    let api_key = if local {
        None
    } else {
        existing_runtime
            .and_then(|runtime| runtime.api_key.clone())
            .or_else(environment_api_key)
    };
    let endpoint = options
        .embedding
        .as_ref()
        .and_then(|embedding| embedding.endpoint.clone())
        .or_else(|| existing_runtime.and_then(|runtime| runtime.endpoint.clone()));
    let device = local.then(|| {
        options.embedding.as_ref().map_or_else(
            || {
                existing_runtime
                    .and_then(|runtime| runtime.device)
                    .unwrap_or(Device::Auto)
            },
            |embedding| embedding.device,
        )
    });
    models
        .acquire(ModelRuntimeRequest::new(
            reference,
            CreateEmbeddingModelOptions {
                api_key,
                endpoint,
                model_cache_dir: options
                    .embedding
                    .as_ref()
                    .and_then(|embedding| embedding.cache_dir.clone())
                    .or_else(|| existing_runtime.and_then(|runtime| runtime.cache_dir.clone())),
                device,
                ..CreateEmbeddingModelOptions::default()
            },
            options.embedding_concurrency,
        ))
        .map_err(ModelError::into_engine_error)
}

fn acquire_search_model(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    embedding_concurrency: Option<usize>,
) -> Result<ModelRuntimeLease, EngineError> {
    let schema = manifest.embedding.as_ref().ok_or_else(|| {
        workspace_index_unavailable(&manifest.path, "embedding schema is missing")
    })?;
    let reference = format!("{}/{}", schema.provider, schema.model);
    let local = schema.provider == "local";
    models
        .acquire(ModelRuntimeRequest::new(
            reference,
            CreateEmbeddingModelOptions {
                api_key: (!local)
                    .then(|| {
                        manifest
                            .embedding_runtime
                            .api_key
                            .clone()
                            .or_else(environment_api_key)
                    })
                    .flatten(),
                endpoint: (!local)
                    .then(|| manifest.embedding_runtime.endpoint.clone())
                    .flatten(),
                device: local.then(|| manifest.embedding_runtime.device.unwrap_or(Device::Auto)),
                model_cache_dir: manifest.embedding_runtime.cache_dir.clone(),
                ..CreateEmbeddingModelOptions::default()
            },
            embedding_concurrency,
        ))
        .map_err(ModelError::into_engine_error)
}

fn embedding_reference(
    existing: Option<&WorkspaceManifest>,
    requested: Option<&EmbeddingModelSpec>,
) -> Result<String, EngineError> {
    resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit: requested.map(|embedding| embedding.reference.clone()),
        existing: existing
            .and_then(|manifest| manifest.embedding.as_ref())
            .map(|embedding| format!("{}/{}", embedding.provider, embedding.model)),
        fallback: Some(DEFAULT_LOCAL_EMBEDDING.to_owned()),
        ..ResolveEmbeddingReferenceOptions::default()
    })
    .map_err(ModelError::into_engine_error)?
    .ok_or_else(|| EngineError::internal("default embedding model is not configured"))
}

fn environment_api_key() -> Option<String> {
    ["ZVEC_GREP_API_KEY", "DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        .into_iter()
        .find_map(|name| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

fn assert_embedding_compatible(
    existing: Option<&WorkspaceManifest>,
    model: &ModelRuntimeLease,
) -> Result<(), EngineError> {
    let Some(existing) = existing.filter(|manifest| is_indexed(manifest)) else {
        return Ok(());
    };
    let Some(schema) = &existing.embedding else {
        return Ok(());
    };
    let current = model.info();
    if schema.provider == current.provider
        && schema.model == current.name
        && schema.dimension == current.dimension
        && schema.metric == metric_name(current.metric)
    {
        return Ok(());
    }
    Err(EngineError::invalid_argument(
        "existing index uses a different embedding model; rebuild the index",
    ))
}

fn resolve_root_paths(
    workspace_root: &Path,
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Vec<RootPath> {
    let mut roots = if options.roots.is_empty() {
        existing.map_or_else(
            || vec![default_root_path(workspace_root)],
            |manifest| manifest.index_info().roots,
        )
    } else {
        options.roots.clone()
    };
    for root in &mut roots {
        if !root.path.is_absolute() {
            root.path = workspace_root.join(&root.path);
        }
        if options.reset_paths {
            root.discovery = DiscoveryOptions::default();
        }
        apply_discovery_overrides(&mut root.discovery, &options.discovery);
    }
    roots
}

fn apply_discovery_overrides(target: &mut DiscoveryOptions, overrides: &DiscoveryOptions) {
    if !overrides.include_paths.is_empty() {
        target.include_paths.clone_from(&overrides.include_paths);
    }
    if !overrides.exclude_paths.is_empty() {
        target.exclude_paths.clone_from(&overrides.exclude_paths);
    }
    if !overrides.globs.is_empty() {
        target.globs.clone_from(&overrides.globs);
    }
    if !overrides.insensitive_globs.is_empty() {
        target
            .insensitive_globs
            .clone_from(&overrides.insensitive_globs);
    }
    if !overrides.file_types.is_empty() {
        target.file_types.clone_from(&overrides.file_types);
    }
    if !overrides.excluded_file_types.is_empty() {
        target
            .excluded_file_types
            .clone_from(&overrides.excluded_file_types);
    }
    if !overrides.ignore_files.is_empty() {
        target.ignore_files.clone_from(&overrides.ignore_files);
    }
    target.hidden |= overrides.hidden;
    target.no_ignore |= overrides.no_ignore;
    target.follow |= overrides.follow;
    if overrides.max_depth.is_some() {
        target.max_depth = overrides.max_depth;
    }
    if overrides.max_file_size_bytes.is_some() {
        target.max_file_size_bytes = overrides.max_file_size_bytes;
    }
}

fn embedding_runtime(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
    model: &ModelRuntimeLease,
) -> EmbeddingRuntimeConfig {
    let current = existing
        .map(|manifest| manifest.embedding_runtime.clone())
        .unwrap_or_default();
    if model.info().provider == "local" {
        EmbeddingRuntimeConfig {
            cache_dir: options
                .embedding
                .as_ref()
                .and_then(|embedding| embedding.cache_dir.clone())
                .or(current.cache_dir),
            device: Some(
                options
                    .embedding
                    .as_ref()
                    .map_or(current.device.unwrap_or(Device::Auto), |embedding| {
                        embedding.device
                    }),
            ),
            ..EmbeddingRuntimeConfig::default()
        }
    } else {
        EmbeddingRuntimeConfig {
            api_key: current.api_key,
            endpoint: options
                .embedding
                .as_ref()
                .and_then(|embedding| embedding.endpoint.clone())
                .or(current.endpoint)
                .or_else(|| model.info().endpoint.clone()),
            device: None,
            cache_dir: None,
        }
    }
}

fn embedding_schema(model: &ModelRuntimeLease) -> WorkspaceIndexEmbedding {
    let info = model.info();
    WorkspaceIndexEmbedding {
        provider: info.provider.clone(),
        model: info.name.clone(),
        dimension: info.dimension,
        metric: metric_name(info.metric).to_owned(),
    }
}

fn storage_embedding_schema(model: &ModelRuntimeLease) -> WorkspaceIndexEmbeddingSchema {
    let info = model.info();
    WorkspaceIndexEmbeddingSchema {
        provider: info.provider.clone(),
        model: info.name.clone(),
        dimension: info.dimension,
        metric: info.metric,
    }
}

const fn metric_name(metric: EmbeddingMetric) -> &'static str {
    match metric {
        EmbeddingMetric::Cosine => "cosine",
        EmbeddingMetric::DotProduct => "dot",
        EmbeddingMetric::Euclidean => "euclidean",
    }
}

fn is_indexed(manifest: &WorkspaceManifest) -> bool {
    manifest.index_policy == WorkspaceIndexPolicy::Enabled
        && manifest.embedding.is_some()
        && manifest.index_version.is_some()
}

fn workspace_index_location_from_option(
    root: Option<&Path>,
) -> Result<WorkspaceIndexLocation, EngineError> {
    workspace_index_location(&resolve_root(root)?)
}

fn resolve_root(root: Option<&Path>) -> Result<PathBuf, EngineError> {
    let root = root
        .map_or_else(env::current_dir, |root| Ok(root.to_path_buf()))
        .map_err(|error| EngineError::from_io("failed to resolve current directory", &error))?;
    if root.is_absolute() {
        Ok(root)
    } else {
        env::current_dir()
            .map(|current| current.join(root))
            .map_err(|error| EngineError::from_io("failed to resolve workspace root", &error))
    }
}

fn default_root_path(root: &Path) -> RootPath {
    RootPath {
        path: root.to_path_buf(),
        recursive: true,
        discovery: DiscoveryOptions::default(),
    }
}

fn workspace_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

fn unindexed_info(location: WorkspaceIndexLocation, policy: WorkspaceIndexPolicy) -> InfoResult {
    InfoResult {
        root: location.root,
        indexed: false,
        index_policy: policy,
        home: location.home,
        index_path: location.index_path,
        source: InfoSource::Unindexed,
        workspace_index: None,
        status: None,
        suggestion: Some("run index to create a workspace index".to_owned()),
    }
}

fn workspace_suggestion(manifest: &WorkspaceManifest, indexed: bool) -> Option<String> {
    if manifest.index_policy == WorkspaceIndexPolicy::Disabled {
        Some("indexing is disabled for this workspace".to_owned())
    } else if !indexed {
        Some("workspace manifest exists but index storage is missing".to_owned())
    } else {
        None
    }
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use tempfile::tempdir;

    use crate::{
        api::{
            index::{
                IndexOptions,
                options::{Device, DiscoveryOptions, EmbeddingModelSpec, RootPath},
            },
            info::InfoOptions,
        },
        storage::spi::{
            FileIndexDiagnostics, IndexedFragment, StorageResult, StorageSearchFilter,
            StorageSearchHit, StoredEntity, StoredFile, WorkspaceIndexStorage,
            WorkspaceIndexStorageFactory, WorkspaceIndexStorageOptions,
        },
    };

    use super::{ModelRuntimeManager, WorkspaceIndexService};

    #[derive(Debug, Default)]
    struct MemoryStorageFactory {
        exists: Arc<AtomicBool>,
    }

    impl WorkspaceIndexStorageFactory for MemoryStorageFactory {
        fn open(
            &self,
            options: WorkspaceIndexStorageOptions,
        ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
            if !options.is_read_only() {
                self.exists.store(true, Ordering::Release);
            }
            Ok(Box::new(EmptyStorage {
                read_only: options.is_read_only(),
            }))
        }

        fn exists(&self, _storage_path: &Path) -> StorageResult<bool> {
            Ok(self.exists.load(Ordering::Acquire))
        }

        fn delete(&self, _storage_path: &Path) -> StorageResult<()> {
            self.exists.store(false, Ordering::Release);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct EmptyStorage {
        read_only: bool,
    }

    #[async_trait::async_trait]
    impl WorkspaceIndexStorage for EmptyStorage {
        fn is_read_only(&self) -> bool {
            self.read_only
        }

        fn list_files(&self) -> StorageResult<Vec<StoredFile>> {
            Ok(Vec::new())
        }

        fn get_entity(
            &self,
            _entity_id: &crate::domain::EntityId,
        ) -> StorageResult<Option<StoredEntity>> {
            Ok(None)
        }

        fn search_fts(
            &self,
            _query: &str,
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn search_vector(
            &self,
            _vector: &[f32],
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn replace_file(
            &self,
            _file: &StoredFile,
            _entries: &[IndexedFragment],
            _diagnostics: Option<&FileIndexDiagnostics>,
        ) -> StorageResult<()> {
            Ok(())
        }

        fn mark_file_failed(&self, _file: &StoredFile, _error: &str) -> StorageResult<()> {
            Ok(())
        }

        fn delete_file(&self, _file_id: &crate::domain::FileId) -> StorageResult<()> {
            Ok(())
        }

        async fn finalize_writes(&self) -> StorageResult<()> {
            Ok(())
        }

        fn close(&self) -> StorageResult<()> {
            Ok(())
        }
    }

    #[test]
    fn dropping_a_missing_index_is_an_idempotent_no_op() {
        let directory = tempdir().expect("temporary directory");

        assert!(
            !WorkspaceIndexService::new()
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .expect("missing index should be an idempotent no-op")
        );
    }

    #[tokio::test]
    async fn composes_workspace_lifecycle_around_the_indexing_pipeline() {
        let directory = tempdir().expect("temporary directory");
        let sources = directory.path().join("sources");
        std::fs::create_dir(&sources).expect("source directory");
        let factory = Arc::new(MemoryStorageFactory::default());
        let service = WorkspaceIndexService::with_storage_factory(factory.clone());
        let models = ModelRuntimeManager::new();

        let result = service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    roots: vec![RootPath {
                        path: sources,
                        recursive: true,
                        discovery: DiscoveryOptions::default(),
                    }],
                    embedding: Some(EmbeddingModelSpec {
                        reference: "local/potion-code-16m-v2".to_owned(),
                        revision: None,
                        cache_dir: Some(directory.path().join("model-cache")),
                        endpoint: None,
                        device: Device::Cpu,
                    }),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("empty workspace should index");
        assert_eq!(result.files_scanned, 0);
        assert!(factory.exists.load(Ordering::Acquire));

        let info = service
            .info(InfoOptions {
                root: Some(directory.path().to_path_buf()),
                include_status: true,
            })
            .await
            .expect("workspace info");
        assert!(info.indexed);
        assert_eq!(info.status.expect("index status").files_stored, 0);
        assert_eq!(
            info.workspace_index.expect("workspace index").generation,
            Some(1)
        );
        let manifest = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(
            manifest.embedding_runtime.cache_dir,
            Some(directory.path().join("model-cache"))
        );
        let lease = super::acquire_search_model(&models, &manifest, None).expect("search model");
        assert_eq!(
            models.snapshot().cached_runtimes,
            1,
            "search reuses the configured model cache"
        );
        drop(lease);

        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    rebuild: true,
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("rebuild preserves configured roots and model runtime");
        let rebuilt = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(rebuilt.root_paths, manifest.root_paths);
        assert_eq!(rebuilt.embedding_runtime, manifest.embedding_runtime);
        assert_eq!(rebuilt.generation, Some(1));

        assert!(
            service
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .expect("drop index")
        );
        assert!(!factory.exists.load(Ordering::Acquire));
        models.close();
    }
}
