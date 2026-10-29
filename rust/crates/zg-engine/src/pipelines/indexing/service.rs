use std::{
    collections::BTreeMap,
    env, fmt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use zg_host_native::NativeScanner;

use crate::{
    EngineError,
    api::{
        index::{
            IndexOptions, IndexResult,
            options::EmbeddingModelSpec,
            progress::{DETAIL_SAVING_INDEX, IndexProgress},
        },
        info::{
            InfoOptions, InfoResult,
            result::{IndexCompatibility, InfoSource, WorkspaceIndexInfo, WorkspaceIndexPolicy},
        },
    },
    domain::{
        IndexDescriptor, IndexState, Workspace, WorkspaceLayout,
        model::{Device, EncryptedApiKey, ModelConfig},
    },
    models::{
        ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest,
        ResolveEmbeddingReferenceOptions, resolve_embedding_reference, resolve_index_concurrency,
    },
    storage::{IndexStore, types::WorkspaceIndexStorageOptions},
    utils::{directory_exists, resolve_directory},
    workspace::{
        CURRENT_INDEX_VERSION,
        credentials::{decrypt_api_key, delete_credentials_key, encrypt_api_key},
        find_nearest_workspace,
        index::{
            discard_build, drop_index_storage, prepare_build, publish_build, recover_build,
            recover_build_for_rebuild,
        },
        lock::{LockGuard, LockMode, LockWait},
        manifest::{
            ManifestState, delete_workspace_manifest, inspect_workspace_manifest,
            read_workspace_manifest, write_workspace_manifest,
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
        let requested_root = resolve_directory(options.root.as_deref())?;
        let layout = WorkspaceLayout::new(requested_root.join(".zvec-grep"))?;
        options.root = Some(requested_root.clone());
        if options.require_existing_index && !layout.has_manifest()? {
            return Err(missing_refresh_index(&requested_root));
        }
        let home_lock = Arc::new(
            LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?
                .acquire(
                    &requested_root,
                    &layout,
                    LockMode::Exclusive,
                    if options.rebuild {
                        "index.rebuild"
                    } else {
                        "index"
                    },
                )
                .await?,
        );
        // Check the on-disk version before any recovery or storage mutation.
        let existing = load_index_workspace(&requested_root, &layout, &options)?;
        // Validate selection before recovery, storage, or model mutations.
        options.scan.normalize_ignore_paths(&requested_root)?;
        let scan = resolve_scan(existing.as_ref().map(Workspace::scan), &options)?;
        crate::file_selection::ScanPolicy::new(&requested_root, &scan)?;
        if options.rebuild {
            recover_build_for_rebuild(&layout)?;
        } else {
            recover_build(&layout)?;
        }
        let rebuilding =
            options.rebuild || !existing.as_ref().is_some_and(Workspace::index_enabled);
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
                .and_then(|workspace| workspace.index().descriptor())
        {
            previous
                .ensure_index_compatible(&descriptor)
                .map_err(|error| {
                    error.with_help("Rebuild the index with `zg --index --rebuild`.")
                })?;
        }
        let mut workspace = index_workspace_state(
            &requested_root,
            &layout,
            existing.as_ref(),
            &options,
            &acquired,
            &descriptor,
            scan,
            rebuilding,
        )?;
        if rebuilding {
            options.changes.clear();
            prepare_build(&workspace)?;
        }
        let result = self
            .run_index(
                &workspace,
                rebuilding,
                acquired,
                &options,
                Arc::clone(&home_lock),
            )
            .await;
        if result.is_err() && rebuilding {
            let _ = discard_build(&workspace);
        }
        let (indexed, completed) = result?;
        let now = epoch_millis();
        if rebuilding {
            publish_build(workspace, now)?;
        } else {
            workspace.record_update(now);
            write_workspace_manifest(workspace.layout().manifest_path(), &workspace)?;
        }
        if let (Some(reporter), Some(progress)) = (&options.on_progress, completed) {
            reporter.report(progress);
        }
        Ok(indexed)
    }

    async fn run_index(
        &self,
        workspace: &Workspace,
        rebuilding: bool,
        models: Vec<ModelRuntimeLease>,
        options: &IndexOptions,
        home_lock: Arc<LockGuard>,
    ) -> Result<(IndexResult, Option<IndexProgress>), EngineError> {
        let storage_path = if rebuilding {
            workspace.layout().staged_index_dir()
        } else {
            workspace.layout().index_dir()
        };
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: storage_path.to_path_buf(),
            embeddings: models.iter().map(|model| model.info().clone()).collect(),
        })?;
        let writer = self.writers.register(
            crate::pipelines::indexed_search::writer::WriterSession::new(
                storage,
                models,
                workspace.clone(),
                home_lock,
            ),
            !rebuilding,
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
            workspace,
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
            // Exercise service cleanup when the indexing pipeline fails to finalize storage.
            if self.fail_index_completion {
                Err(EngineError::storage_failure(
                    "injected finalization failure",
                ))
            } else {
                Ok(indexed)
            }
        });
        let completed = completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if result.is_ok()
            && let (Some(reporter), Some(progress)) = (&options.on_progress, &completed)
        {
            // Optimization has finished, but closing storage and publishing the
            // manifest still take time. Keep that work visible without reporting
            // Done before the active generation is durable.
            reporter.report(IndexProgress {
                phase: crate::api::index::progress::IndexProgressPhase::Indexing,
                detail: Some(DETAIL_SAVING_INDEX.to_owned()),
                ..progress.clone()
            });
        }
        // Flush writes and close storage before the active manifest can select
        // the completed generation.
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
        Ok((indexed, completed))
    }

    pub(in crate::pipelines) async fn workspace_needs_refresh(
        &self,
        source_root: &Path,
        layout: &WorkspaceLayout,
        wait: &LockWait<'_>,
        signal: Option<CancellationToken>,
    ) -> Result<bool, EngineError> {
        let _lock = wait
            .acquire(source_root, layout, LockMode::Shared, "context.refresh")
            .await?;
        let Some(workspace) = read_workspace_manifest(source_root, layout)? else {
            return Ok(false);
        };
        if !workspace.index_enabled() {
            return Ok(false);
        }
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: workspace.layout().index_dir().to_path_buf(),
        })?;
        let status =
            get_workspace_index_status(&workspace, &storage, &self.scanner, signal, false).await;
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
        let requested_root = resolve_directory(options.root.as_deref())?;
        let Some((source_root, layout)) = find_nearest_workspace(&requested_root)? else {
            return Ok(unindexed_info(
                &requested_root,
                &WorkspaceLayout::new(requested_root.join(".zvec-grep"))?,
                WorkspaceIndexPolicy::Uninitialized,
            ));
        };
        let _lock = LockWait::new(None, None)?
            .acquire(&source_root, &layout, LockMode::Shared, "info")
            .await?;
        let workspace = match inspect_workspace_manifest(&source_root, &layout)? {
            ManifestState::Current(workspace) => workspace,
            ManifestState::Missing => {
                return Ok(unindexed_info(
                    &source_root,
                    &layout,
                    WorkspaceIndexPolicy::Uninitialized,
                ));
            }
            ManifestState::RebuildRequired {
                actual_version,
                reason,
            } => {
                let mut info = unindexed_info(&source_root, &layout, WorkspaceIndexPolicy::Enabled);
                info.compatibility = IndexCompatibility::RebuildRequired {
                    actual_version,
                    expected_version: CURRENT_INDEX_VERSION,
                    reason,
                };
                info.suggestion = Some("rebuild the index with `zg --index --rebuild`".to_owned());
                return Ok(info);
            }
        };
        let metadata_indexed = workspace.index_enabled();
        let storage_exists = directory_exists(workspace.layout().index_dir())?;
        let indexed = metadata_indexed && storage_exists;
        let status = if options.include_status && indexed {
            let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: workspace.layout().index_dir().to_path_buf(),
            })?;
            let status =
                get_workspace_index_status(&workspace, &storage, &self.scanner, None, true).await;
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
            root: source_root,
            indexed,
            index_policy: workspace.index().into(),
            home: layout.storage_dir().to_path_buf(),
            index_path: workspace.layout().index_dir().to_path_buf(),
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(WorkspaceIndexInfo::from_workspace(&workspace)),
            status,
            suggestion: workspace_suggestion(&workspace, indexed),
        })
    }

    pub(crate) async fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let root = resolve_directory(options.root.as_deref())?;
        let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
        if !layout.has_index_data()? {
            return Ok(false);
        }
        let _lock = LockWait::new(None, None)?
            .acquire(&root, &layout, LockMode::Exclusive, "index.drop")
            .await?;
        // Corrupt metadata must not prevent explicit cleanup.
        let has_data = layout.has_index_data()?;
        if has_data {
            drop_index_storage(&layout)?;
            delete_workspace_manifest(&layout)?;
            delete_credentials_key(&layout)?;
        }
        Ok(has_data)
    }
}

fn load_index_workspace(
    root: &Path,
    layout: &WorkspaceLayout,
    options: &IndexOptions,
) -> Result<Option<Workspace>, EngineError> {
    let existing = inspect_workspace_manifest(root, layout)?.into_workspace(options.rebuild)?;
    // Recheck after admission: a queued refresh must not become a first build.
    if options.require_existing_index && !existing.as_ref().is_some_and(Workspace::index_enabled) {
        return Err(missing_refresh_index(root));
    }
    Ok(existing)
}

fn missing_refresh_index(root: &Path) -> EngineError {
    EngineError::not_found(format!(
        "No index found in {}. Run `zg --index --embedding <model>` to build it before refreshing.",
        root.display()
    ))
}

#[allow(clippy::too_many_arguments)]
fn index_workspace_state(
    source_root: &Path,
    layout: &WorkspaceLayout,
    active: Option<&Workspace>,
    options: &IndexOptions,
    models: &[ModelRuntimeLease],
    descriptor: &IndexDescriptor,
    scan: crate::domain::ScanRules,
    rebuilding: bool,
) -> Result<Workspace, EngineError> {
    let generation = if rebuilding {
        uuid::Uuid::new_v4().to_string()
    } else {
        active
            .and_then(Workspace::storage_generation)
            .expect("existing index generation")
            .to_owned()
    };
    let now = epoch_millis();
    let configs = models
        .iter()
        .map(|model| {
            Ok((
                model.info().model().reference(),
                saved_embedding_config(layout, active, options, model)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, EngineError>>()?;
    let descriptor = IndexDescriptor::new(
        descriptor.default_embedding_model().clone(),
        descriptor.embedding_routes().clone(),
        configs,
        descriptor.fts(),
    )?;
    Workspace::new(
        source_root,
        layout.clone(),
        scan,
        Some(CURRENT_INDEX_VERSION),
        Some(generation),
        active.map_or(now, Workspace::created_epoch_ms),
        now,
        IndexState::Enabled(Box::new(descriptor)),
    )
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
    for cache_dir in options
        .model_cache
        .as_mut()
        .or_else(|| {
            options
                .embedding
                .as_mut()
                .and_then(|spec| spec.cache_dir.as_mut())
        })
        .into_iter()
        .chain(options.runtime_model_cache.iter_mut())
    {
        *cache_dir = std::path::absolute(&*cache_dir).map_err(|error| {
            EngineError::from_io("failed to resolve model cache directory", &error)
        })?;
    }
    let saved_cache_dir = options.model_cache.as_ref().or_else(|| {
        options
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.cache_dir.as_ref())
    });
    if let Some(cache_dir) = saved_cache_dir
        && cache_dir.to_str().is_none()
    {
        return Err(EngineError::invalid_argument(format!(
            "cannot save model cache directory '{}': path is not valid Unicode",
            cache_dir.display(),
        )));
    }
    Ok(())
}

struct IndexModels {
    runtimes: Vec<ModelRuntimeLease>,
    descriptor: IndexDescriptor,
}

fn acquire_index_models(
    models: &ModelRuntimeManager,
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Result<IndexModels, EngineError> {
    let model = acquire_model(models, existing, options)?;
    let descriptor = IndexDescriptor::new(
        model.info().clone(),
        std::collections::BTreeMap::new(),
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
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Result<ModelRuntimeLease, EngineError> {
    if let Some(embedding) = &options.embedding
        && embedding.revision.is_some()
    {
        return Err(EngineError::unsupported(
            "embedding revision overrides are not supported by the catalog-backed runtime",
        ));
    }
    let reference = embedding_reference(existing, options.embedding.as_ref())?;
    let config = crate::config::read()?;
    let provider = reference.split('/').next().unwrap_or_default();
    let local = provider == "local";
    if (!local && options.device.is_some()) || (local && options.endpoint.is_some()) {
        return Err(EngineError::invalid_argument(
            "device requires a local model; endpoint requires a remote model",
        ));
    }
    let existing_index = existing.and_then(|workspace| workspace.index().descriptor());
    let existing_runtime =
        existing_index.and_then(|index| index.embedding_configs().get(&reference));
    let requested_key = options
        .runtime_api_key
        .as_ref()
        .or(options.api_key.as_ref());
    let saved_key = if local || requested_key.is_some() {
        None
    } else if let (Some(workspace), Some(runtime)) = (existing, existing_runtime) {
        runtime
            .api_key
            .as_ref()
            .map(|key| decrypt_api_key(key, workspace.layout(), &reference))
            .transpose()?
    } else {
        None
    };
    let api_key = if local {
        None
    } else {
        requested_key
            .cloned()
            .or(saved_key)
            .or_else(|| crate::config::string(&config, &["providers", provider, "apiKey"]))
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
        let root = resolve_directory(options.root.as_deref())?;
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
                        .runtime_model_cache
                        .clone()
                        .or_else(|| options.model_cache.clone())
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
    existing: Option<&Workspace>,
    requested: Option<&EmbeddingModelSpec>,
) -> Result<String, EngineError> {
    let config = crate::config::read()?;
    resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit: requested.map(|embedding| embedding.reference.clone()),
        existing: existing
            .and_then(|workspace| {
                workspace
                    .index()
                    .descriptor()
                    .map(IndexDescriptor::default_embedding_model)
            })
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
    existing: Option<&IndexDescriptor>,
    model: &crate::domain::EmbeddingModelInfo,
) -> Result<(), EngineError> {
    let Some(existing) = existing else {
        return Ok(());
    };
    let schema = existing
        .embeddings()
        .into_iter()
        .find(|schema| schema.model().reference() == model.model().reference())
        .ok_or_else(|| {
            EngineError::invalid_argument(
                "model is not part of the workspace index; rebuild the index",
            )
        })?;
    schema
        .ensure_index_compatible(model)
        .map_err(|error| error.with_help("Rebuild the index with `zg --index --rebuild`."))
}

fn resolve_scan(
    existing: Option<&crate::domain::ScanRules>,
    options: &IndexOptions,
) -> Result<crate::domain::ScanRules, EngineError> {
    let scan = if options.reset_paths {
        crate::domain::ScanRules::default()
    } else {
        existing.cloned().unwrap_or_default()
    };
    options.scan.apply(&scan)
}

fn saved_embedding_config(
    layout: &WorkspaceLayout,
    existing: Option<&Workspace>,
    options: &IndexOptions,
    model: &ModelRuntimeLease,
) -> Result<ModelConfig<EncryptedApiKey>, EngineError> {
    let reference = model.info().model().reference();
    let mut stored = existing
        .and_then(|workspace| {
            workspace
                .index()
                .descriptor()
                .and_then(|index| index.embedding_configs().get(&reference))
                .cloned()
        })
        .unwrap_or_default();
    if model.info().model().provider() == "local" {
        stored.device = options
            .device
            .or_else(|| {
                options
                    .embedding
                    .as_ref()
                    .map(|embedding| embedding.device)
                    .filter(|device| *device != Device::Auto)
            })
            .or(stored.device);
        stored.cache_dir = options
            .model_cache
            .clone()
            .or_else(|| {
                options
                    .embedding
                    .as_ref()
                    .and_then(|embedding| embedding.cache_dir.clone())
            })
            .or(stored.cache_dir);
        return Ok(stored);
    }
    stored.endpoint = model
        .configured_endpoint()
        .map(str::to_owned)
        .or(stored.endpoint);
    if let Some(key) = options.api_key.as_deref() {
        stored.api_key = Some(encrypt_api_key(layout, &reference, key)?);
    }
    Ok(stored)
}

fn unindexed_info(
    source_root: &Path,
    layout: &WorkspaceLayout,
    policy: WorkspaceIndexPolicy,
) -> InfoResult {
    InfoResult {
        compatibility: IndexCompatibility::Unbuilt,
        root: source_root.to_path_buf(),
        indexed: false,
        index_policy: policy,
        home: layout.storage_dir().to_path_buf(),
        index_path: layout.index_dir().to_path_buf(),
        source: InfoSource::Unindexed,
        workspace_index: None,
        status: None,
        suggestion: Some("run zg --index to create a workspace index".to_owned()),
    }
}

fn workspace_suggestion(workspace: &Workspace, indexed: bool) -> Option<String> {
    if workspace.index() == &IndexState::Disabled {
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::{
        api::{
            index::{
                IndexOptions,
                options::{EmbeddingModelSpec, ScanRulesUpdate},
            },
            info::InfoOptions,
        },
        domain::{IndexState, Workspace, WorkspaceLayout, model::Device},
    };

    use super::{ModelRuntimeManager, WorkspaceIndexService};

    #[test]
    fn normalizes_selected_model_cache_paths() {
        let mut options = IndexOptions {
            runtime_model_cache: Some("temporary-models".into()),
            embedding: Some(EmbeddingModelSpec {
                reference: "local/potion-code-16m-v2".into(),
                revision: None,
                cache_dir: Some("embedding-models".into()),
                endpoint: None,
                device: Device::Auto,
            }),
            ..IndexOptions::default()
        };
        super::normalize_model_paths(&mut options).expect("normalize cache paths");
        for (actual, relative) in [
            (options.runtime_model_cache.as_deref(), "temporary-models"),
            (
                options
                    .embedding
                    .as_ref()
                    .and_then(|model| model.cache_dir.as_deref()),
                "embedding-models",
            ),
        ] {
            assert_eq!(
                actual,
                Some(
                    std::path::absolute(relative)
                        .expect("absolute path")
                        .as_path()
                )
            );
        }

        options.model_cache = Some("saved-models".into());
        options.embedding.as_mut().expect("embedding").cache_dir = Some(PathBuf::new());
        super::normalize_model_paths(&mut options).expect("ignore overridden empty cache path");
        assert_eq!(
            options.model_cache,
            Some(std::path::absolute("saved-models").expect("absolute cache path"))
        );
        assert_eq!(
            options.embedding.expect("embedding").cache_dir,
            Some(PathBuf::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_unicode_saved_cache_paths_but_allows_temporary_overrides() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let cache_dir = std::path::PathBuf::from(OsString::from_vec(b"models-\xff".to_vec()));
        let embedding = EmbeddingModelSpec {
            reference: "local/potion-code-16m-v2".into(),
            revision: None,
            cache_dir: Some(cache_dir.clone()),
            endpoint: None,
            device: Device::Auto,
        };
        for mut options in [
            IndexOptions {
                model_cache: Some(cache_dir.clone()),
                ..Default::default()
            },
            IndexOptions {
                embedding: Some(embedding.clone()),
                ..Default::default()
            },
        ] {
            let error = super::normalize_model_paths(&mut options).expect_err("saved cache path");
            assert!(
                error
                    .message()
                    .contains("cannot save model cache directory")
            );
            assert!(error.message().contains("not valid Unicode"));
        }

        let mut temporary = IndexOptions {
            runtime_model_cache: Some(cache_dir.clone()),
            ..Default::default()
        };
        super::normalize_model_paths(&mut temporary).expect("temporary native path");
        assert_eq!(
            temporary.runtime_model_cache,
            Some(std::path::absolute(&cache_dir).expect("absolute native path"))
        );

        let mut overridden = IndexOptions {
            model_cache: Some("saved-models".into()),
            embedding: Some(embedding),
            ..Default::default()
        };
        super::normalize_model_paths(&mut overridden).expect("explicit cache takes precedence");
    }

    fn build_target(active: &Workspace) -> Workspace {
        Workspace::new(
            active.source_root(),
            active.layout().clone(),
            active.scan().clone(),
            active.index_version(),
            Some(uuid::Uuid::new_v4().to_string()),
            active.created_epoch_ms(),
            active.updated_epoch_ms(),
            active.index().clone(),
        )
        .expect("build target")
    }

    fn write_previous_index_version(home: &std::path::Path, manifest: &crate::domain::Workspace) {
        let mut previous = manifest.clone();
        previous = crate::domain::Workspace::new(
            previous.source_root(),
            previous.layout().clone(),
            previous.scan().clone(),
            Some(super::CURRENT_INDEX_VERSION - 1),
            previous.storage_generation().map(str::to_owned),
            previous.created_epoch_ms(),
            previous.updated_epoch_ms(),
            previous.index().clone(),
        )
        .expect("workspace");
        std::fs::write(
            home.join("manifest.json"),
            serde_json::to_vec(&previous).expect("previous manifest JSON"),
        )
        .expect("previous index metadata");
    }

    #[test]
    fn saved_credentials_are_decrypted_only_when_selected_for_execution() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        std::fs::create_dir(layout.storage_dir()).expect("storage directory");
        let models = ModelRuntimeManager::new();
        let options = IndexOptions {
            root: Some(directory.path().to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/text-embedding-v4".into(),
                revision: None,
                device: Device::Auto,
                cache_dir: None,
                endpoint: None,
            }),
            api_key: Some("saved-private-token".into()),
            endpoint: Some("https://example.test/embeddings".into()),
            allow_remote: true,
            ..IndexOptions::default()
        };
        let acquired =
            super::acquire_index_models(&models, None, &options).expect("model metadata");
        let workspace = super::index_workspace_state(
            directory.path(),
            &layout,
            None,
            &options,
            &acquired.runtimes,
            &acquired.descriptor,
            crate::domain::ScanRules::default(),
            true,
        )
        .expect("workspace");
        super::write_workspace_manifest(layout.manifest_path(), &workspace).expect("save");
        let bytes = std::fs::read_to_string(layout.manifest_path()).expect("manifest");
        assert!(!bytes.contains("saved-private-token"));
        std::fs::remove_file(layout.credentials_key_path()).expect("remove key");
        let loaded = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("metadata does not decrypt")
        .expect("workspace");
        assert_eq!(loaded, workspace);
        let mut request = IndexOptions {
            root: options.root.clone(),
            allow_remote: true,
            ..IndexOptions::default()
        };
        let error = super::acquire_model(&models, Some(&loaded), &request)
            .err()
            .expect("selected saved key must decrypt");
        assert!(error.message().contains("credentials.key"));
        request.runtime_api_key = Some("override-private-token".into());
        let model = super::acquire_model(&models, Some(&loaded), &request)
            .expect("runtime key bypasses missing saved key");
        let stored = super::saved_embedding_config(&layout, Some(&loaded), &request, &model)
            .expect("runtime key preserves saved settings");
        assert_eq!(
            Some(&stored),
            loaded
                .index()
                .descriptor()
                .expect("descriptor")
                .embedding_configs()
                .get("qwen/text-embedding-v4")
        );
        assert_eq!(
            std::fs::read_to_string(layout.manifest_path()).expect("unchanged manifest"),
            bytes
        );
        assert!(!layout.credentials_key_path().exists());
        models.close();
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
        let manifest = Workspace::new(
            directory.path(),
            WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
            ScanRulesUpdate {
                globs: Some(vec!["*.rs".into()]),
                include_hidden: Some(true),
                max_depth: Some(Some(3)),
                ..ScanRulesUpdate::default()
            }
            .apply(&crate::domain::ScanRules::default())
            .expect("scan rules"),
            None,
            None,
            0,
            0,
            IndexState::Uninitialized,
        )
        .expect("manifest");
        let mut options = IndexOptions::default();
        assert_eq!(
            super::resolve_scan(Some(manifest.scan()), &options).expect("scan rules"),
            manifest.scan().clone()
        );
        options.scan.globs = Some(Vec::new());
        options.scan.include_hidden = Some(false);
        options.scan.max_depth = Some(None);
        let scan = super::resolve_scan(Some(manifest.scan()), &options).expect("scan rules");
        assert!(scan.globs().is_empty());
        assert!(!scan.include_hidden());
        assert_eq!(scan.max_depth(), None);
        options = IndexOptions {
            reset_paths: true,
            ..IndexOptions::default()
        };
        assert_eq!(
            super::resolve_scan(Some(manifest.scan()), &options).expect("scan rules"),
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
        assert!(error.message().contains("path must be a directory"));
        assert_eq!(models.snapshot().cached_runtimes, 0);
    }

    #[tokio::test]
    async fn completion_progress_is_sent_only_after_successful_publication() {
        use crate::api::index::progress::{
            DETAIL_FINALIZING_INDEX, DETAIL_SAVING_INDEX, IndexProgressPhase, IndexProgressReporter,
        };
        for fail in [false, true] {
            let directory = tempdir().expect("workspace");
            let mut service = WorkspaceIndexService::new();
            service.fail_index_completion = fail;
            let models = ModelRuntimeManager::new();
            let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
            let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
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
                            if progress.detail.as_deref() == Some(DETAIL_SAVING_INDEX) {
                                assert_eq!(progress.phase, IndexProgressPhase::Indexing);
                                assert!(!layout.manifest_path().exists());
                            }
                            if progress.phase == IndexProgressPhase::Done {
                                assert!(
                                    super::read_workspace_manifest(
                                        layout.storage_dir().parent().expect("source root"),
                                        &layout
                                    )
                                    .expect("published metadata")
                                    .is_some()
                                );
                            }
                            if matches!(
                                progress.detail.as_deref(),
                                Some(DETAIL_FINALIZING_INDEX | DETAIL_SAVING_INDEX)
                            ) || progress.phase == IndexProgressPhase::Done
                            {
                                captured.lock().expect("observed progress").push(
                                    serde_json::to_value(&progress).expect("progress wire value")
                                        ["detail"]
                                        .clone(),
                                );
                            }
                        })),
                        ..Default::default()
                    },
                )
                .await;
            assert_eq!(result.is_err(), fail);
            assert_eq!(
                *observed.lock().expect("observed progress"),
                if fail {
                    vec![serde_json::json!("finalizing index")]
                } else {
                    vec![
                        serde_json::json!("finalizing index"),
                        serde_json::json!("saving index"),
                        serde_json::json!("indexing complete"),
                    ]
                }
            );
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
        let before = super::read_workspace_manifest(
            &original,
            &WorkspaceLayout::new(original.join(".zvec-grep")).expect("layout"),
        )
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
        let after = super::read_workspace_manifest(
            &moved,
            &WorkspaceLayout::new(moved.join(".zvec-grep")).expect("layout"),
        )
        .expect("manifest read")
        .expect("manifest");
        assert_eq!(after.scan(), before.scan());
        assert_eq!(
            after.source_root(),
            moved.canonicalize().expect("moved root")
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_rejects_symlinked_storage_without_removing_external_data() {
        use std::{fs, os::unix::fs::symlink};

        for name in ["credentials.key", "temp"] {
            let directory = tempdir().expect("workspace");
            let external = tempdir().expect("external data");
            let home = directory.path().join(".zvec-grep");
            fs::create_dir_all(home.join("index")).expect("index directory");
            fs::write(home.join("manifest.json"), b"saved manifest").expect("manifest");
            fs::write(home.join("index/keep"), b"saved index").expect("index data");
            fs::write(external.path().join("keep"), b"external data").expect("external data");
            symlink(external.path(), home.join(name)).expect("cleanup target alias");

            let error = WorkspaceIndexService::new()
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .await
                .expect_err("reject storage alias");

            assert!(error.message().contains("symbolic link"), "{name}");
            assert_eq!(
                fs::read(external.path().join("keep")).expect("external data"),
                b"external data"
            );
        }
    }

    #[tokio::test]
    async fn dropping_orphaned_index_storage_does_not_require_a_manifest() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir().to_path_buf();
        let storage = home.join("index");
        std::fs::create_dir_all(&storage).expect("orphaned index");
        let service = WorkspaceIndexService::new();
        let options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(!home.join("manifest.json").exists());
        assert!(!layout.has_build_record().expect("build record"));
        assert!(
            service
                .drop_index(&options)
                .await
                .expect("drop orphaned storage")
        );
        assert!(!storage.exists());
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let active = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
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
            super::read_workspace_manifest(
                layout.storage_dir().parent().expect("source root"),
                &layout
            )
            .expect("manifest"),
            Some(active.clone())
        );
        assert!(active.layout().index_dir().exists());
        assert!(!layout.has_build_record().expect("build record"));
        let refresh = crate::pipelines::indexed_search::service::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
        );
        service
            .index(&models, refresh)
            .await
            .expect("ordinary refresh updates active index");
        let updated = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest")
        .expect("active");
        assert_eq!(updated.storage_generation(), active.storage_generation());
        assert!(active.layout().index_dir().exists());
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
        let rebuilt = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest")
        .expect("active");
        assert_ne!(rebuilt.storage_generation(), active.storage_generation());
        assert_eq!(rebuilt.layout().index_dir(), active.layout().index_dir());
        assert!(rebuilt.layout().index_dir().exists());
        models.close();
    }

    #[tokio::test]
    async fn model_preferences_save_index_changes_but_not_query_overrides() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = super::read_workspace_manifest(directory.path(), &layout)
            .expect("read")
            .expect("manifest");
        let request = crate::api::context::ContextOptions {
            device: Some(Device::Auto),
            model_cache: Some(directory.path().join("temporary-models")),
            ..Default::default()
        };
        let options = crate::pipelines::indexed_search::service::refresh_options(
            &request,
            directory.path().to_path_buf(),
        );
        service.index(&models, options).await.expect("refresh");
        let refreshed = super::read_workspace_manifest(directory.path(), &layout)
            .expect("read")
            .expect("manifest");
        assert_eq!(
            refreshed
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs(),
            manifest
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs()
        );
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    device: Some(Device::Auto),
                    model_cache: Some(directory.path().join("saved-models")),
                    ..Default::default()
                },
            )
            .await
            .expect("explicit configuration update");
        let configured = super::read_workspace_manifest(directory.path(), &layout)
            .expect("read")
            .expect("manifest");
        assert_eq!(
            configured.storage_generation(),
            manifest.storage_generation()
        );
        let saved = serde_json::to_value(&configured).expect("manifest serialization");
        for runtime in saved["index"]["descriptor"]["embeddingConfigs"]
            .as_object()
            .expect("runtimes")
            .values()
        {
            assert_eq!(runtime["device"], "auto");
            assert_eq!(
                runtime["cacheDir"],
                serde_json::json!(directory.path().join("saved-models"))
            );
        }
        let replacement = directory.path().join("replacement-models");
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    model_cache: Some(replacement.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect("replace saved cache preference");
        let replaced = super::read_workspace_manifest(directory.path(), &layout)
            .expect("read")
            .expect("manifest");
        assert_eq!(
            replaced.storage_generation(),
            configured.storage_generation()
        );
        for config in replaced
            .index()
            .descriptor()
            .expect("index")
            .embedding_configs()
            .values()
        {
            assert_eq!(config.cache_dir.as_ref(), Some(&replacement));
        }
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let active = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("active");
        let mut target = build_target(&active);
        let mut globs = target.scan().globs().to_vec();
        globs.push("*.rs".into());
        target = crate::domain::Workspace::new(
            target.source_root(),
            target.layout().clone(),
            ScanRulesUpdate {
                globs: Some(globs),
                ..ScanRulesUpdate::default()
            }
            .apply(target.scan())
            .expect("scan rules"),
            target.index_version(),
            target.storage_generation().map(str::to_owned),
            target.created_epoch_ms(),
            target.updated_epoch_ms(),
            target.index().clone(),
        )
        .expect("workspace");
        target = {
            let index = target.index().descriptor().expect("index");
            let mut configs = index.embedding_configs().clone();
            configs.values_mut().next().expect("runtime").endpoint =
                Some("https://abandoned.test/embeddings".into());
            let index = crate::domain::IndexDescriptor::new(
                index.default_embedding_model().clone(),
                index.embedding_routes().clone(),
                configs,
                index.fts(),
            )
            .expect("descriptor");
            crate::domain::Workspace::new(
                target.source_root(),
                target.layout().clone(),
                target.scan().clone(),
                target.index_version(),
                target.storage_generation().map(str::to_owned),
                target.created_epoch_ms(),
                target.updated_epoch_ms(),
                crate::domain::IndexState::Enabled(Box::new(index)),
            )
            .expect("workspace")
        };
        let abandoned = target;
        super::prepare_build(&abandoned).expect("crashed build");
        std::fs::write(
            abandoned
                .layout()
                .staged_index_dir()
                .join("unfinished-data"),
            "discard me",
        )
        .expect("unfinished build data");
        service
            .index(&models, options.clone())
            .await
            .expect("ordinary index uses active settings");
        let updated = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("active");
        assert_eq!(updated.storage_generation(), active.storage_generation());
        assert_eq!(updated.scan(), active.scan());
        assert_eq!(
            updated
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs(),
            active
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs()
        );
        assert!(!abandoned.layout().staged_index_dir().exists());
        assert!(!layout.has_build_record().expect("build record"));
        models.close();
    }

    #[tokio::test]
    async fn explicit_rebuild_starts_over_after_a_completed_publication() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let active = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("active");
        let published = build_target(&active);
        super::prepare_build(&published).expect("build");
        super::publish_build(published.clone(), super::epoch_millis())
            .expect("publish completed build");
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
        let rebuilt = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("active");
        assert_ne!(rebuilt.storage_generation(), published.storage_generation());
        assert_eq!(active.layout().index_dir(), rebuilt.layout().index_dir());
        assert!(!published.layout().staged_index_dir().exists());
        assert!(rebuilt.layout().index_dir().exists());
        assert!(!layout.has_build_record().expect("build record"));
        models.close();
    }

    #[tokio::test]
    async fn failed_finalization_does_not_publish_an_incomplete_rebuild() {
        let directory = tempdir().expect("workspace");
        let mut service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let layout = WorkspaceLayout::new(
            super::resolve_directory(Some(directory.path()))
                .expect("root")
                .join(".zvec-grep"),
        )
        .expect("layout");
        let active = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
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
            .expect_err("finalization fails");
        assert!(error.message().contains("finalization"));
        assert_eq!(
            super::read_workspace_manifest(
                layout.storage_dir().parent().expect("source root"),
                &layout
            )
            .expect("manifest"),
            Some(active.clone())
        );
        assert!(active.layout().index_dir().exists());
        assert!(!layout.has_build_record().expect("build record"));
        // The unchanged active index does not need a query-driven refresh.
        assert!(
            !service
                .workspace_needs_refresh(
                    directory.path(),
                    &layout,
                    &super::LockWait::new(None, None).expect("wait"),
                    None,
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
        assert!(!layout.has_build_record().expect("build record"));
        assert!(!active.layout().index_dir().exists());
        models.close();
    }

    #[tokio::test]
    async fn cancelled_refresh_scan_releases_its_read_lock() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let layout = WorkspaceLayout::new(
            super::resolve_directory(Some(directory.path()))
                .expect("root")
                .join(".zvec-grep"),
        )
        .expect("layout");
        let signal = super::CancellationToken::new();
        signal.cancel();
        // Admission stays uncancelled so this exercises the scanner's token.
        let wait = super::LockWait::new(None, None).expect("wait");
        let error = service
            .workspace_needs_refresh(directory.path(), &layout, &wait, Some(signal))
            .await
            .expect_err("cancelled scan");
        assert_eq!(error.code(), crate::EngineError::CANCELLED);
        let _writer = super::LockWait::new(None, Some(0))
            .expect("immediate acquisition")
            .acquire(
                directory.path(),
                &layout,
                super::LockMode::Exclusive,
                "after cancelled scan",
            )
            .await
            .expect("scan releases its storage and read lock");
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
    async fn automatic_refresh_does_not_create_a_missing_index() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let refresh = crate::pipelines::indexed_search::service::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
        );
        let error = service
            .index(&models, refresh)
            .await
            .expect_err("refresh needs an existing index");
        assert_eq!(error.code(), crate::EngineError::NOT_FOUND);
        assert!(!directory.path().join(".zvec-grep").exists());
        models.close();
    }

    #[tokio::test]
    async fn automatic_refresh_rechecks_index_after_waiting_for_a_writer() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let writer = super::LockWait::new(None, None)
            .expect("lock wait")
            .acquire(
                directory.path(),
                &layout,
                super::LockMode::Exclusive,
                "fixture writer",
            )
            .await
            .expect("hold writer admission");
        let options = crate::pipelines::indexed_search::service::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
        );
        let refresh = service.index(&models, options);
        tokio::pin!(refresh);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), refresh.as_mut())
                .await
                .is_err(),
            "refresh must wait for the writer"
        );
        std::fs::remove_dir_all(layout.storage_dir()).expect("delete queued refresh's index");
        drop(writer);

        let error = refresh.await.expect_err("index disappeared while queued");
        assert_eq!(error.code(), crate::EngineError::NOT_FOUND);
        assert!(!layout.manifest_path().exists());
        models.close();
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
        let parent_layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        let parent_home = parent_layout.storage_dir().to_path_buf();
        let parent_bytes =
            std::fs::read(parent_home.join("manifest.json")).expect("parent manifest");
        let parent = super::read_workspace_manifest(
            parent_layout.storage_dir().parent().expect("source root"),
            &parent_layout,
        )
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
        let child_layout = WorkspaceLayout::new(child.join(".zvec-grep")).expect("layout");
        let indexed = super::read_workspace_manifest(
            child_layout.storage_dir().parent().expect("source root"),
            &child_layout,
        )
        .expect("child manifest read")
        .expect("child owns its manifest");
        assert_eq!(
            indexed.source_root(),
            child.canonicalize().expect("child root")
        );
        assert_eq!(
            indexed
                .index()
                .descriptor()
                .map(crate::domain::IndexDescriptor::default_embedding_model)
                .expect("child model")
                .model()
                .reference(),
            model
        );
        assert_eq!(
            indexed.scan().globs(),
            vec![crate::domain::GlobRule::from("src/**")]
        );
        assert_eq!(
            std::fs::read(parent_home.join("manifest.json")).expect("parent manifest"),
            parent_bytes
        );
        assert!(
            super::directory_exists(parent.layout().index_dir()).expect("parent storage remains")
        );
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
    async fn dropping_a_deleted_source_directory_reports_missing_without_recreating_it() {
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
        for _ in 0..2 {
            let error = service
                .drop_index(&options)
                .await
                .expect_err("source directory is missing");
            assert_eq!(error.code(), crate::EngineError::NOT_FOUND);
            assert!(!deleted.exists());
        }
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
        std::fs::create_dir_all(copy.join(".zvec-grep/temp")).expect("copy home");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(&original))
            .await
            .expect("create workspace fixture");
        let layout = WorkspaceLayout::new(original.join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir().to_path_buf();
        let manifest = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("manifest");
        super::prepare_build(&build_target(&manifest)).expect("simulate crashed first build");
        std::fs::remove_file(home.join("manifest.json")).expect("unpublished fixture");
        let original_build = original.join(".zvec-grep/temp/build.json");
        let original_bytes = std::fs::read(&original_build).expect("original pending build");
        assert!(!original.join(".zvec-grep/manifest.json").exists());
        std::fs::copy(&original_build, copy.join(".zvec-grep/temp/build.json"))
            .expect("copy pending metadata");
        service
            .index(&models, empty_index_options(&copy))
            .await
            .expect("a copy is addressed by its own root");
        let copied = super::read_workspace_manifest(
            &copy,
            &WorkspaceLayout::new(copy.join(".zvec-grep")).expect("layout"),
        )
        .expect("copied manifest")
        .expect("published copy");
        assert_eq!(
            copied.source_root(),
            copy.canonicalize().expect("copy root")
        );
        assert_eq!(
            std::fs::read(original_build).expect("original build unchanged"),
            original_bytes
        );
        models.close();
    }

    #[tokio::test]
    async fn index_normalizes_requested_ignore_paths_before_saving() {
        let directory = tempdir().expect("workspace");
        let root = directory.path().join("source");
        std::fs::create_dir_all(root.join("config")).expect("source directory");
        let root = root.canonicalize().expect("source root");
        let external = root.parent().expect("parent").join("external.ignore");
        std::fs::write(&external, "").expect("external rules");
        std::fs::write(root.join("config/rules.ignore"), "*\n").expect("internal rules");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    scan: ScanRulesUpdate {
                        ignore_files: Some(vec![
                            "../external.ignore".into(),
                            root.join("config/rules.ignore"),
                            "missing.ignore".into(),
                            "../missing.ignore".into(),
                        ]),
                        ..ScanRulesUpdate::default()
                    },
                    ..empty_index_options(&root)
                },
            )
            .await
            .expect("index with normalized ignore paths");
        let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        let manifest = super::read_workspace_manifest(&root, &layout)
            .expect("read manifest")
            .expect("published workspace");
        let missing_external = external.with_file_name("missing.ignore");
        assert_eq!(
            manifest.scan().ignore_files().collect::<Vec<_>>(),
            [
                external.as_path(),
                Path::new("config/rules.ignore"),
                Path::new("missing.ignore"),
                missing_external.as_path(),
            ]
        );
        models.close();
    }

    #[tokio::test]
    async fn zero_size_limit_persists_and_indexes_no_files() {
        let directory = tempdir().expect("workspace");
        std::fs::write(directory.path().join("source.txt"), "source text").expect("source file");
        std::fs::write(directory.path().join("empty.txt"), "").expect("empty file");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let result = service
            .index(
                &models,
                IndexOptions {
                    scan: ScanRulesUpdate {
                        max_file_size_bytes: Some(Some(0)),
                        ..ScanRulesUpdate::default()
                    },
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("index with zero size limit");
        assert_eq!(result.files_added, 0);
        assert_eq!(result.files_failed, 0);
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = super::read_workspace_manifest(directory.path(), &layout)
            .expect("read manifest")
            .expect("published workspace");
        assert_eq!(manifest.scan().max_file_size_bytes(), Some(0));
        let result = service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("reuse saved zero limit");
        assert_eq!(result.files_added, 0);
        assert_eq!(result.files_failed, 0);
        models.close();
    }

    #[tokio::test]
    async fn invalid_scan_rules_do_not_create_index_or_acquire_models() {
        let directory = tempdir().expect("workspace");
        std::fs::write(directory.path().join("invalid.ignore"), "[z-a]\n")
            .expect("invalid ignore rules");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        for scan in [
            ScanRulesUpdate {
                globs: Some(vec!["[".into()]),
                ..ScanRulesUpdate::default()
            },
            ScanRulesUpdate {
                ignore_files: Some(vec!["invalid.ignore".into()]),
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
            let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
            let home = layout.storage_dir().to_path_buf();
            assert!(!home.join("manifest.json").exists());
            assert!(!layout.has_build_record().expect("build record"));
            assert!(!layout.has_index_data().expect("no index data"));
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let active = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("read")
        .expect("active index");
        let pending = build_target(&active);
        super::prepare_build(&pending).expect("pending rebuild");
        std::fs::write(directory.path().join("invalid.ignore"), "[z-a]\n")
            .expect("invalid ignore rules");
        let error = service
            .index(
                &models,
                IndexOptions {
                    scan: ScanRulesUpdate {
                        ignore_files: Some(vec!["invalid.ignore".into()]),
                        ..ScanRulesUpdate::default()
                    },
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("invalid explicit ignore rules");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            super::read_workspace_manifest(
                layout.storage_dir().parent().expect("source root"),
                &layout
            )
            .expect("unchanged manifest"),
            Some(active.clone())
        );
        assert!(layout.has_build_record().expect("build record"));
        assert!(pending.layout().staged_index_dir().exists());
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir().to_path_buf();
        assert!(!home.join("manifest.json").exists());
        assert!(!layout.has_build_record().expect("build record"));
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
        let manifest = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest")
        .expect("index");
        assert_eq!(
            manifest.source_root(),
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir().to_path_buf();
        assert!(!home.join("manifest.json").exists());
        assert!(!layout.has_build_record().expect("build record"));
        assert!(!layout.has_index_data().expect("no abandoned index data"));
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("fresh initial build");
        assert!(
            super::read_workspace_manifest(
                layout.storage_dir().parent().expect("source root"),
                &layout
            )
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest")
        .expect("index");
        let serialized = serde_json::to_string(&manifest).expect("manifest serialization");
        assert!(!serialized.contains("embedding_concurrency"));
        assert!(!serialized.contains("embeddingConcurrency"));
        let default = crate::api::context::ContextOptions {
            model_cache: Some(directory.path().join("model-cache")),
            ..Default::default()
        };
        let first = crate::pipelines::indexed_search::service::acquire_search_model(
            &models, &manifest, &default,
        )
        .expect("query default");
        let second = crate::pipelines::indexed_search::service::acquire_search_model(
            &models,
            &manifest,
            &crate::api::context::ContextOptions {
                embedding_concurrency: Some(24),
                ..default
            },
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
        std::fs::create_dir(directory.path().join("model-cache")).expect("saved model cache");
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
        let layout = WorkspaceLayout::new(info.root.join(".zvec-grep")).expect("layout");
        let manifest = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest read")
        .expect("manifest");
        let serialized = serde_json::to_value(&manifest).expect("manifest serialization");
        assert_eq!(
            serialized["index"]["descriptor"]["embeddingConfigs"]["local/potion-code-16m-v2"]["cacheDir"],
            serde_json::json!(directory.path().join("model-cache"))
        );
        let lease = crate::pipelines::indexed_search::service::acquire_search_model(
            &models,
            &manifest,
            &crate::api::context::ContextOptions::default(),
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
        let rebuilt = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest read")
        .expect("manifest");
        assert_eq!(rebuilt.index_version(), Some(super::CURRENT_INDEX_VERSION));
        assert_eq!(rebuilt.source_root(), manifest.source_root());
        assert_eq!(rebuilt.scan(), manifest.scan());
        assert_eq!(
            rebuilt
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs(),
            manifest
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs()
        );
        assert!(rebuilt.created_epoch_ms() >= manifest.created_epoch_ms());
        assert_ne!(rebuilt.storage_generation(), manifest.storage_generation());

        assert!(service.drop_index(&info_options).await.expect("drop index"));
        assert!(
            !super::directory_exists(rebuilt.layout().index_dir()).expect("storage was deleted")
        );
        models.close();
    }
}
