use std::{
    collections::BTreeMap,
    env, fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use std::sync::Arc;

use zg_host_native::NativeScanner;

use crate::{
    EngineError,
    api::{
        index::{IndexOptions, IndexResult, options::EmbeddingModelSpec},
        info::{
            InfoOptions, InfoResult,
            result::{IndexCompatibility, InfoSource, WorkspaceIndexInfo, WorkspaceIndexPolicy},
        },
    },
    domain::{
        IndexDescriptor, IndexState, Workspace,
        model::{Device, ModelConfig},
    },
    models::{
        ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest,
        ResolveEmbeddingReferenceOptions, resolve_embedding_reference, resolve_index_concurrency,
    },
    storage::{IndexStore, types::WorkspaceIndexStorageOptions},
    workspace::{
        CURRENT_INDEX_VERSION,
        build::{
            WorkspaceBuild, discard_build, has_build, has_index_storage, prepare_build,
            publish_build, read_build_registration, recover_build, recover_build_for_rebuild,
        },
        credentials::{EncryptedApiKey, StoredModelConfig},
        layout::{
            WorkspaceIndexLocation, find_nearest_workspace, reset_workspace_index,
            workspace_index_location,
        },
        lock::{FileLock, LockMode, LockWait},
        manifest::{
            ManifestState, WorkspaceManifest, inspect_workspace_manifest, read_workspace_manifest,
            write_workspace_manifest,
        },
        registry::WorkspaceRegistry,
    },
};

use super::pipeline::{
    IndexEmbeddingRuntime, IndexingContext, get_workspace_index_status, index_workspace,
};

const DEFAULT_LOCAL_EMBEDDING: &str = "local/potion-code-16m-v2";

#[derive(Clone)]
pub(crate) struct WorkspaceIndexService {
    pub(crate) writers: crate::pipelines::indexed_search::writer::WriterContexts,
    scanner: NativeScanner,
    registry: Option<WorkspaceRegistry>,
    #[cfg(test)]
    _registry_directory: Option<Arc<tempfile::TempDir>>,
    #[cfg(test)]
    fail_index_completion: bool,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            writers: crate::pipelines::indexed_search::writer::WriterContexts::default(),
            scanner: NativeScanner::default(),
            registry: None,
            #[cfg(test)]
            _registry_directory: None,
            #[cfg(test)]
            fail_index_completion: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_registry() -> Self {
        let directory = Arc::new(tempfile::tempdir().expect("test workspace registry"));
        Self {
            writers: crate::pipelines::indexed_search::writer::WriterContexts::default(),
            scanner: NativeScanner::default(),
            registry: Some(
                WorkspaceRegistry::at(directory.path().join("workspaces.json"))
                    .expect("registry path"),
            ),
            _registry_directory: Some(directory),
            fail_index_completion: false,
        }
    }

    #[cfg(test)]
    fn registry(&self) -> Result<WorkspaceRegistry, EngineError> {
        self.registry
            .clone()
            .ok_or_else(|| EngineError::internal("test registry missing"))
    }

    fn optional_registry(&self) -> Option<WorkspaceRegistry> {
        self.registry.clone().or_else(|| {
            match WorkspaceRegistry::available_global() {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(%error, "workspace registry is unavailable; using workspace metadata");
                    None
                }
            }
        })
    }

    fn remember_workspace(&self, manifest: &WorkspaceManifest) {
        if let Some(registry) = self.optional_registry()
            && let Err(error) = registry.remember(
                &manifest.workspace.name,
                &manifest.workspace.root,
                &manifest.path,
            )
        {
            tracing::warn!(%error, "index saved; optional workspace registry was not updated");
        }
    }

    fn forget_workspace(&self, root: &Path, home: &Path) -> bool {
        let Some(registry) = self.optional_registry() else {
            return false;
        };
        match registry.forget(root, home) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(%error, "optional workspace registry was not updated");
                false
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one ordered workspace transaction from validation through publication"
    )]
    pub(crate) async fn index(
        &self,
        models: &ModelRuntimeManager,
        mut options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        if let Some(name) = options.name.as_deref() {
            Workspace::validate_name(name)?;
        }
        normalize_model_paths(&mut options)?;
        let implicit_source = options.root.is_none();
        let location = crate::workspace::layout::index_location(
            options.root.as_deref(),
            options.index_dir.as_deref(),
        )?;
        crate::workspace::layout::validate_source_root(&location)?;
        if implicit_source && options.changes.is_empty() {
            options
                .changes
                .push(crate::api::index::options::WorkspaceChange::Rescan);
        }
        options.root = Some(location.root.clone());
        options.index_dir = Some(location.home.clone());
        crate::workspace::layout::ensure_workspace_home(&location)?;
        let _binding = crate::workspace::lock::bind_source(&location.home, &location.root)?;
        let home_lock = Arc::new(
            LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?
                .acquire(
                    &location.home,
                    LockMode::Write,
                    if options.rebuild {
                        "index.rebuild"
                    } else {
                        "index"
                    },
                )
                .await?,
        );
        // Check the on-disk version before any registry, recovery or storage mutation.
        let ignore_override = options
            .scan
            .ignore_files
            .as_deref()
            .or_else(|| options.reset_paths.then_some([].as_slice()));
        let existing = crate::workspace::manifest::inspect_workspace_manifest_with_ignore_files(
            &location,
            ignore_override,
        )?
        .into_manifest(options.rebuild)?;
        if !options.changes.is_empty()
            && existing
                .as_ref()
                .is_some_and(|manifest| manifest.workspace.root != location.root)
        {
            return Err(EngineError::resource_busy(
                "workspace source directory changed while an update was pending; reopen the index before refreshing",
            ));
        }
        if existing.is_none()
            && !options.rebuild
            && !has_build(&location.home)
            && has_index_storage(&location.home)?
        {
            return Err(EngineError::storage_failure(
                "index storage has no usable manifest; rebuild the index with `zg --index --rebuild`",
            ));
        }
        let scan = resolve_scan(existing.as_ref(), &options)?;
        crate::file_selection::GlobMatcher::new(&location.root, scan.globs())?;
        // Resolve all routes before any registry or storage mutation.
        let _ = embedding_plan(existing.as_ref(), &options)?;
        let abandoned = match read_build_registration(&location.home) {
            Ok(registration) => registration,
            Err(_) if options.rebuild => None,
            Err(error) => return Err(error),
        };
        // Retain only workspace naming/relocation information from a crashed
        // first build. Its model settings and computed data are never reused.
        let name = options
            .name
            .clone()
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|manifest| manifest.workspace.name.clone())
            })
            .or(abandoned)
            .unwrap_or_else(|| workspace_name(&location.root));
        if options.rebuild {
            recover_build_for_rebuild(&location.home)?;
        } else {
            recover_build(&location.home)?;
        }
        options.name = Some(name);
        let rebuilding = options.rebuild
            || existing
                .as_ref()
                .is_none_or(|manifest| !is_indexed(manifest));
        if options.reset_paths
            || options.scan != crate::api::index::options::ScanRulesUpdate::default()
        {
            // A changed selection can admit files outside a watcher's narrow change scope.
            options.changes.clear();
        }
        let IndexModels {
            runtimes: acquired,
            descriptor,
        } = acquire_index_models(models, existing.as_ref(), &options)?;
        if !rebuilding
            && let Some(previous) = existing
                .as_ref()
                .and_then(|manifest| manifest.workspace.index.descriptor())
        {
            previous.ensure_index_compatible(&descriptor)?;
        }
        let mut manifest = index_manifest(
            &location,
            existing.as_ref(),
            &options,
            &acquired,
            descriptor,
        )?;
        let build = if rebuilding {
            // Fresh builds always cover the full configured workspace.
            options.changes.clear();
            let build = prepare_build(manifest)?;
            manifest = build.target.clone();
            Some(build)
        } else {
            None
        };
        let pending = build.clone();
        let registration = manifest.clone();
        let result = self
            .run_index(manifest, build, acquired, options, Arc::clone(&home_lock))
            .await;
        if result.is_err()
            && let Some(build) = pending
        {
            // Storage handles have been released. Preserve the original error if
            // cleanup also fails; its build record lets the next writer retry.
            let _ = discard_build(&build);
        }
        if result.is_ok() {
            self.remember_workspace(&registration);
        }
        result
    }

    async fn run_index(
        &self,
        mut manifest: WorkspaceManifest,
        build: Option<WorkspaceBuild>,
        models: Vec<ModelRuntimeLease>,
        options: IndexOptions,
        home_lock: Arc<FileLock>,
    ) -> Result<IndexResult, EngineError> {
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: build
                .as_ref()
                .map_or_else(|| manifest.index_path(), WorkspaceBuild::index_path),
            tables: manifest
                .workspace
                .index
                .descriptor()
                .expect("enabled index")
                .content_indexes(),
        })?;
        let writer = self.writers.register(
            crate::pipelines::indexed_search::writer::WriterSession::new(
                storage,
                models,
                manifest.clone(),
                home_lock,
            ),
            build.is_none(),
        );
        let embedding_models = writer
            .session
            .models
            .iter()
            .map(|model| model as &dyn IndexEmbeddingRuntime)
            .collect::<Vec<_>>();
        let result = index_workspace(&IndexingContext {
            workspace_index: &manifest.workspace,
            index_home: Some(&manifest.path),
            storage: &writer.session.storage,
            scanner: &self.scanner,
            embedding_models: &embedding_models,
            embedding_concurrency: options.embedding_concurrency,
            on_progress: options.on_progress,
            signal: options.signal.clone(),
            changes: &options.changes,
        })
        .await;
        #[cfg(test)]
        let result = result.and_then(|indexed| {
            // Exercise service cleanup when the indexing pipeline reports a failed checkpoint.
            if self.fail_index_completion {
                Err(EngineError::storage_failure("injected checkpoint failure"))
            } else {
                Ok(indexed)
            }
        });
        // Closing precedes publication: all checkpoints and native handles belong
        // to the completed generation before the active manifest can select it.
        writer.retire().await;
        let close_result = writer.session.storage.close();
        let indexed = result?;
        close_result?;
        if options
            .signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(EngineError::cancelled(
                "indexing was cancelled before publication",
            ));
        }
        let now = epoch_millis();
        if let Some(build) = build {
            publish_build(build, now)?;
        } else {
            manifest.record_update(now);
            write_workspace_manifest(&manifest.path, &manifest)?;
        }
        Ok(indexed)
    }

    pub(in crate::pipelines) async fn workspace_needs_refresh(
        &self,
        location: &WorkspaceIndexLocation,
        wait: &LockWait<'_>,
    ) -> Result<bool, EngineError> {
        let _lock = wait
            .acquire(&location.home, LockMode::Read, "context.refresh")
            .await?;
        let Some(manifest) = read_workspace_manifest(location)? else {
            return Ok(false);
        };
        if !is_indexed(&manifest) || manifest.workspace.index == IndexState::Disabled {
            return Ok(false);
        }
        // Metadata drift must reach the writer even when the bytes are unchanged.
        let status = self.read_status(&manifest, false).await?;
        Ok(status.files_added > 0
            || status.files_modified > 0
            || status.files_deleted > 0
            || status.files_pending > 0
            || status.files_failed > 0)
    }

    async fn read_status(
        &self,
        manifest: &WorkspaceManifest,
        detailed: bool,
    ) -> Result<crate::api::info::result::IndexStats, EngineError> {
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: manifest.index_path(),
        })?;
        storage.ensure_compatible(
            &manifest
                .workspace
                .index
                .descriptor()
                .expect("enabled index")
                .content_indexes(),
        )?;
        let status = get_workspace_index_status(
            &manifest.workspace,
            Some(&manifest.path),
            &storage,
            &self.scanner,
            None,
            detailed,
        )
        .await;
        let close = storage.close();
        let status = status?;
        close?;
        Ok(status)
    }

    pub(crate) async fn info(&self, options: InfoOptions) -> Result<InfoResult, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let Some(location) = find_nearest_workspace(&requested_root, options.index_dir.as_deref())?
        else {
            return Ok(unindexed_info(
                workspace_index_location(&requested_root, options.index_dir.as_deref())?,
                WorkspaceIndexPolicy::Uninitialized,
            ));
        };
        if !location.manifest_path.exists() && !has_build(&location.home) {
            let orphaned = has_index_storage(&location.home)?;
            let info = unindexed_info(location, WorkspaceIndexPolicy::Uninitialized);
            if orphaned {
                return Ok(rebuild_required_info(
                    info,
                    None,
                    "index storage has no usable manifest".into(),
                ));
            }
            return Ok(info);
        }
        crate::workspace::layout::validate_storage_paths(&location.home)?;
        let _lock = LockWait::new(None, None)?
            .acquire(&location.home, LockMode::Read, "info")
            .await?;
        let _binding = crate::workspace::lock::bind_source(&location.home, &location.root)?;
        let manifest = match inspect_workspace_manifest(&location)? {
            ManifestState::Current(manifest) => manifest,
            ManifestState::Missing => {
                return Ok(unindexed_info(
                    location,
                    WorkspaceIndexPolicy::Uninitialized,
                ));
            }
            ManifestState::RebuildRequired {
                actual_version,
                reason,
            } => {
                return Ok(rebuild_required_info(
                    unindexed_info(location, WorkspaceIndexPolicy::Enabled),
                    actual_version,
                    reason,
                ));
            }
        };
        if manifest.workspace.root != location.root {
            return Err(EngineError::resource_busy(
                "workspace source directory changed; retry workspace inspection",
            ));
        }
        let metadata_indexed = is_indexed(&manifest);
        let storage_exists = IndexStore::exists(&manifest.index_path())?;
        let indexed = metadata_indexed && storage_exists;
        let source_available = location.root.is_dir();
        let status = if options.include_status && indexed && source_available {
            Some(self.read_status(&manifest, true).await?)
        } else {
            None
        };

        Ok(InfoResult {
            compatibility: if metadata_indexed {
                IndexCompatibility::Compatible {
                    version: CURRENT_INDEX_VERSION,
                }
            } else {
                IndexCompatibility::Unbuilt
            },
            root: location.root,
            indexed,
            index_policy: (&manifest.workspace.index).into(),
            home: location.home,
            index_path: manifest.index_path(),
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(workspace_info(&manifest)),
            status,
            suggestion: if source_available {
                workspace_suggestion(&manifest, indexed)
            } else {
                Some("source root is unavailable; specify the current source root to scan or update this index".into())
            },
        })
    }

    pub(crate) async fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let location = find_nearest_workspace(&requested_root, options.index_dir.as_deref())?
            .map_or_else(
                || workspace_index_location(&requested_root, options.index_dir.as_deref()),
                Ok,
            )?;
        let has_data = workspace_has_index_data(&location)?;
        if !has_data {
            return Ok(self.forget_workspace(&location.root, &location.home));
        }
        crate::workspace::layout::ensure_workspace_home(&location)?;
        let _lock = LockWait::new(None, None)?
            .acquire(&location.home, LockMode::Write, "index.drop")
            .await?;
        let _binding = crate::workspace::lock::bind_source(&location.home, &location.root)?;
        reset_workspace_index(&location)?;
        self.forget_workspace(&location.root, &location.home);
        Ok(true)
    }
}

fn rebuild_required_info(
    mut info: InfoResult,
    actual_version: Option<u32>,
    reason: String,
) -> InfoResult {
    info.compatibility = IndexCompatibility::RebuildRequired {
        actual_version,
        expected_version: CURRENT_INDEX_VERSION,
        reason,
    };
    info.suggestion = Some("rebuild the index with `zg --index --rebuild`".into());
    info
}

fn workspace_has_index_data(location: &WorkspaceIndexLocation) -> Result<bool, EngineError> {
    Ok(location.manifest_path.exists()
        || has_build(&location.home)
        || location.home.join("files.zvec").exists()
        || location.home.join("index.zvec").exists()
        || has_index_storage(&location.home)?)
}

fn index_manifest(
    location: &WorkspaceIndexLocation,
    active: Option<&WorkspaceManifest>,
    options: &IndexOptions,
    models: &[ModelRuntimeLease],
    descriptor: IndexDescriptor,
) -> Result<WorkspaceManifest, EngineError> {
    let now = epoch_millis();
    let workspace = Workspace {
        name: options
            .name
            .clone()
            .or_else(|| active.map(|value| value.workspace.name.clone()))
            .unwrap_or_else(|| workspace_name(&location.root)),
        root: location.root.clone(),
        scan: resolve_scan(active, options)?,
        index: IndexState::Enabled(Box::new(descriptor)),
        created_epoch_ms: active.map_or(now, |value| value.workspace.created_epoch_ms),
        updated_epoch_ms: now,
    };
    let plan = embedding_plan(active, options)?;
    let runtime = models
        .iter()
        .map(|model| {
            Ok((
                model.info().model().reference(),
                embedding_runtime(
                    active,
                    &plan.requests[&model.info().model().reference()],
                    model,
                )?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, EngineError>>()?;
    let mut manifest = WorkspaceManifest::new(
        workspace,
        location.home.clone(),
        Some(CURRENT_INDEX_VERSION),
        runtime,
    )?;
    manifest.storage_generation = active.and_then(|value| value.storage_generation.clone());
    Ok(manifest)
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

fn normalize_model_paths(options: &mut IndexOptions) -> Result<(), EngineError> {
    for spec in options
        .embedding
        .iter_mut()
        .chain(options.embedding_routes.values_mut())
    {
        if let Some(cache_dir) = spec.cache_dir.as_mut() {
            *cache_dir = std::path::absolute(&*cache_dir).map_err(|error| {
                EngineError::from_io("failed to resolve model cache directory", &error)
            })?;
        }
    }
    Ok(())
}

/// Concrete per-model requests shared by execution and destination preflight.
pub(crate) struct EmbeddingPlan {
    pub default_ref: String,
    pub routes: BTreeMap<crate::domain::ContentKind, String>,
    pub requests: BTreeMap<String, IndexOptions>,
}

pub(crate) fn embedding_plan(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Result<EmbeddingPlan, EngineError> {
    let default_ref = embedding_reference(existing, options.embedding.as_ref())?;
    let mut routes = existing
        .and_then(|manifest| manifest.workspace.index.descriptor())
        .map_or_else(BTreeMap::new, |index| {
            index
                .embedding_routes()
                .iter()
                .map(|(kind, model)| (*kind, model.model().reference()))
                .collect()
        });
    for kind in &options.clear_embedding_routes {
        if options.embedding_routes.contains_key(kind) {
            return Err(EngineError::invalid_argument(format!(
                "cannot set and clear the {} embedding route in one request",
                kind.as_str(),
            )));
        }
        routes.remove(kind);
    }
    let mut specifications = BTreeMap::new();
    if let Some(spec) = &options.embedding {
        specifications.insert(spec.reference.clone(), spec.clone());
    }
    for (kind, spec) in &options.embedding_routes {
        if specifications
            .get(&spec.reference)
            .is_some_and(|previous| previous != spec)
        {
            return Err(EngineError::invalid_argument(format!(
                "conflicting configurations for embedding model {}",
                spec.reference,
            )));
        }
        specifications.insert(spec.reference.clone(), spec.clone());
        routes.insert(*kind, spec.reference.clone());
    }
    for (kind, reference) in &routes {
        let entry =
            crate::models::get_embedding_model_catalog_entry(reference).ok_or_else(|| {
                EngineError::invalid_argument(format!("unknown embedding model: {reference}"))
            })?;
        if !entry.model_info()?.supports_content(*kind) {
            return Err(EngineError::invalid_argument(format!(
                "embedding model {reference} does not support {} content",
                kind.as_str(),
            )));
        }
    }
    let mut requests = BTreeMap::new();
    for reference in std::iter::once(&default_ref).chain(routes.values()) {
        if requests.contains_key(reference) {
            continue;
        }
        if crate::models::get_embedding_model_catalog_entry(reference).is_none() {
            return Err(EngineError::invalid_argument(format!(
                "unknown embedding model: {reference}"
            )));
        }
        let spec = specifications
            .get(reference)
            .cloned()
            .unwrap_or_else(|| EmbeddingModelSpec {
                reference: reference.clone(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Auto,
            });
        if spec.revision.is_some() {
            return Err(EngineError::unsupported(
                "embedding revision overrides are not supported by the catalog-backed runtime",
            ));
        }
        let mut request = options.clone();
        request.embedding = Some(spec);
        request.embedding_routes.clear();
        request.clear_embedding_routes.clear();
        if reference != &default_ref {
            request.api_key = None;
            request.endpoint = None;
            if !reference.starts_with("local/") {
                request.device = None;
            }
        }
        requests.insert(reference.clone(), request);
    }
    Ok(EmbeddingPlan {
        default_ref,
        routes,
        requests,
    })
}

struct IndexModels {
    runtimes: Vec<ModelRuntimeLease>,
    descriptor: IndexDescriptor,
}

fn acquire_index_models(
    models: &ModelRuntimeManager,
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Result<IndexModels, EngineError> {
    let plan = embedding_plan(existing, options)?;
    let runtimes = plan
        .requests
        .values()
        .map(|request| acquire_model(models, existing, request))
        .collect::<Result<Vec<_>, _>>()?;
    let info_for = |reference: &str| {
        runtimes
            .iter()
            .find(|runtime| runtime.info().model().reference() == reference)
            .map(|runtime| runtime.info().clone())
            .ok_or_else(|| EngineError::internal(format!("missing embedding runtime {reference}")))
    };
    let descriptor = IndexDescriptor::new(
        info_for(&plan.default_ref)?,
        plan.routes
            .into_iter()
            .map(|(kind, reference)| Ok((kind, info_for(&reference)?)))
            .collect::<Result<_, EngineError>>()?,
        crate::domain::FTS_CONFIG,
    )?;
    Ok(IndexModels {
        runtimes,
        descriptor,
    })
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
    let concurrency = resolve_index_concurrency(&reference, options.embedding_concurrency)
        .map_err(ModelError::into_engine_error)?;
    let config = crate::config::read()?;
    let local = reference.starts_with("local/");
    if (!local && options.device.is_some()) || (local && options.endpoint.is_some()) {
        return Err(EngineError::invalid_argument(
            "device requires a local model; endpoint requires a remote model",
        ));
    }
    let existing_runtime =
        existing.and_then(|manifest| manifest.embedding_runtimes.get(&reference));
    let api_key = if local {
        None
    } else {
        workspace_api_key(existing, &reference, options.api_key.as_deref())?
            .or_else(|| {
                crate::config::string(
                    &config,
                    &[
                        "providers",
                        reference.split('/').next().unwrap_or_default(),
                        "apiKey",
                    ],
                )
            })
            .or_else(environment_api_key)
    };
    let endpoint = options.endpoint.clone().or_else(|| {
        options
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.endpoint.clone())
            .or_else(|| existing_runtime.and_then(|runtime| runtime.endpoint.clone()))
    });
    let endpoint = if local {
        endpoint
    } else {
        let endpoint = crate::authorization::remote_endpoint(&reference, endpoint.as_deref())?;
        let root = resolve_root(options.root.as_deref())?;
        crate::authorization::require_with_targets(
            &root,
            options.index_dir.as_deref(),
            &reference,
            &endpoint,
            options.allow_remote,
            &options.authorized_remote,
        )?;
        Some(endpoint)
    };
    let device = if local {
        crate::config::runtime_device(
            &config,
            &reference,
            options.runtime_device.or(options.device).or_else(|| {
                options
                    .embedding
                    .as_ref()
                    .map(|e| e.device)
                    .filter(|device| *device != Device::Auto)
            }),
        )?
    } else {
        None
    };
    models
        .acquire(ModelRuntimeRequest::new(
            reference.clone(),
            ModelConfig {
                api_key,
                endpoint,
                cache_dir: crate::config::model_cache(
                    &config,
                    options
                        .model_cache
                        .clone()
                        .or_else(|| options.embedding.as_ref().and_then(|e| e.cache_dir.clone())),
                ),
                device,
            },
            concurrency,
        ))
        .map_err(ModelError::into_engine_error)
}

pub(crate) fn embedding_reference(
    existing: Option<&WorkspaceManifest>,
    requested: Option<&EmbeddingModelSpec>,
) -> Result<String, EngineError> {
    let config = crate::config::read()?;
    resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit: requested.map(|embedding| embedding.reference.clone()),
        existing: existing
            .and_then(|manifest| manifest.default_embedding())
            .map(|embedding| embedding.model().reference()),
        global_default: crate::config::string(&config, &["defaults", "embedding"]),
        fallback: Some(DEFAULT_LOCAL_EMBEDDING.to_owned()),
        ..ResolveEmbeddingReferenceOptions::default()
    })
    .map_err(ModelError::into_engine_error)?
    .ok_or_else(|| EngineError::internal("default embedding model is not configured"))
}

pub(in crate::pipelines) fn workspace_api_key(
    manifest: Option<&WorkspaceManifest>,
    reference: &str,
    explicit: Option<&str>,
) -> Result<Option<String>, EngineError> {
    if let Some(key) = explicit {
        return Ok(Some(key.to_owned()));
    }
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    manifest
        .embedding_runtimes
        .get(reference)
        .and_then(|runtime| runtime.api_key.as_ref())
        .map(|key| key.open(&manifest.path, reference))
        .transpose()
}

pub(in crate::pipelines) fn environment_api_key() -> Option<String> {
    ["ZVEC_GREP_API_KEY", "DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        .into_iter()
        .find_map(|name| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

pub(in crate::pipelines) fn assert_embedding_compatible(
    existing: Option<&WorkspaceManifest>,
    model: &ModelRuntimeLease,
) -> Result<(), EngineError> {
    let Some(existing) = existing.filter(|manifest| is_indexed(manifest)) else {
        return Ok(());
    };
    let schema = existing
        .embeddings()
        .into_iter()
        .find(|schema| schema.model().reference() == model.info().model().reference())
        .ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "embedding model {} is not part of the workspace index; rebuild the index",
                model.info().model().reference(),
            ))
        })?;
    schema.ensure_index_compatible(model.info())
}

fn resolve_scan(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Result<crate::domain::ScanRules, EngineError> {
    let scan = if options.reset_paths {
        crate::domain::ScanRules::default()
    } else {
        existing.map_or_else(crate::domain::ScanRules::default, |manifest| {
            manifest.workspace.scan.clone()
        })
    };
    options.scan.apply(&scan)
}

fn embedding_runtime(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
    model: &ModelRuntimeLease,
) -> Result<StoredModelConfig, EngineError> {
    let current = existing
        .and_then(|manifest| {
            manifest
                .embedding_runtimes
                .get(&model.info().model().reference())
                .cloned()
        })
        .unwrap_or_default();
    if model.info().model().provider() == "local" {
        Ok(StoredModelConfig::default())
    } else {
        Ok(StoredModelConfig {
            api_key: match options.api_key.as_deref() {
                Some(key) => Some(EncryptedApiKey::seal(
                    options
                        .index_dir
                        .as_deref()
                        .ok_or_else(|| EngineError::internal("index location was not resolved"))?,
                    &model.info().model().reference(),
                    key,
                    existing.is_none_or(|manifest| {
                        manifest
                            .embedding_runtimes
                            .values()
                            .all(|runtime| runtime.api_key.is_none())
                    }),
                )?),
                None => current.api_key,
            },
            endpoint: model
                .configured_endpoint()
                .map(str::to_owned)
                .or(current.endpoint),
        })
    }
}

pub(in crate::pipelines) fn is_indexed(manifest: &WorkspaceManifest) -> bool {
    manifest.workspace.index_enabled() && manifest.index_version.is_some()
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

fn workspace_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

fn unindexed_info(location: WorkspaceIndexLocation, policy: WorkspaceIndexPolicy) -> InfoResult {
    InfoResult {
        compatibility: IndexCompatibility::Unbuilt,
        root: location.root,
        indexed: false,
        index_policy: policy,
        home: location.home,
        index_path: location.index_path,
        source: InfoSource::Unindexed,
        workspace_index: None,
        status: None,
        suggestion: Some("run zg --index to create a workspace index".to_owned()),
    }
}

fn workspace_suggestion(manifest: &WorkspaceManifest, indexed: bool) -> Option<String> {
    if manifest.workspace.index == IndexState::Disabled {
        Some("indexing is disabled for this workspace".to_owned())
    } else if !indexed {
        Some("workspace manifest exists but index storage is missing".to_owned())
    } else {
        None
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn workspace_info(manifest: &WorkspaceManifest) -> WorkspaceIndexInfo {
    WorkspaceIndexInfo::from_workspace(&manifest.workspace, &manifest.path, manifest.index_version)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use crate::{
        api::{
            index::{
                IndexOptions,
                options::{EmbeddingModelSpec, ScanRulesUpdate},
            },
            info::InfoOptions,
        },
        domain::{IndexState, Workspace, model::Device},
    };

    use super::{ModelRuntimeManager, WorkspaceIndexService};

    fn read_manifest(
        home: &std::path::Path,
    ) -> crate::EngineResult<Option<super::WorkspaceManifest>> {
        let location =
            super::workspace_index_location(home.parent().expect("source root"), Some(home))?;
        super::read_workspace_manifest(&location)
    }

    fn write_previous_index_version(
        home: &std::path::Path,
        manifest: &crate::workspace::manifest::WorkspaceManifest,
    ) {
        let mut previous = manifest.clone();
        previous.index_version = Some(super::CURRENT_INDEX_VERSION - 1);
        std::fs::write(
            home.join("manifest.json"),
            serde_json::to_vec(&previous).expect("previous manifest JSON"),
        )
        .expect("previous index metadata");
    }

    #[tokio::test]
    async fn inspection_rejects_source_changed_while_waiting_for_the_read_lock() {
        let directory = tempdir().expect("workspace package");
        let parent = directory.path().canonicalize().expect("parent");
        let root = parent.join("materials");
        let replacement = parent.join("replacement");
        let home = parent.join("index");
        std::fs::create_dir(&root).expect("source");
        std::fs::create_dir(&replacement).expect("replacement source");
        let mut manifest = crate::workspace::manifest::WorkspaceManifest::new(
            Workspace {
                name: "fixture".into(),
                root: root.clone(),
                scan: crate::domain::ScanRules::default(),
                index: IndexState::Uninitialized,
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            },
            home.clone(),
            None,
            std::collections::BTreeMap::new(),
        )
        .expect("manifest");
        super::write_workspace_manifest(&home, &manifest).expect("initial manifest");
        let write_lock = crate::workspace::lock::acquire_home_lock(
            &home,
            super::LockMode::Write,
            "source change fixture",
        )
        .expect("writer");
        let service = WorkspaceIndexService::with_test_registry();
        let inspection = service.info(InfoOptions {
            root: Some(root),
            index_dir: Some(home.clone()),
            include_status: false,
        });
        tokio::pin!(inspection);
        assert!(futures_util::poll!(&mut inspection).is_pending());
        manifest.workspace.root = replacement;
        super::write_workspace_manifest(&home, &manifest).expect("publish source change");
        drop(write_lock);
        let error = inspection
            .await
            .expect_err("mixed source metadata is rejected");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_BUSY);
        assert!(error.message().contains("retry workspace inspection"));
    }

    #[test]
    fn rejects_incompatible_index_versions() {
        crate::workspace::manifest::require_current_index_version(Some(
            super::CURRENT_INDEX_VERSION,
        ))
        .expect("current index");
        for version in [
            None,
            Some(0),
            Some(1),
            Some(super::CURRENT_INDEX_VERSION - 1),
            Some(super::CURRENT_INDEX_VERSION + 1),
            Some(u32::MAX),
        ] {
            let error = crate::workspace::manifest::require_current_index_version(version)
                .expect_err("incompatible index format");
            assert!(error.message().contains("rebuild the index"));
        }
    }

    #[test]
    fn selection_updates_preserve_omitted_settings_and_clear_explicit_values() {
        let directory = tempdir().expect("workspace");
        let manifest = crate::workspace::manifest::WorkspaceManifest::new(
            Workspace {
                name: "workspace".into(),
                root: directory.path().to_path_buf(),
                scan: ScanRulesUpdate {
                    globs: Some(vec!["*.rs".into()]),
                    include_hidden: Some(true),
                    max_depth: Some(Some(3)),
                    ..ScanRulesUpdate::default()
                }
                .apply(&crate::domain::ScanRules::default())
                .expect("scan rules"),
                index: IndexState::Uninitialized,
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            },
            directory.path().join(".zvec-grep"),
            None,
            std::collections::BTreeMap::new(),
        )
        .expect("manifest");
        let mut options = IndexOptions::default();
        assert_eq!(
            super::resolve_scan(Some(&manifest), &options).expect("scan rules"),
            manifest.workspace.scan
        );
        options.scan.globs = Some(Vec::new());
        options.scan.include_hidden = Some(false);
        options.scan.max_depth = Some(None);
        let scan = super::resolve_scan(Some(&manifest), &options).expect("scan rules");
        assert!(scan.globs().is_empty());
        assert!(!scan.include_hidden());
        assert_eq!(scan.max_depth(), None);
        options = IndexOptions {
            reset_paths: true,
            ..IndexOptions::default()
        };
        assert_eq!(
            super::resolve_scan(Some(&manifest), &options).expect("scan rules"),
            crate::domain::ScanRules::default()
        );
    }

    #[tokio::test]
    async fn a_file_cannot_be_a_workspace_root() {
        let directory = tempdir().expect("workspace");
        let file = directory.path().join("file.txt");
        std::fs::write(&file, "text").expect("source file");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(file),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect_err("workspace requires a directory");
        assert!(error.message().contains("existing directory"));
        assert_eq!(models.snapshot().cached_runtimes, 0);
    }

    #[tokio::test]
    async fn remote_endpoint_changes_reuse_the_index_and_persist_for_reopen() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let service = WorkspaceIndexService::with_test_registry();
        let mut models = ModelRuntimeManager::new();
        let reference = "qwen/text-embedding-v4";
        let mut options = IndexOptions {
            root: Some(directory.path().to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: reference.into(),
                revision: None,
                cache_dir: None,
                endpoint: Some(" https://first.example.test/v1 ".into()),
                device: Device::Auto,
            }),
            api_key: Some("fixture".into()),
            allow_remote: true,
            ..IndexOptions::default()
        };
        service
            .index(&models, options.clone())
            .await
            .expect("empty workspace requires no remote embedding requests");
        let initial = read_manifest(&home).expect("read").expect("manifest");
        assert_eq!(
            initial.embedding_runtimes[reference].endpoint.as_deref(),
            Some("https://first.example.test/v1/embeddings")
        );

        options.endpoint = Some("https://second.example.test/v1".into());
        service
            .index(&models, options.clone())
            .await
            .expect("another endpoint for the same model reuses the index");
        let updated = read_manifest(&home)
            .expect("read")
            .expect("updated manifest");
        assert_eq!(updated.workspace.index, initial.workspace.index);
        assert_eq!(updated.storage_generation, initial.storage_generation);
        assert_eq!(
            updated.embedding_runtimes[reference].endpoint.as_deref(),
            Some("https://second.example.test/v1/embeddings")
        );

        options.embedding = None;
        options.endpoint = None;
        models.close();
        models = ModelRuntimeManager::new();
        options.api_key = None;
        service
            .index(&models, options.clone())
            .await
            .expect("reopen uses the persisted endpoint");
        let reopened = read_manifest(&home)
            .expect("read")
            .expect("reopened manifest");
        assert_eq!(reopened.workspace.index, updated.workspace.index);
        assert_eq!(reopened.storage_generation, initial.storage_generation);
        assert_eq!(reopened.embedding_runtimes, updated.embedding_runtimes);

        let saved = std::fs::read(home.join("manifest.json")).expect("saved manifest");
        options.endpoint = Some("not a URL".into());
        let error = service
            .index(&models, options)
            .await
            .expect_err("invalid endpoint must not overwrite saved configuration");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            std::fs::read(home.join("manifest.json")).expect("unchanged manifest"),
            saved
        );
        models.close();
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one credential lifecycle across publication, failure, move and tampering"
    )]
    async fn saved_credentials_survive_reopen_move_and_failed_replacement() {
        let directory = tempdir().expect("workspace");
        let root = directory.path().join("original");
        std::fs::create_dir(&root).expect("source root");
        let mut service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let reference = "qwen/text-embedding-v4";
        let options = IndexOptions {
            api_key: Some("first-test-secret".into()),
            endpoint: Some("https://example.test/embeddings".into()),
            allow_remote: true,
            embedding: Some(EmbeddingModelSpec {
                reference: reference.into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Auto,
            }),
            root: Some(root.clone()),
            ..Default::default()
        };
        service
            .index(&models, options.clone())
            .await
            .expect("save key");
        let home = root.join(".zvec-grep");
        let read = || read_manifest(&home).expect("read").expect("manifest");
        let first = read();
        assert_eq!(
            super::workspace_api_key(Some(&first), reference, None)
                .expect("decrypt")
                .as_deref(),
            Some("first-test-secret")
        );
        let bytes = std::fs::read_to_string(home.join("manifest.json")).expect("disk data");
        assert!(!bytes.contains("first-test-secret"));
        assert!(bytes.contains("ciphertext"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("credentials.key"))
                    .expect("key")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let key_path = home.join("credentials.key");
        let key_bytes = std::fs::read(&key_path).expect("encryption key");
        std::fs::remove_file(&key_path).expect("simulate incomplete copy");
        let error = service
            .index(&models, options.clone())
            .await
            .expect_err("must not replace a missing key when saved credentials exist");
        assert!(error.message().contains("credentials.key"));
        assert!(!key_path.exists());
        assert_eq!(read(), first);
        std::fs::write(&key_path, key_bytes).expect("restore encryption key");

        service.fail_index_completion = true;
        let failed = service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    api_key: Some("failed-test-secret".into()),
                    ..options.clone()
                },
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(
            read(),
            first,
            "failed build cannot replace saved credentials"
        );
        service.fail_index_completion = false;
        service
            .index(
                &models,
                IndexOptions {
                    api_key: Some("second-test-secret".into()),
                    ..options
                },
            )
            .await
            .expect("replace key");
        let second = read();
        assert_ne!(second.embedding_runtimes, first.embedding_runtimes);
        assert_eq!(
            super::workspace_api_key(Some(&second), reference, None)
                .expect("decrypt")
                .as_deref(),
            Some("second-test-secret")
        );
        models.close();
        let moved = directory.path().join("moved");
        std::fs::rename(&root, &moved).expect("move workspace");
        let reopened = read_manifest(&moved.join(".zvec-grep"))
            .expect("read moved")
            .expect("manifest");
        assert_eq!(
            super::workspace_api_key(Some(&reopened), reference, None)
                .expect("decrypt moved key")
                .as_deref(),
            Some("second-test-secret")
        );
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    allow_remote: true,
                    ..Default::default()
                },
            )
            .await
            .expect("reuse saved credentials");
        let mut tampered = serde_json::to_value(&reopened).expect("manifest JSON");
        tampered["embeddingRuntimes"][reference]["apiKey"]["ciphertext"] =
            serde_json::json!("AAAAAAAAAAAAAAAAAAAAAA==");
        std::fs::write(
            moved.join(".zvec-grep/manifest.json"),
            serde_json::to_vec(&tampered).expect("tampered JSON"),
        )
        .expect("tamper");
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    allow_remote: true,
                    ..Default::default()
                },
            )
            .await
            .expect_err("authenticated encryption detects tampering");
        assert!(error.message().contains(reference));
        assert!(!error.message().contains("second-test-secret"));
        service
            .drop_index(&InfoOptions {
                index_dir: None,
                root: Some(moved.clone()),
                include_status: false,
            })
            .await
            .expect("drop");
        assert!(!moved.join(".zvec-grep/credentials.key").exists());
        models.close();
    }

    #[tokio::test]
    async fn reopening_a_moved_workspace_uses_its_current_root() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&original).expect("workspace root");
        std::fs::write(original.join(".ignore"), "ignored.rs\n").expect("ignore file");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(original.clone()),
                    model_cache: Some(original.join(".zvec-grep/models")),
                    scan: ScanRulesUpdate {
                        globs: Some(vec!["*.rs".into()]),
                        ignore_files: Some(vec![original.join(".ignore")]),
                        ..ScanRulesUpdate::default()
                    },
                    embedding: Some(EmbeddingModelSpec {
                        reference: "local/potion-code-16m-v2".into(),
                        revision: None,
                        cache_dir: None,
                        endpoint: None,
                        device: Device::Cpu,
                    }),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("empty workspace index");
        let before = read_manifest(&original.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        std::fs::rename(&original, &moved).expect("move workspace");
        let info = service
            .info(InfoOptions {
                index_dir: None,
                root: Some(moved.clone()),
                include_status: true,
            })
            .await
            .expect("moved workspace info");
        let workspace = info.workspace_index.expect("workspace index");
        assert_eq!(workspace.name, before.workspace.name.as_str());
        assert_eq!(
            workspace.root,
            std::fs::canonicalize(&moved).expect("moved root")
        );
        assert_eq!(workspace.path, workspace.root.join(".zvec-grep"));
        assert_eq!(
            workspace.scan.globs(),
            vec![crate::domain::GlobRule::from("*.rs")]
        );
        assert_eq!(info.status.expect("status").files_scanned, 0);
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("update moved workspace");
        let after = read_manifest(&moved.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(after.workspace.name, before.workspace.name);
        assert_eq!(after.workspace.scan.globs(), before.workspace.scan.globs());
        assert_eq!(
            after.workspace.scan.ignore_files(),
            [std::path::PathBuf::from(".ignore")]
        );
        assert_eq!(
            after.embedding_runtimes["local/potion-code-16m-v2"],
            super::StoredModelConfig::default()
        );
        assert_eq!(
            after.workspace.root,
            std::fs::canonicalize(&moved).expect("canonical source")
        );
        assert_eq!(
            service
                .registry()
                .expect("registry")
                .root_for_name(&before.workspace.name)
                .expect("registered move"),
            Some(std::fs::canonicalize(&moved).expect("moved root"))
        );
        models.close();
    }

    #[tokio::test]
    async fn dropping_a_missing_index_is_an_idempotent_no_op() {
        let directory = tempdir().expect("temporary directory");

        assert!(
            !WorkspaceIndexService::with_test_registry()
                .drop_index(&InfoOptions {
                    index_dir: None,
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .await
                .expect("missing index should be an idempotent no-op")
        );
    }

    #[tokio::test]
    async fn dropping_orphaned_generation_storage_does_not_require_a_manifest() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let storage = home.join("index");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("real index");
        models.close();
        std::fs::remove_file(home.join("manifest.json")).expect("orphaned storage");
        let options = InfoOptions {
            index_dir: None,
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("temp/build.json").exists());
        assert!(
            service
                .drop_index(&options)
                .await
                .expect("drop orphaned storage")
        );
        assert!(!storage.exists());
        assert!(!home.join("index").exists());
        assert!(
            !service
                .drop_index(&options)
                .await
                .expect("empty container is not an index")
        );
    }

    #[tokio::test]
    async fn cancellation_discards_stage_and_normal_index_keeps_active_storage() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = read_manifest(&home).expect("manifest").expect("active");
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    signal: Some(signal),
                    ..options.clone()
                },
            )
            .await
            .expect_err("cancelled rebuild");
        assert_eq!(
            read_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.index_path().exists());
        assert!(!super::has_build(&home));
        let refresh = crate::pipelines::indexed_search::service::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
            home.clone(),
        );
        service
            .index(&models, refresh)
            .await
            .expect("ordinary refresh updates active index");
        let updated = read_manifest(&home).expect("manifest").expect("active");
        assert_eq!(updated.storage_generation, active.storage_generation);
        assert_eq!(updated.workspace.name, active.workspace.name);
        assert!(active.index_path().exists());
        service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    ..options
                },
            )
            .await
            .expect("fresh rebuild");
        let rebuilt = read_manifest(&home).expect("manifest").expect("active");
        assert_ne!(rebuilt.storage_generation, active.storage_generation);
        assert_eq!(rebuilt.index_path(), active.index_path());
        assert!(rebuilt.index_path().exists());
        models.close();
    }

    #[tokio::test]
    async fn search_device_override_does_not_persist_in_manifest() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let manifest = read_manifest(&home).expect("read").expect("manifest");
        let request = crate::api::context::ContextOptions {
            device: Some(Device::Cpu),
            ..Default::default()
        };
        let options = crate::pipelines::indexed_search::service::refresh_options(
            &request,
            directory.path().to_path_buf(),
            home.clone(),
        );
        service.index(&models, options).await.expect("refresh");
        let refreshed = read_manifest(&home).expect("read").expect("manifest");
        assert_eq!(refreshed.embedding_runtimes, manifest.embedding_runtimes);
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    device: Some(Device::Cpu),
                    ..Default::default()
                },
            )
            .await
            .expect("explicit configuration update");
        let configured = read_manifest(&home).expect("read").expect("manifest");
        assert!(
            configured
                .embedding_runtimes
                .values()
                .all(|runtime| *runtime == super::StoredModelConfig::default())
        );
        models.close();
    }

    #[tokio::test]
    async fn abandoned_rebuild_does_not_change_normal_index_settings_or_storage() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = read_manifest(&home).expect("read").expect("active");
        let mut target = active.clone();
        let mut globs = target.workspace.scan.globs().to_vec();
        globs.push("*.rs".into());
        target.workspace.scan = ScanRulesUpdate {
            globs: Some(globs),
            ..ScanRulesUpdate::default()
        }
        .apply(&target.workspace.scan)
        .expect("scan rules");
        target
            .embedding_runtimes
            .values_mut()
            .next()
            .expect("runtime")
            .endpoint = Some("https://abandoned.test/embeddings".into());
        let abandoned = super::prepare_build(target).expect("crashed build");
        std::fs::write(abandoned.index_path().join("checkpoint"), "discard me")
            .expect("checkpoint");
        service
            .index(&models, options.clone())
            .await
            .expect("ordinary index uses active settings");
        let updated = read_manifest(&home).expect("read").expect("active");
        assert_eq!(updated.storage_generation, active.storage_generation);
        assert_eq!(updated.workspace.scan, active.workspace.scan);
        assert_eq!(updated.embedding_runtimes, active.embedding_runtimes);
        assert!(!abandoned.index_path().exists());
        assert!(!super::has_build(&home));
        models.close();
    }

    #[tokio::test]
    async fn explicit_rebuild_starts_over_even_after_recovering_a_publication() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = read_manifest(&home).expect("read").expect("active");
        let published = super::prepare_build(active.clone()).expect("build");
        std::fs::create_dir_all(published.index_path()).expect("completed storage");
        super::write_workspace_manifest(&home, &published.target)
            .expect("commit before simulated crash");
        service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    ..options
                },
            )
            .await
            .expect("fresh explicit rebuild");
        let rebuilt = read_manifest(&home).expect("read").expect("active");
        assert_ne!(
            rebuilt.storage_generation,
            published.target.storage_generation
        );
        assert!(active.index_path().exists());
        assert!(!published.index_path().exists());
        assert!(rebuilt.index_path().exists());
        assert!(!super::has_build(&home));
        models.close();
    }

    #[tokio::test]
    async fn failed_checkpoint_does_not_publish_an_incomplete_rebuild() {
        let directory = tempdir().expect("workspace");
        let mut service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = read_manifest(&home).expect("manifest").expect("active");
        service.fail_index_completion = true;
        let error = service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    ..options
                },
            )
            .await
            .expect_err("checkpoint fails");
        assert!(error.message().contains("checkpoint"));
        assert_eq!(
            read_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.index_path().exists());
        assert!(!super::has_build(&home));
        // The unchanged active index does not need a query-driven refresh.
        let location = super::workspace_index_location(directory.path(), None).expect("location");
        assert!(
            !service
                .workspace_needs_refresh(
                    &location,
                    &super::LockWait::new(None, None).expect("wait")
                )
                .await
                .expect("refresh decision")
        );
        assert!(
            service
                .drop_index(&InfoOptions {
                    index_dir: None,
                    root: Some(directory.path().to_path_buf()),
                    include_status: false
                })
                .await
                .expect("drop incomplete build")
        );
        assert!(!super::has_build(&home));
        assert!(!active.index_path().exists());
        models.close();
    }

    fn empty_index_options(root: &Path) -> IndexOptions {
        IndexOptions {
            root: Some(root.to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: "local/potion-code-16m-v2".into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Cpu,
            }),
            ..IndexOptions::default()
        }
    }

    async fn assert_nested_index_keeps_parent(model: &str, rebuild: bool) {
        let directory = tempdir().expect("nested workspaces");
        let root = std::fs::canonicalize(directory.path()).expect("parent root");
        let child = root.join("sub");
        std::fs::create_dir(root.join("docs")).expect("parent sources");
        std::fs::create_dir_all(child.join("src")).expect("child sources");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let mut parent_options = empty_index_options(&root);
        parent_options.embedding.as_mut().expect("model").reference =
            "local/potion-retrieval-32m".into();
        parent_options.scan.globs = Some(vec!["docs/**".into()]);
        service
            .index(&models, parent_options)
            .await
            .expect("parent index");
        let parent_home = root.join(".zvec-grep");
        let parent_bytes =
            std::fs::read(parent_home.join("manifest.json")).expect("parent manifest");
        let parent = read_manifest(&parent_home)
            .expect("manifest read")
            .expect("parent manifest");
        assert_eq!(
            service
                .info(InfoOptions {
                    index_dir: None,
                    root: Some(child.clone()),
                    include_status: false
                })
                .await
                .expect("ancestor info")
                .root,
            root
        );

        let mut options = empty_index_options(&child);
        options.embedding.as_mut().expect("model").reference = model.into();
        options.scan.globs = Some(vec!["src/**".into()]);
        options.rebuild = rebuild;
        service
            .index(&models, options)
            .await
            .expect("independent child index");
        let child_home = child.join(".zvec-grep");
        let indexed = read_manifest(&child_home)
            .expect("child manifest read")
            .expect("child owns its manifest");
        assert_eq!(indexed.workspace.root, child);
        assert_eq!(
            indexed
                .workspace
                .index
                .descriptor()
                .expect("child index")
                .model_for(crate::domain::ContentKind::Text)
                .expect("child text model")
                .model()
                .reference(),
            model
        );
        assert_eq!(
            indexed.workspace.scan.globs(),
            vec![crate::domain::GlobRule::from("src/**")]
        );
        assert_eq!(
            std::fs::read(parent_home.join("manifest.json")).expect("parent manifest"),
            parent_bytes
        );
        assert!(super::IndexStore::exists(&parent.index_path()).expect("parent storage remains"));
        assert_eq!(
            service
                .info(InfoOptions {
                    index_dir: None,
                    root: Some(child.clone()),
                    include_status: true
                })
                .await
                .expect("child info")
                .root,
            child
        );

        let mut incompatible = empty_index_options(&child);
        incompatible.embedding.as_mut().expect("model").reference =
            if model == "local/potion-retrieval-32m" {
                "local/potion-code-16m-v2".into()
            } else {
                "local/potion-retrieval-32m".into()
            };
        service
            .index(&models, incompatible)
            .await
            .expect_err("existing child model still requires rebuild");
        models.close();
    }

    #[tokio::test]
    async fn nested_index_with_same_model_keeps_parent() {
        assert_nested_index_keeps_parent("local/potion-retrieval-32m", false).await;
    }

    #[tokio::test]
    async fn nested_index_with_different_model_does_not_require_rebuild() {
        assert_nested_index_keeps_parent("local/potion-code-16m-v2", false).await;
    }

    #[tokio::test]
    async fn nested_index_rebuild_keeps_parent() {
        assert_nested_index_keeps_parent("local/potion-code-16m-v2", true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_lock_directory_is_rejected_before_writing_outside_the_home() {
        let directory = tempdir().expect("fixture");
        let root = directory.path().join("source");
        let home = directory.path().join("data");
        let outside = directory.path().join("outside");
        for path in [&root, &home, &outside] {
            std::fs::create_dir(path).expect("directory");
        }
        std::fs::write(home.join("manifest.json"), b"{}").expect("manifest");
        std::os::unix::fs::symlink(&outside, home.join("locks")).expect("linked locks");
        let service = WorkspaceIndexService::with_test_registry();
        let options = InfoOptions {
            root: Some(root.clone()),
            index_dir: Some(home.clone()),
            ..InfoOptions::default()
        };
        assert!(service.info(options.clone()).await.is_err());
        assert!(service.drop_index(&options).await.is_err());
        assert!(crate::workspace::lock::bind_source(&home, &root).is_err());
        assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0);
    }

    #[tokio::test]
    async fn explicit_drop_releases_a_deleted_source_directory_without_recreating_it() {
        let directory = tempdir().expect("workspace roots");
        let deleted = directory.path().join("deleted");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&deleted).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&deleted)
                },
            )
            .await
            .expect("original workspace owns name");
        std::fs::remove_dir_all(&deleted).expect("source directory deleted externally");
        let options = InfoOptions {
            index_dir: None,
            root: Some(deleted.clone()),
            include_status: false,
        };
        assert!(
            service
                .drop_index(&options)
                .await
                .expect("drop deleted workspace")
        );
        assert!(!deleted.exists());
        assert!(
            !service
                .drop_index(&options)
                .await
                .expect("idempotent cleanup")
        );
        let name = "shared".to_owned();
        let registry = service.registry().expect("registry");
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        service
            .index(
                &models,
                IndexOptions {
                    name: Some(name.clone()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("replacement workspace can reuse name");
        assert_eq!(
            registry.root_for_name(&name).expect("replacement owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    #[tokio::test]
    async fn dropping_a_moved_workspace_releases_its_old_registration_before_reindexing() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&original).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&original)
                },
            )
            .await
            .expect("original workspace owns name");
        std::fs::rename(&original, &moved).expect("move without reopening index");
        assert!(
            service
                .drop_index(&InfoOptions {
                    index_dir: None,
                    root: Some(moved.clone()),
                    include_status: false,
                })
                .await
                .expect("drop moved workspace")
        );
        let name = "shared".to_owned();
        let registry = service.registry().expect("registry");
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        assert!(!moved.join(".zvec-grep/manifest.json").exists());

        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("another root can reuse the dropped name");
        assert_eq!(
            registry.root_for_name(&name).expect("new owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    #[tokio::test]
    async fn invalid_names_are_rejected_before_workspace_mutation() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let registry = service.registry().expect("registry");
        for name in ["", " project", "project/child", "project\nchild"] {
            let error = service
                .index(
                    &models,
                    IndexOptions {
                        name: Some(name.to_owned()),
                        ..empty_index_options(directory.path())
                    },
                )
                .await
                .expect_err("invalid workspace name");
            assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
            assert!(error.message().contains("workspace name"));
            assert!(!directory.path().join(".zvec-grep").exists());
            assert_eq!(
                registry.name_for_root(directory.path()).expect("registry"),
                None
            );
        }
        assert_eq!(models.snapshot().cached_runtimes, 0);
    }

    #[tokio::test]
    async fn duplicate_names_and_unavailable_registry_do_not_prevent_workspace_operations() {
        let directory = tempdir().expect("roots");
        let mut service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        for child in ["one", "two"] {
            let root = directory.path().join(child);
            std::fs::create_dir(&root).expect("root");
            service
                .index(
                    &models,
                    IndexOptions {
                        name: Some("shared".into()),
                        ..empty_index_options(&root)
                    },
                )
                .await
                .expect("index");
            let info = service
                .info(InfoOptions {
                    index_dir: None,
                    root: Some(root.clone()),
                    include_status: true,
                })
                .await
                .expect("status");
            assert!(info.indexed);
            assert_eq!(info.workspace_index.expect("workspace").name, "shared");
            assert_eq!(
                service
                    .registry()
                    .expect("registry")
                    .name_for_root(&root)
                    .expect("cached name")
                    .as_deref(),
                Some("shared")
            );
        }
        let blocked = directory.path().join("not-a-directory");
        std::fs::write(&blocked, b"occupied").expect("unavailable registry parent");
        service.registry = Some(
            super::WorkspaceRegistry::at(blocked.join("workspaces.json")).expect("cache path"),
        );
        let root = directory.path().join("one");
        service
            .index(&models, empty_index_options(&root))
            .await
            .expect("optional registry cannot fail a published index");
        let options = InfoOptions {
            index_dir: None,
            root: Some(root),
            include_status: true,
        };
        assert!(service.info(options.clone()).await.expect("status").indexed);
        assert!(service.drop_index(&options).await.expect("drop"));
        assert_eq!(
            std::fs::read(blocked).expect("unchanged parent"),
            b"occupied"
        );
        models.close();
    }

    #[tokio::test]
    async fn explicit_rename_preserves_generation_storage() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let before = read_manifest(&home)
            .expect("manifest")
            .expect("active workspace");
        let marker = before.index_path().join("checkpoint-marker");
        std::fs::write(&marker, b"existing generation").expect("generation storage marker");
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("after".into()),
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("rename workspace");
        let after = read_manifest(&home)
            .expect("manifest")
            .expect("renamed workspace");
        assert_eq!(after.workspace.name.as_str(), "after");
        assert_eq!(after.storage_generation, before.storage_generation);
        assert_eq!(
            after.workspace.created_epoch_ms,
            before.workspace.created_epoch_ms
        );
        assert_eq!(
            std::fs::read(&marker).expect("retained generation storage"),
            b"existing generation"
        );
        let info = service
            .info(InfoOptions {
                index_dir: None,
                root: Some(directory.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("renamed info");
        assert_eq!(info.workspace_index.expect("workspace").name, "after");
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("update keeps new name");
        let updated = read_manifest(&home)
            .expect("manifest")
            .expect("updated workspace");
        assert_eq!(updated.workspace.name, after.workspace.name);
        assert_eq!(updated.storage_generation, before.storage_generation);
        assert_eq!(
            std::fs::read(&marker).expect("retained generation storage"),
            b"existing generation"
        );
        assert_eq!(
            service
                .registry()
                .expect("registry")
                .root_for_name("before")
                .expect("released name"),
            None
        );
        models.close();
    }

    #[tokio::test]
    async fn stale_registry_name_cannot_override_workspace_manifest() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let before = read_manifest(&home)
            .expect("manifest")
            .expect("active workspace");
        service
            .registry()
            .expect("registry")
            .rename(&before.workspace.name, "replayed", directory.path())
            .expect("commit registry rename before crash");
        let info = service
            .info(InfoOptions {
                index_dir: None,
                root: Some(directory.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("info reconciles name");
        assert_eq!(info.workspace_index.expect("workspace").name, "before");
        assert_eq!(
            read_manifest(&home).expect("read-only info retained manifest"),
            Some(before.clone())
        );
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("index replays rename");
        let after = read_manifest(&home)
            .expect("manifest")
            .expect("reconciled workspace");
        assert_eq!(after.workspace.name.as_str(), "before");
        assert_eq!(after.storage_generation, before.storage_generation);
        models.close();
    }

    #[tokio::test]
    async fn moved_workspace_keeps_its_manifest_name_despite_stale_registry() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let before = move_after_interrupted_rename(&service, &models, &original, &moved).await;
        let info = service
            .info(InfoOptions {
                index_dir: None,
                root: Some(moved.clone()),
                include_status: false,
            })
            .await
            .expect("info resolves the authoritative name through the recorded old root");
        assert_eq!(info.workspace_index.expect("workspace").name, "before");
        let registry = service.registry().expect("registry");
        let new_name = "after".to_owned();
        assert_eq!(
            registry.root_for_name(&new_name).expect("read-only info"),
            Some(before.workspace.root.clone())
        );
        let home = moved.join(".zvec-grep");
        assert_eq!(
            read_manifest(&home)
                .expect("manifest")
                .expect("unchanged persisted name")
                .workspace
                .name,
            before.workspace.name
        );
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("index rebinds the renamed workspace after the move");
        let after = read_manifest(&home)
            .expect("manifest")
            .expect("reconciled workspace");
        assert_eq!(after.workspace.name, before.workspace.name);
        assert_eq!(after.storage_generation, before.storage_generation);
        assert_eq!(
            registry
                .root_for_name(&before.workspace.name)
                .expect("moved registration"),
            Some(std::fs::canonicalize(&moved).expect("moved root"))
        );
        assert_eq!(
            registry
                .root_for_name(&new_name)
                .expect("stale cache name is removed"),
            None
        );
        models.close();
    }

    #[tokio::test]
    async fn drop_after_interrupted_rename_and_move_releases_the_authoritative_name() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        move_after_interrupted_rename(&service, &models, &original, &moved).await;
        assert!(
            service
                .drop_index(&InfoOptions {
                    index_dir: None,
                    root: Some(moved),
                    include_status: false,
                })
                .await
                .expect("drop the moved workspace before rename replay")
        );
        let registry = service.registry().expect("registry");
        let new_name = "after".to_owned();
        assert_eq!(
            registry.root_for_name(&new_name).expect("released name"),
            None
        );
        assert_eq!(
            registry
                .name_for_root(&original)
                .expect("released old root"),
            None
        );
        service
            .index(
                &models,
                IndexOptions {
                    name: Some(new_name.clone()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("another workspace can reuse the authoritative name");
        assert_eq!(
            registry.root_for_name(&new_name).expect("new owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    async fn move_after_interrupted_rename(
        service: &WorkspaceIndexService,
        models: &ModelRuntimeManager,
        original: &Path,
        moved: &Path,
    ) -> crate::workspace::manifest::WorkspaceManifest {
        std::fs::create_dir(original).expect("original root");
        let original = std::fs::canonicalize(original).expect("canonical original root");
        service
            .index(
                models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(&original)
                },
            )
            .await
            .expect("initial index");
        let before = read_manifest(&original.join(".zvec-grep"))
            .expect("manifest")
            .expect("active workspace");
        service
            .registry()
            .expect("registry")
            .rename(&before.workspace.name, "after", &original)
            .expect("commit rename before manifest update is interrupted");
        std::fs::rename(original, moved).expect("move without replaying the rename");
        before
    }

    #[tokio::test]
    async fn invalid_model_configuration_does_not_reserve_a_workspace_name() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.name = Some("reserved".into());
        options.embedding.as_mut().expect("model spec").revision = Some("unsupported".into());
        let error = service
            .index(&models, options)
            .await
            .expect_err("model acquisition fails");
        assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        assert_eq!(models.snapshot().cached_runtimes, 0);
        let home = directory.path().join(".zvec-grep");
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("temp/build.json").exists());
        let registry = service.registry().expect("registry");
        let name = "reserved".to_owned();
        assert_eq!(
            registry
                .name_for_root(directory.path())
                .expect("reservation"),
            None
        );
        let info = InfoOptions {
            index_dir: None,
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(!service.drop_index(&info).await.expect("nothing to drop"));
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        assert!(!service.drop_index(&info).await.expect("idempotent drop"));
        models.close();
    }

    #[tokio::test]
    async fn initial_build_cancellation_leaves_no_published_or_pending_index() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        service
            .index(
                &models,
                IndexOptions {
                    signal: Some(signal),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("initial build cancelled");
        let home = directory.path().join(".zvec-grep");
        assert!(!home.join("manifest.json").exists());
        assert!(!super::has_build(&home));
        assert!(!super::has_index_storage(&home).expect("no abandoned storage"));
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("fresh initial build");
        assert!(read_manifest(&home).expect("manifest").is_some());
        models.close();
    }

    #[tokio::test]
    async fn index_concurrency_is_ephemeral_and_queries_ignore_refresh_overrides() {
        let directory = tempdir().expect("workspace");
        let root = std::fs::canonicalize(directory.path()).expect("workspace root");
        let cache = root.join("model-cache");
        let reference = "local/all-minilm-l6-v2";
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(&root);
        options.embedding.as_mut().expect("model").reference = reference.into();
        options.model_cache = Some(cache.clone());
        options.runtime_device = Some(Device::Cpu);
        options.embedding_concurrency = Some(8);
        service
            .index(&models, options)
            .await
            .expect("empty index with override");
        let manifest = read_manifest(&root.join(".zvec-grep"))
            .expect("manifest")
            .expect("index");
        let serialized = serde_json::to_string(&manifest).expect("manifest serialization");
        assert!(!serialized.contains("embedding_concurrency"));
        assert!(!serialized.contains("embeddingConcurrency"));
        // Pin the default query budget so a wrong override cannot hide behind cache eviction.
        let query_lease = models
            .acquire(super::ModelRuntimeRequest::new(
                reference,
                super::ModelConfig {
                    cache_dir: Some(cache.clone()),
                    device: Some(Device::Cpu),
                    ..super::ModelConfig::default()
                },
                None,
            ))
            .expect("default query runtime");
        for concurrency in [None, Some(24)] {
            crate::pipelines::indexed_search::service::context(
                &service,
                &models,
                &crate::api::context::ContextOptions {
                    root: Some(root.clone()),
                    query: Some("fixture".into()),
                    target_kind: Some(crate::domain::ContentKind::Text),
                    filter: crate::api::context::options::QueryFilter {
                        globs: vec!["!**".into()],
                        ..crate::api::context::options::QueryFilter::default()
                    },
                    model_cache: Some(cache.clone()),
                    device: Some(Device::Cpu),
                    embedding_concurrency: concurrency,
                    auto_update: false,
                    ..crate::api::context::ContextOptions::default()
                },
                None,
                false,
            )
            .await
            .expect("filtered query without inference");
            assert_eq!(
                models.snapshot().cached_runtimes,
                1,
                "queries share their default budget"
            );
            assert_eq!(models.snapshot().active_leases, 1);
        }
        assert!(!cache.exists(), "empty work does not load a model");
        drop(query_lease);
        models.close();
    }

    #[tokio::test]
    async fn composes_workspace_lifecycle_around_the_indexing_pipeline() {
        let directory = tempdir().expect("temporary directory");
        let root = std::fs::canonicalize(directory.path()).expect("canonical workspace root");
        let sources = root.join("sources");
        std::fs::create_dir(&sources).expect("source directory");
        let service = WorkspaceIndexService::with_test_registry();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(&root);
        options.scan.globs = Some(vec!["sources/**".into()]);
        options.embedding.as_mut().expect("local model").cache_dir = Some(root.join("model-cache"));
        let result = service
            .index(&models, options.clone())
            .await
            .expect("empty workspace should index");
        assert_eq!(result.files_scanned, 0);

        let info_options = InfoOptions {
            index_dir: None,
            root: Some(root.clone()),
            include_status: true,
        };
        let info = service
            .info(info_options.clone())
            .await
            .expect("workspace info");
        assert!(info.indexed);
        assert_eq!(info.status.expect("index status").files_stored, 0);
        assert!(info.workspace_index.is_some());
        let manifest = read_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert!(
            manifest
                .embedding_runtimes
                .values()
                .all(|runtime| *runtime == super::StoredModelConfig::default())
        );
        crate::pipelines::indexed_search::service::context(
            &service,
            &models,
            &crate::api::context::ContextOptions {
                root: Some(root.clone()),
                query: Some("fixture".into()),
                target_kind: Some(crate::domain::ContentKind::Text),
                auto_update: false,
                ..crate::api::context::ContextOptions::default()
            },
            None,
            false,
        )
        .await
        .expect("empty indexed search");
        assert_eq!(
            models.snapshot().cached_runtimes,
            1,
            "search reuses the configured model cache"
        );

        // An incompatible physical index requires an explicit rebuild.
        write_previous_index_version(&info.home, &manifest);
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(root.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect_err("previous index needs rebuild");
        assert!(error.message().contains("rebuild the index"));

        service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    ..options
                },
            )
            .await
            .expect("rebuild uses explicitly supplied configuration");
        let rebuilt = read_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(rebuilt.index_version, Some(super::CURRENT_INDEX_VERSION));
        assert_eq!(rebuilt.workspace.root, manifest.workspace.root);
        assert_eq!(rebuilt.workspace.scan, manifest.workspace.scan);
        assert_eq!(rebuilt.embedding_runtimes, manifest.embedding_runtimes);
        assert_eq!(rebuilt.workspace.name, manifest.workspace.name);
        assert!(rebuilt.workspace.created_epoch_ms >= manifest.workspace.created_epoch_ms);
        assert_ne!(rebuilt.storage_generation, manifest.storage_generation);

        assert!(service.drop_index(&info_options).await.expect("drop index"));
        assert!(!super::IndexStore::exists(&rebuilt.index_path()).expect("storage was deleted"));
        models.close();
    }
}
