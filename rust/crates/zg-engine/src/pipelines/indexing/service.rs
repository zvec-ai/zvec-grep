use std::{
    collections::BTreeMap,
    env, fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use std::sync::{Arc, Mutex};

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
            WorkspaceBuild, discard_build, has_build, has_generation_storage, prepare_build,
            publish_build, recover_build, recover_build_for_rebuild,
        },
        layout::{
            WorkspaceIndexLocation, find_nearest_workspace, reset_workspace_index,
            workspace_index_location,
        },
        lock::{FileLock, LockMode, LockWait},
        manifest::{
            ManifestState, WorkspaceManifest, inspect_workspace_manifest, read_workspace_manifest,
            write_workspace_manifest,
        },
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
    #[cfg(test)]
    fail_index_completion: bool,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            writers: crate::pipelines::indexed_search::writer::WriterContexts::default(),
            scanner: NativeScanner::default(),
            #[cfg(test)]
            fail_index_completion: false,
        }
    }

    pub(crate) async fn index(
        &self,
        models: &ModelRuntimeManager,
        mut options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        normalize_model_paths(&mut options)?;
        let requested_root = resolve_root(options.root.as_deref())?;
        validate_workspace_root(&requested_root)?;
        let location = workspace_index_location(&requested_root)?;
        options.root = Some(location.root.clone());
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
        // Check the on-disk version before any recovery or storage mutation.
        let existing =
            inspect_workspace_manifest(&location.home)?.into_manifest(options.rebuild)?;
        // Validate selection before recovery, storage, or model mutations.
        let scan = resolve_scan(existing.as_ref(), &options)?;
        crate::file_selection::ScanPolicy::new(&location.root, &scan)?;
        if options.rebuild {
            recover_build_for_rebuild(&location.home)?;
        } else {
            recover_build(&location.home)?;
        }
        let rebuilding = options.rebuild || !existing.as_ref().is_some_and(is_indexed);
        if options.reset_paths
            || options.scan != crate::api::index::options::ScanRulesUpdate::default()
        {
            // A changed selection can admit files outside a watcher's narrow change scope.
            options.changes.clear();
        }
        // Resolve only at the shared index boundary; query runtimes never read these overrides.
        options.embedding_concurrency = resolve_index_concurrency(
            &embedding_reference(existing.as_ref(), options.embedding.as_ref())?,
            options.embedding_concurrency,
        )
        .map_err(ModelError::into_engine_error)?;
        let IndexModels {
            runtimes: acquired,
            descriptor,
        } = acquire_index_models(models, existing.as_ref(), &options)?;
        if !rebuilding
            && let Some(previous) = existing
                .as_ref()
                .and_then(|manifest| manifest.workspace.index.descriptor())
        {
            previous
                .ensure_index_compatible(&descriptor)
                .map_err(|error| {
                    error.with_help("Rebuild the index with `zg --index --rebuild`.")
                })?;
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
            storage_path: manifest.storage_home(),
            embeddings: models.iter().map(|model| model.info().clone()).collect(),
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
        let completion = Arc::new(Mutex::new(None));
        let pipeline_progress = options.on_progress.as_ref().map(|reporter| {
            let reporter = reporter.clone();
            let completion = Arc::clone(&completion);
            crate::api::index::progress::IndexProgressReporter::new(move |progress| {
                if progress.phase == crate::api::index::progress::IndexProgressPhase::Done {
                    *completion
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(progress);
                } else {
                    reporter.report(progress);
                }
            })
        });
        let result = index_workspace(&IndexingContext {
            workspace_index: &manifest.workspace,
            storage: &writer.session.storage,
            scanner: &self.scanner,
            embedding_models: &embedding_models,
            embedding_concurrency: options.embedding_concurrency,
            on_progress: pipeline_progress,
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
        // Completion is observable only after storage has closed and publication succeeded.
        let completed = completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let (Some(reporter), Some(progress)) = (&options.on_progress, completed) {
            reporter.report(progress);
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
        let Some(manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(false);
        };
        if !is_indexed(&manifest) || manifest.workspace.index == IndexState::Disabled {
            return Ok(false);
        }
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: manifest.storage_home(),
        })?;
        let status =
            get_workspace_index_status(&manifest.workspace, &storage, &self.scanner, None).await;
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
                WorkspaceIndexPolicy::Uninitialized,
            ));
        };
        let _lock = LockWait::new(None, None)?
            .acquire(&location.home, LockMode::Read, "info")
            .await?;
        let manifest = match inspect_workspace_manifest(&location.home)? {
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
                let mut info = unindexed_info(location, WorkspaceIndexPolicy::Enabled);
                info.compatibility = IndexCompatibility::RebuildRequired {
                    actual_version,
                    expected_version: CURRENT_INDEX_VERSION,
                    reason,
                };
                info.suggestion = Some("rebuild the index with `zg --index --rebuild`".to_owned());
                return Ok(info);
            }
        };
        let metadata_indexed = is_indexed(&manifest);
        let storage_exists = IndexStore::exists(&manifest.storage_home())?;
        let indexed = metadata_indexed && storage_exists;
        let status = if options.include_status && indexed {
            let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: manifest.storage_home(),
            })?;
            let status =
                get_workspace_index_status(&manifest.workspace, &storage, &self.scanner, None)
                    .await;
            let close = storage.close();
            let status = status?;
            close?;
            Some(status)
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
            index_path: manifest.storage_home().join("storage"),
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(workspace_info(&manifest)),
            status,
            suggestion: workspace_suggestion(&manifest, indexed),
        })
    }

    pub(crate) async fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let location = workspace_index_location_from_option(options.root.as_deref())?;
        if !workspace_has_index_data(&location)? {
            return Ok(false);
        }
        let _lock = LockWait::new(None, None)?
            .acquire(&location.home, LockMode::Write, "index.drop")
            .await?;
        // Corrupt metadata must not prevent explicit cleanup.
        let has_data = workspace_has_index_data(&location)?;
        if has_data {
            reset_workspace_index(&location)?;
        }
        Ok(has_data)
    }
}

fn workspace_has_index_data(location: &WorkspaceIndexLocation) -> Result<bool, EngineError> {
    Ok(location.manifest_path.exists()
        || has_build(&location.home)
        || location.home.join("files.zvec").exists()
        || location.home.join("index.zvec").exists()
        || has_generation_storage(&location.home)?)
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
        root: location.root.clone(),
        scan: resolve_scan(active, options)?,
        index: IndexState::Enabled(Box::new(descriptor)),
        created_epoch_ms: active.map_or(now, |value| value.workspace.created_epoch_ms),
        updated_epoch_ms: now,
    };
    let runtime = models
        .iter()
        .map(|model| {
            Ok((
                model.info().model().reference(),
                embedding_runtime(active, options, model)?,
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
    if let Some(cache_dir) = options
        .embedding
        .as_mut()
        .and_then(|spec| spec.cache_dir.as_mut())
    {
        *cache_dir = std::path::absolute(&*cache_dir).map_err(|error| {
            EngineError::from_io("failed to resolve model cache directory", &error)
        })?;
    }
    Ok(())
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
    let model = acquire_model(models, existing, options)?;
    let descriptor = IndexDescriptor::new(
        model.info().clone(),
        std::collections::BTreeMap::new(),
        crate::domain::FTS_CONFIG,
    )?;
    if !descriptor
        .content_indexes()
        .iter()
        .any(|index| index.kind == crate::domain::ContentKind::Text)
    {
        return Err(EngineError::unsupported(
            "text indexing requires a model that supports text content",
        ));
    }
    Ok(IndexModels {
        runtimes: vec![model],
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
        options
            .api_key
            .clone()
            .or_else(|| existing_runtime.and_then(|runtime| runtime.api_key.clone()))
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
            existing_runtime.and_then(|runtime| runtime.device),
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
                    existing_runtime.and_then(|runtime| runtime.cache_dir.clone()),
                ),
                device,
            },
            options.embedding_concurrency,
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
            .and_then(|manifest| manifest.embedding())
            .map(|embedding| embedding.model().reference()),
        global_default: crate::config::string(&config, &["defaults", "embedding"]),
        fallback: Some(DEFAULT_LOCAL_EMBEDDING.to_owned()),
        ..ResolveEmbeddingReferenceOptions::default()
    })
    .map_err(ModelError::into_engine_error)?
    .ok_or_else(|| EngineError::internal("default embedding model is not configured"))
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
            EngineError::invalid_argument(
                "model is not part of the workspace index; rebuild the index",
            )
        })?;
    schema
        .ensure_index_compatible(model.info())
        .map_err(|error| error.with_help("Rebuild the index with `zg --index --rebuild`."))
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
) -> Result<ModelConfig, EngineError> {
    let current = existing
        .and_then(|manifest| {
            manifest
                .embedding_runtimes
                .get(&model.info().model().reference())
                .cloned()
        })
        .unwrap_or_default();
    let config = crate::config::read()?;
    if model.info().model().provider() == "local" {
        let reference = format!(
            "{}/{}",
            model.info().model().provider(),
            model.info().model().name()
        );
        Ok(ModelConfig {
            cache_dir: crate::config::model_cache(
                &config,
                options
                    .model_cache
                    .clone()
                    .or_else(|| options.embedding.as_ref().and_then(|e| e.cache_dir.clone())),
                current.cache_dir,
            ),
            device: crate::config::runtime_device(
                &config,
                &reference,
                options.device.or_else(|| {
                    options
                        .embedding
                        .as_ref()
                        .map(|e| e.device)
                        .filter(|device| *device != Device::Auto)
                }),
                current.device,
            )?,
            ..ModelConfig::default()
        })
    } else {
        Ok(ModelConfig {
            api_key: current.api_key,
            endpoint: model
                .configured_endpoint()
                .map(str::to_owned)
                .or(current.endpoint),
            device: None,
            cache_dir: None,
        })
    }
}

pub(in crate::pipelines) fn is_indexed(manifest: &WorkspaceManifest) -> bool {
    manifest.workspace.index_enabled() && manifest.index_version.is_some()
}

fn validate_workspace_root(root: &Path) -> Result<(), EngineError> {
    if !root.is_dir() {
        return Err(EngineError::invalid_argument(format!(
            "workspace root must be an existing directory: {}",
            root.display()
        )));
    }
    Ok(())
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

    #[test]
    fn rejects_incompatible_index_versions() {
        crate::workspace::manifest::require_current_index_version(Some(
            super::CURRENT_INDEX_VERSION,
        ))
        .expect("current index");
        for version in [None, Some(0), Some(1), Some(3), Some(5), Some(u32::MAX)] {
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
        let service = WorkspaceIndexService::new();
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
    async fn completion_progress_is_sent_only_after_successful_publication() {
        use crate::api::index::progress::{IndexProgressPhase, IndexProgressReporter};
        for fail in [false, true] {
            let directory = tempdir().expect("workspace");
            let mut service = WorkspaceIndexService::new();
            service.fail_index_completion = fail;
            let models = ModelRuntimeManager::new();
            let home = directory.path().join(".zvec-grep");
            let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let captured = observed.clone();
            let result = service
                .index(
                    &models,
                    IndexOptions {
                        root: Some(directory.path().to_path_buf()),
                        embedding: Some(EmbeddingModelSpec {
                            reference: "local/potion-code-16m-v2".into(),
                            revision: None,
                            cache_dir: None,
                            endpoint: None,
                            device: Device::Cpu,
                        }),
                        on_progress: Some(IndexProgressReporter::new(move |progress| {
                            if progress.phase == IndexProgressPhase::Done {
                                assert!(
                                    super::read_workspace_manifest(&home)
                                        .expect("published metadata")
                                        .is_some()
                                );
                                captured.store(true, std::sync::atomic::Ordering::Release);
                            }
                        })),
                        ..Default::default()
                    },
                )
                .await;
            assert_eq!(result.is_err(), fail);
            assert_eq!(observed.load(std::sync::atomic::Ordering::Acquire), !fail);
            models.close();
        }
    }

    #[tokio::test]
    async fn reopening_a_moved_workspace_uses_its_current_root() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&original).expect("workspace root");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(original.clone()),
                    scan: ScanRulesUpdate {
                        globs: Some(vec!["*.rs".into()]),
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
        let before = super::read_workspace_manifest(&original.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        std::fs::rename(&original, &moved).expect("move workspace");
        let info = service
            .info(InfoOptions {
                root: Some(moved.clone()),
                include_status: true,
            })
            .await
            .expect("moved workspace info");
        let workspace = info.workspace_index.expect("workspace index");
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
        let after = super::read_workspace_manifest(&moved.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(after.workspace.scan, before.workspace.scan);
        assert_eq!(after.workspace.root, moved);
        models.close();
    }

    #[tokio::test]
    async fn dropping_a_missing_index_is_an_idempotent_no_op() {
        let directory = tempdir().expect("temporary directory");

        assert!(
            !WorkspaceIndexService::new()
                .drop_index(&InfoOptions {
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
        let storage = home
            .join("generations")
            .join(uuid::Uuid::new_v4().to_string())
            .join("storage");
        std::fs::create_dir_all(&storage).expect("orphaned generation");
        let service = WorkspaceIndexService::new();
        let options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("build.json").exists());
        assert!(
            service
                .drop_index(&options)
                .await
                .expect("drop orphaned storage")
        );
        assert!(!storage.exists());
        assert!(home.join("generations").is_dir());
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
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
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
            super::read_workspace_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage").exists());
        assert!(!super::has_build(&home));
        let refresh = crate::pipelines::indexed_search::service::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
        );
        service
            .index(&models, refresh)
            .await
            .expect("ordinary refresh updates active index");
        let updated = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        assert_eq!(updated.storage_generation, active.storage_generation);
        assert!(active.storage_home().exists());
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
        let rebuilt = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        assert_ne!(rebuilt.storage_generation, active.storage_generation);
        assert!(!active.storage_home().exists());
        models.close();
    }

    #[tokio::test]
    async fn search_device_override_does_not_persist_in_manifest() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("manifest");
        for runtime in manifest.embedding_runtimes.values_mut() {
            runtime.device = Some(Device::Auto);
        }
        super::write_workspace_manifest(&home, &manifest).expect("save automatic device");
        let request = crate::api::context::ContextOptions {
            device: Some(Device::Cpu),
            ..Default::default()
        };
        let options = crate::pipelines::indexed_search::service::refresh_options(
            &request,
            directory.path().to_path_buf(),
        );
        service.index(&models, options).await.expect("refresh");
        let refreshed = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("manifest");
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
        let configured = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("manifest");
        assert!(
            configured
                .embedding_runtimes
                .values()
                .all(|runtime| runtime.device == Some(Device::Cpu))
        );
        models.close();
    }

    #[tokio::test]
    async fn abandoned_rebuild_does_not_change_normal_index_settings_or_storage() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("active");
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
        std::fs::write(
            abandoned.target.storage_home().join("checkpoint"),
            "discard me",
        )
        .expect("checkpoint");
        service
            .index(&models, options.clone())
            .await
            .expect("ordinary index uses active settings");
        let updated = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("active");
        assert_eq!(updated.storage_generation, active.storage_generation);
        assert_eq!(updated.workspace.scan, active.workspace.scan);
        assert_eq!(updated.embedding_runtimes, active.embedding_runtimes);
        assert!(!abandoned.target.storage_home().exists());
        assert!(!super::has_build(&home));
        models.close();
    }

    #[tokio::test]
    async fn explicit_rebuild_starts_over_even_after_recovering_a_publication() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("active");
        let published = super::prepare_build(active.clone()).expect("build");
        std::fs::create_dir(published.target.storage_home().join("storage"))
            .expect("completed storage");
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
        let rebuilt = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("active");
        assert_ne!(
            rebuilt.storage_generation,
            published.target.storage_generation
        );
        assert!(!active.storage_home().exists());
        assert!(!published.target.storage_home().exists());
        assert!(rebuilt.storage_home().join("storage").exists());
        assert!(!super::has_build(&home));
        models.close();
    }

    #[tokio::test]
    async fn failed_checkpoint_does_not_publish_an_incomplete_rebuild() {
        let directory = tempdir().expect("workspace");
        let mut service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
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
            super::read_workspace_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage").exists());
        assert!(!super::has_build(&home));
        // The unchanged active index does not need a query-driven refresh.
        let location = super::workspace_index_location(directory.path()).expect("location");
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
                    root: Some(directory.path().to_path_buf()),
                    include_status: false
                })
                .await
                .expect("drop incomplete build")
        );
        assert!(!super::has_build(&home));
        assert!(!active.storage_home().exists());
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

    #[tokio::test]
    async fn unrelated_workspaces_can_share_a_directory_name() {
        let directory = tempdir().expect("workspaces");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        for parent in ["first", "second"] {
            let root = directory.path().join(parent).join("docs");
            std::fs::create_dir_all(&root).expect("workspace directory");
            service
                .index(&models, empty_index_options(&root))
                .await
                .expect("unrelated docs workspace is addressed by its root");
            let info = service
                .info(InfoOptions {
                    root: Some(root.clone()),
                    include_status: false,
                })
                .await
                .expect("workspace info");
            let workspace = info.workspace_index.expect("workspace index");
            assert_eq!(workspace.root, root.canonicalize().expect("canonical root"));
        }
        models.close();
    }

    async fn assert_nested_index_keeps_parent(model: &str, rebuild: bool) {
        let directory = tempdir().expect("nested workspaces");
        let root = std::fs::canonicalize(directory.path()).expect("parent root");
        let child = root.join("sub");
        std::fs::create_dir(root.join("docs")).expect("parent sources");
        std::fs::create_dir_all(child.join("src")).expect("child sources");
        let service = WorkspaceIndexService::new();
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
        let parent = super::read_workspace_manifest(&parent_home)
            .expect("manifest read")
            .expect("parent manifest");
        assert_eq!(
            service
                .info(InfoOptions {
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
        let indexed = super::read_workspace_manifest(&child_home)
            .expect("child manifest read")
            .expect("child owns its manifest");
        assert_eq!(indexed.workspace.root, child);
        assert_eq!(
            indexed
                .embedding()
                .expect("child model")
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
        assert!(super::IndexStore::exists(&parent.storage_home()).expect("parent storage remains"));
        assert_eq!(
            service
                .info(InfoOptions {
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

    #[tokio::test]
    async fn dropping_a_deleted_source_directory_is_a_no_op_without_recreating_it() {
        let directory = tempdir().expect("workspace roots");
        let deleted = directory.path().join("deleted");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&deleted).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(&deleted))
            .await
            .expect("original workspace index");
        std::fs::remove_dir_all(&deleted).expect("source directory deleted externally");
        let options = InfoOptions {
            root: Some(deleted.clone()),
            include_status: false,
        };
        assert!(
            !service
                .drop_index(&options)
                .await
                .expect("deleted workspace has no index to drop")
        );
        assert!(!deleted.exists());
        assert!(
            !service
                .drop_index(&options)
                .await
                .expect("idempotent cleanup")
        );
        service
            .index(&models, empty_index_options(&replacement))
            .await
            .expect("replacement workspace has its own index");
        models.close();
    }

    #[tokio::test]
    async fn dropping_a_moved_workspace_removes_its_local_index() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&original).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(&original))
            .await
            .expect("original workspace index");
        std::fs::rename(&original, &moved).expect("move without reopening index");
        assert!(
            service
                .drop_index(&InfoOptions {
                    root: Some(moved.clone()),
                    include_status: false,
                })
                .await
                .expect("drop moved workspace")
        );
        assert!(!moved.join(".zvec-grep/manifest.json").exists());

        service
            .index(&models, empty_index_options(&replacement))
            .await
            .expect("another root can be indexed");
        models.close();
    }

    #[tokio::test]
    async fn copied_pending_first_build_uses_its_root_without_changing_the_original() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let copy = directory.path().join("copy");
        std::fs::create_dir(&original).expect("original root");
        std::fs::create_dir_all(copy.join(".zvec-grep")).expect("copy home");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(&original))
            .await
            .expect("create workspace fixture");
        let home = original.join(".zvec-grep");
        let manifest = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("manifest");
        super::prepare_build(manifest).expect("simulate crashed first build");
        std::fs::remove_file(home.join("manifest.json")).expect("unpublished fixture");
        let original_build = original.join(".zvec-grep/build.json");
        let original_bytes = std::fs::read(&original_build).expect("original pending build");
        assert!(!original.join(".zvec-grep/manifest.json").exists());
        std::fs::copy(&original_build, copy.join(".zvec-grep/build.json"))
            .expect("copy pending metadata");
        service
            .index(&models, empty_index_options(&copy))
            .await
            .expect("a copy is addressed by its own root");
        let copied = super::read_workspace_manifest(&copy.join(".zvec-grep"))
            .expect("copied manifest")
            .expect("published copy");
        assert_eq!(
            copied
                .workspace
                .root
                .canonicalize()
                .expect("canonical copy root"),
            copy.canonicalize().expect("copy root")
        );
        assert_eq!(
            std::fs::read(original_build).expect("original build unchanged"),
            original_bytes
        );
        models.close();
    }

    #[tokio::test]
    async fn invalid_scan_rules_do_not_create_index_or_acquire_models() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        for scan in [
            ScanRulesUpdate {
                max_file_size_bytes: Some(Some(0)),
                ..ScanRulesUpdate::default()
            },
            ScanRulesUpdate {
                globs: Some(vec!["[".into()]),
                ..ScanRulesUpdate::default()
            },
            ScanRulesUpdate {
                ignore_files: Some(vec!["missing.ignore".into()]),
                ..ScanRulesUpdate::default()
            },
        ] {
            let error = service
                .index(
                    &models,
                    IndexOptions {
                        scan,
                        ..empty_index_options(directory.path())
                    },
                )
                .await
                .expect_err("invalid scan rules");
            assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
            let home = directory.path().join(".zvec-grep");
            assert!(!home.join("manifest.json").exists());
            assert!(!super::has_build(&home));
            assert!(!super::has_generation_storage(&home).expect("no index generation"));
            assert_eq!(models.snapshot().cached_runtimes, 0);
        }
    }

    #[tokio::test]
    async fn invalid_scan_update_preserves_manifest_and_pending_rebuild() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("read")
            .expect("active index");
        let pending = super::prepare_build(active.clone()).expect("pending rebuild");
        let error = service
            .index(
                &models,
                IndexOptions {
                    scan: ScanRulesUpdate {
                        ignore_files: Some(vec!["missing.ignore".into()]),
                        ..ScanRulesUpdate::default()
                    },
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("missing explicit ignore file");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            super::read_workspace_manifest(&home).expect("unchanged manifest"),
            Some(active.clone())
        );
        assert!(super::has_build(&home));
        assert!(pending.target.storage_home().exists());
        models.close();
    }

    #[tokio::test]
    async fn legacy_manifest_names_are_ignored_and_removed_without_rebuilding() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let path = home.join("manifest.json");
        let before = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("workspace");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("manifest bytes")).expect("JSON");
        legacy["name"] = serde_json::json!("docs");
        std::fs::write(&path, serde_json::to_vec(&legacy).expect("legacy JSON"))
            .expect("legacy manifest");
        let info = WorkspaceIndexService::new()
            .info(InfoOptions {
                root: Some(directory.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("read legacy metadata");
        assert_eq!(
            info.workspace_index.expect("workspace").root,
            directory.path().canonicalize().expect("root")
        );
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("update legacy index");
        let after = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("updated workspace");
        assert_eq!(after.storage_generation, before.storage_generation);
        assert_eq!(
            after.workspace.created_epoch_ms,
            before.workspace.created_epoch_ms
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("manifest bytes")).expect("JSON");
        assert!(saved.get("name").is_none());
        models.close();
    }

    #[tokio::test]
    async fn initial_model_failure_leaves_no_workspace_metadata() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.embedding.as_mut().expect("model spec").revision = Some("unsupported".into());
        let error = service
            .index(&models, options)
            .await
            .expect_err("model acquisition fails");
        assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        assert_eq!(models.snapshot().cached_runtimes, 0);
        let home = directory.path().join(".zvec-grep");
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("build.json").exists());
        assert!(
            !service
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .await
                .expect("no index to drop")
        );
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("retry");
        let manifest = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("index");
        assert_eq!(
            manifest.workspace.root.canonicalize().expect("root"),
            directory.path().canonicalize().expect("root")
        );
        models.close();
    }

    #[tokio::test]
    async fn initial_build_cancellation_leaves_no_published_or_pending_index() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
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
        assert!(!super::has_generation_storage(&home).expect("no abandoned storage"));
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("fresh initial build");
        assert!(
            super::read_workspace_manifest(&home)
                .expect("manifest")
                .is_some()
        );
        models.close();
    }

    #[tokio::test]
    async fn index_concurrency_is_ephemeral_and_queries_ignore_refresh_overrides() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.embedding.as_mut().expect("model").reference = "local/all-minilm-l6-v2".into();
        options.embedding.as_mut().expect("model").cache_dir =
            Some(directory.path().join("model-cache"));
        options.embedding_concurrency = Some(8);
        service
            .index(&models, options)
            .await
            .expect("empty index with override");
        let home = directory.path().join(".zvec-grep");
        let manifest = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("index");
        let serialized = serde_json::to_string(&manifest).expect("manifest serialization");
        assert!(!serialized.contains("embedding_concurrency"));
        assert!(!serialized.contains("embeddingConcurrency"));
        let default = crate::api::context::ContextOptions::default();
        let first = crate::pipelines::indexed_search::service::acquire_search_model(
            &models,
            &manifest,
            &default,
            directory.path(),
        )
        .expect("query default");
        let second = crate::pipelines::indexed_search::service::acquire_search_model(
            &models,
            &manifest,
            &crate::api::context::ContextOptions {
                embedding_concurrency: Some(24),
                ..default
            },
            directory.path(),
        )
        .expect("query with refresh override");
        assert_eq!(
            models.snapshot().cached_runtimes,
            1,
            "queries share their default budget"
        );
        assert_eq!(models.snapshot().active_leases, 2);
        assert!(
            !directory.path().join("model-cache").exists(),
            "empty work does not load a model"
        );
        drop(first);
        drop(second);
        models.close();
    }

    #[tokio::test]
    async fn composes_workspace_lifecycle_around_the_indexing_pipeline() {
        let directory = tempdir().expect("temporary directory");
        let sources = directory.path().join("sources");
        std::fs::create_dir(&sources).expect("source directory");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.scan.globs = Some(vec!["sources/**".into()]);
        options.embedding.as_mut().expect("local model").cache_dir =
            Some(directory.path().join("model-cache"));
        let result = service
            .index(&models, options.clone())
            .await
            .expect("empty workspace should index");
        assert_eq!(result.files_scanned, 0);

        let info_options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: true,
        };
        let info = service
            .info(info_options.clone())
            .await
            .expect("workspace info");
        assert!(info.indexed);
        assert_eq!(info.status.expect("index status").files_stored, 0);
        assert!(info.workspace_index.is_some());
        let manifest = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(
            manifest
                .embedding_runtimes
                .values()
                .next()
                .expect("runtime")
                .cache_dir,
            Some(directory.path().join("model-cache"))
        );
        let lease = crate::pipelines::indexed_search::service::acquire_search_model(
            &models,
            &manifest,
            &crate::api::context::ContextOptions::default(),
            directory.path(),
        )
        .expect("search model");
        assert_eq!(
            models.snapshot().cached_runtimes,
            1,
            "search reuses the configured model cache"
        );
        drop(lease);

        // An incompatible physical index requires an explicit rebuild.
        write_previous_index_version(&info.home, &manifest);
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
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
        let rebuilt = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(rebuilt.index_version, Some(super::CURRENT_INDEX_VERSION));
        assert_eq!(rebuilt.workspace.root, manifest.workspace.root);
        assert_eq!(rebuilt.workspace.scan, manifest.workspace.scan);
        assert_eq!(rebuilt.embedding_runtimes, manifest.embedding_runtimes);
        assert!(rebuilt.workspace.created_epoch_ms >= manifest.workspace.created_epoch_ms);
        assert_ne!(rebuilt.storage_generation, manifest.storage_generation);

        assert!(service.drop_index(&info_options).await.expect("drop index"));
        assert!(!super::IndexStore::exists(&rebuilt.storage_home()).expect("storage was deleted"));
        models.close();
    }
}
