use std::{
    collections::BTreeMap,
    env, fmt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use zg_host_native::NativeScanner;

use crate::pipelines::indexed_search::writer::WriterSession;
use crate::{
    EngineError,
    api::{
        index::{
            IndexOptions, IndexResult,
            options::EmbeddingModelSpec,
            progress::{
                DETAIL_SAVING_INDEX, IndexProgress, IndexProgressPhase, IndexProgressReporter,
            },
        },
        info::{
            InfoOptions, InfoResult,
            result::{IndexCompatibility, InfoSource, WorkspaceIndexInfo, WorkspaceIndexPolicy},
        },
    },
    domain::{
        EmbeddingModelInfo, IndexDescriptor, IndexState, Workspace, WorkspaceLayout,
        model::{Device, EncryptedApiKey, HttpAuth, HttpEmbeddingConfig, ModelConfig},
    },
    models::{
        ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest,
        resolve_index_concurrency,
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

use super::{
    embedding_policy,
    input_budget::embedding_input_budget,
    pipeline::{
        IndexEmbeddingRuntime, IndexModelProvider, IndexingContext, get_workspace_index_status,
        index_workspace,
    },
    routing::EmbeddingPlan,
};

#[derive(Clone)]
pub(crate) struct WorkspaceIndexService {
    pub(crate) writers: crate::pipelines::indexed_search::writer::WriterContexts,
    scanner: NativeScanner,
    #[cfg(test)]
    fail_index_completion: bool,
    #[cfg(test)]
    fragment_count_reads: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    fail_fragment_count: bool,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            writers: crate::pipelines::indexed_search::writer::WriterContexts::default(),
            scanner: NativeScanner::default(),
            #[cfg(test)]
            fail_index_completion: false,
            #[cfg(test)]
            fragment_count_reads: Arc::default(),
            #[cfg(test)]
            fail_fragment_count: false,
        }
    }

    /// Runs the indexing pipeline in one linear sequence: recovery, model
    /// resolution, build, and publication are order-dependent, so the steps
    /// stay visible here instead of behind helpers.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn index(
        &self,
        models: &ModelRuntimeManager,
        mut options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        options.normalize_model_cache()?;
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
        let scan = indexing_scan(&requested_root, existing.as_ref(), &mut options)?;
        crate::file_selection::ScanPolicy::new(&requested_root, &scan)?;
        if options.rebuild {
            recover_build_for_rebuild(&layout)?;
        } else {
            recover_build(&layout)?;
        }
        let rebuilding =
            options.rebuild || !existing.as_ref().is_some_and(Workspace::index_enabled);
        let selection =
            embedding_policy::select(&requested_root, &scan, existing.as_ref(), &options).await?;
        if options.embedding.is_none() {
            if let Some(reporter) = options.on_progress.take() {
                let explanation = selection.clone();
                // Daemon clients consume coalesced snapshots. Keep the decision on
                // every event, including completion, rather than a lossy one-off event.
                let reporter = IndexProgressReporter::new(move |mut progress| {
                    progress.embedding.get_or_insert_default().selection =
                        Some(explanation.clone());
                    reporter.report(progress);
                });
                reporter.report(IndexProgress {
                    phase: IndexProgressPhase::Scanning,
                    files_total: None,
                    files_indexed: None,
                    files_failed: None,
                    detail: None,
                    embedding: None,
                });
                options.on_progress = Some(reporter);
            }
            // Freeze this operation's choice before acquiring models. Auto device
            // and absent runtime overrides preserve the saved workspace settings.
            options.embedding = Some(EmbeddingModelSpec {
                reference: selection.model,
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Auto,
            });
        }
        let plan = EmbeddingPlan::new(
            existing.as_ref(),
            &options,
            &embedding_reference(existing.as_ref(), &options)?,
        )?;
        let descriptor = &plan.proposed;
        if !rebuilding
            && let Some(previous) = existing
                .as_ref()
                .and_then(|workspace| workspace.index().descriptor())
        {
            previous
                .ensure_index_compatible(descriptor)
                .map_err(|error| {
                    error.with_help("Rerun the same indexing command with `--rebuild`.")
                })?;
        }
        let mut workspace = index_workspace_state(
            &requested_root,
            &layout,
            existing.as_ref(),
            &plan,
            &options,
            scan,
            rebuilding,
        )?;
        if rebuilding {
            prepare_build(&workspace)?;
        }
        let result = self
            .run_index(
                &workspace,
                rebuilding,
                models,
                plan.requests,
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

    /// Opens the pass storage, runs the pipeline, sweeps cross-file references,
    /// and publishes. The steps are order-dependent — storage closes before the
    /// manifest selects the new generation — so they stay visible in one place.
    #[allow(clippy::too_many_lines)]
    async fn run_index(
        &self,
        workspace: &Workspace,
        rebuilding: bool,
        models: &ModelRuntimeManager,
        requests: BTreeMap<String, IndexOptions>,
        options: &IndexOptions,
        home_lock: Arc<LockGuard>,
    ) -> Result<(IndexResult, Option<IndexProgress>), EngineError> {
        // PDF extraction can use local models even when embeddings are remote.
        let cache = index_model_cache(&crate::config::read()?, Some(workspace), options);
        let extractor = crate::extraction::ExtractionService::new(models.clone(), cache);
        let storage_path = if rebuilding {
            workspace.layout().staged_index_dir()
        } else {
            workspace.layout().index_dir()
        };
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            graph_enabled: workspace
                .index()
                .descriptor()
                .is_some_and(IndexDescriptor::graph_indexing),
            storage_path: storage_path.to_path_buf(),
            indexes: workspace
                .index()
                .descriptor()
                .expect("enabled index")
                .content_indexes(),
        })
        .map_err(|mut error| {
            if let Some(help) = error.help() {
                let help = format!(
                    "{help} Run this command in workspace '{}'.",
                    workspace.source_root().display()
                );
                error = error.with_help(help);
            }
            error.with_workspace_root(workspace.source_root())
        })?;
        let writer = self.writers.register(
            WriterSession::new(storage, requests, workspace.clone(), home_lock),
            !rebuilding,
        );
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
            extractor: &extractor,
            models: &IndexRuntimes {
                manager: models,
                writer: &writer.session,
            },
            embedding_concurrency: options.embedding_concurrency,
            on_progress: pipeline_progress,
            signal: options.signal.clone(),
            changes: &options.changes,
        })
        .await;
        // Cross-file resolution runs once per pass, after every file snapshot is
        // durable. A failed sweep degrades to "still pending": unresolved
        // references stay queued for the next pass instead of failing an index
        // whose zvec side has already been published. The sweep honors the
        // pass's signal, so a cancelled pass stops resolving, keeps the
        // remaining references queued, and releases this writer below.
        let result = result.inspect(|_indexed| {
            // An incremental pass patches the previous graph: rows it resolved
            // in an earlier pass are invisible to the sweep, so a changed name
            // candidate set or import path would never reach them. Requeue
            // them first. A rebuild writes a fresh graph and has none.
            if !rebuilding {
                match writer.session.storage.requeue_stale_resolutions() {
                    Ok(0) => {}
                    Ok(requeued) => tracing::debug!(requeued, "requeued stale references"),
                    Err(error) => {
                        tracing::warn!(%error, "reference requeue failed; resolutions stay stale");
                    }
                }
            }
            match super::graph_resolver::resolve_pending(
                &writer.session.storage,
                Some(workspace.source_root()),
                options.signal.as_ref(),
            ) {
                Ok(sweep) => tracing::debug!(sweep = ?sweep, "graph resolution sweep"),
                Err(error) if error.code() == EngineError::CANCELLED => {
                    // The cancellation itself is reported after storage closes;
                    // degrading here would double-report a failed sweep.
                    tracing::debug!(%error, "graph resolution sweep stopped for cancellation");
                }
                Err(error) => {
                    tracing::warn!(%error, "graph resolution sweep failed; references stay pending");
                }
            }
        });
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
                phase: crate::api::index::progress::IndexProgressPhase::Finalizing,
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
            graph_enabled: workspace
                .index()
                .descriptor()
                .is_some_and(IndexDescriptor::graph_indexing),
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
                ..
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
                graph_enabled: workspace
                    .index()
                    .descriptor()
                    .is_some_and(IndexDescriptor::graph_indexing),
                storage_path: workspace.layout().index_dir().to_path_buf(),
            })?;
            let status =
                get_workspace_index_status(&workspace, &storage, &self.scanner, None, true)
                    .await
                    .and_then(|mut status| {
                        if options.include_counts {
                            #[cfg(test)]
                            {
                                self.fragment_count_reads
                                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                                if self.fail_fragment_count {
                                    return Err(EngineError::storage_failure(
                                        "injected fragment count failure",
                                    ));
                                }
                            }
                            status.fragments_indexed = Some(storage.count_fragments()?);
                        }
                        Ok(status)
                    });
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
            workspace_index: Some(workspace_index_info(&workspace, &options, indexed)),
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

pub(crate) async fn resolve_index_embedding(
    mut options: IndexOptions,
) -> Result<crate::api::index::embedding::EmbeddingSelection, EngineError> {
    let root = resolve_directory(options.root.as_deref())?;
    let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
    let existing = load_index_workspace(&root, &layout, &options)?;
    let scan = indexing_scan(&root, existing.as_ref(), &mut options)?;
    crate::file_selection::ScanPolicy::new(&root, &scan)?;
    embedding_policy::select(&root, &scan, existing.as_ref(), &options).await
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

fn index_workspace_state(
    source_root: &Path,
    layout: &WorkspaceLayout,
    active: Option<&Workspace>,
    plan: &EmbeddingPlan,
    options: &IndexOptions,
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
    let configs = plan
        .requests
        .iter()
        .map(|(reference, options)| {
            Ok((
                reference.clone(),
                saved_embedding_config(
                    layout,
                    active,
                    options,
                    reference,
                    plan.active.contains(reference),
                    plan.proposed
                        .embedding_configs()
                        .get(reference)
                        .and_then(|config| config.http.as_ref()),
                )?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, EngineError>>()?;
    let descriptor = &plan.proposed;
    let descriptor = IndexDescriptor::new(
        descriptor.default_embedding_model().clone(),
        descriptor.embedding_routes().clone(),
        configs,
        descriptor.graph_indexing(),
        descriptor.fts(),
    )?;
    let workspace = Workspace::new(
        source_root,
        layout.clone(),
        scan,
        CURRENT_INDEX_VERSION,
        Some(generation),
        active.map_or(now, Workspace::created_epoch_ms),
        now,
        IndexState::Enabled(Box::new(descriptor)),
    )?;
    Ok(
        workspace.with_model_cache_dir(options.model_cache.clone().or_else(|| {
            active
                .and_then(Workspace::model_cache_dir)
                .map(Path::to_path_buf)
        })),
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

struct IndexRuntimes<'a> {
    manager: &'a ModelRuntimeManager,
    writer: &'a WriterSession,
}

impl IndexModelProvider for IndexRuntimes<'_> {
    fn acquire(&self, reference: &str) -> Result<&dyn IndexEmbeddingRuntime, EngineError> {
        let (options, slot) = &self.writer.models[reference];
        if let Some(lease) = slot.get() {
            return Ok(lease);
        }
        let lease = acquire_model(
            self.manager,
            Some(&self.writer.workspace),
            options,
            self.writer.http_config(reference),
        )?;
        Ok(slot.get_or_init(|| lease))
    }
}

impl WriterSession {
    fn http_config(&self, reference: &str) -> Option<&HttpEmbeddingConfig> {
        self.workspace
            .index()
            .descriptor()?
            .embedding_configs()
            .get(reference)?
            .http
            .as_ref()
    }

    pub(in crate::pipelines) fn matches_query_request(
        &self,
        request: &ModelRuntimeRequest,
    ) -> bool {
        let reference = request.reference();
        let Some((options, lease)) = self.models.get(reference) else {
            return false;
        };
        if let Some(lease) = lease.get() {
            return lease.matches_query_request(request);
        }
        // A writer may only update text while an existing code route serves queries.
        // Comparing its planned configuration neither loads it nor uploads documents.
        index_model_request(
            Some(&self.workspace),
            options,
            self.http_config(reference),
            |_, _| Ok(()),
        )
        .is_ok_and(|planned| planned.matches_query_request(request))
    }
}

fn index_model_cache(
    config: &serde_json::Value,
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Option<std::path::PathBuf> {
    crate::config::model_cache(
        config,
        options
            .runtime_model_cache
            .clone()
            .or_else(|| options.model_cache.clone()),
        existing
            .and_then(Workspace::model_cache_dir)
            .map(Path::to_path_buf),
    )
}

fn acquire_model(
    models: &ModelRuntimeManager,
    existing: Option<&Workspace>,
    options: &IndexOptions,
    http: Option<&HttpEmbeddingConfig>,
) -> Result<ModelRuntimeLease, EngineError> {
    let request = index_model_request(existing, options, http, |reference, endpoint| {
        let root = resolve_directory(options.root.as_deref())?;
        crate::authorization::require_with_targets(
            &root,
            reference,
            endpoint,
            options.allow_remote,
            &options.authorized_remote,
        )
    })?;
    models
        .acquire(request)
        .map_err(ModelError::into_engine_error)
}

fn index_model_request(
    existing: Option<&Workspace>,
    options: &IndexOptions,
    http: Option<&HttpEmbeddingConfig>,
    check_remote: impl FnOnce(&str, &str) -> Result<(), EngineError>,
) -> Result<ModelRuntimeRequest, EngineError> {
    let reference = embedding_reference(existing, options)?;
    let concurrency = resolve_index_concurrency(Some(&reference), options.embedding_concurrency)
        .map_err(ModelError::into_engine_error)?;
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
    let endpoint = options
        .endpoint
        .as_deref()
        .or_else(|| {
            options
                .embedding
                .as_ref()
                .and_then(|spec| spec.endpoint.as_deref())
        })
        .or_else(|| existing_runtime.and_then(|runtime| runtime.endpoint.as_deref()));
    let endpoint = if local {
        endpoint.map(str::to_owned)
    } else {
        let endpoint = crate::authorization::remote_endpoint(&reference, endpoint)?;
        check_remote(&reference, &endpoint)?;
        Some(endpoint)
    };
    let requested_key = options
        .runtime_api_key
        .as_deref()
        .or(options.api_key.as_deref());
    let api_key = resolve_remote_api_key(
        &config,
        &reference,
        http,
        existing,
        existing_runtime,
        requested_key,
        endpoint.as_deref(),
    )?;
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
    let cache_dir = if http.is_some() {
        None
    } else {
        index_model_cache(&config, existing, options)
    };
    Ok(ModelRuntimeRequest::new(
        reference,
        ModelConfig {
            http: http.cloned(),
            api_key,
            endpoint,
            cache_dir,
            device,
        },
        concurrency,
    ))
}

fn embedding_reference(
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Result<String, EngineError> {
    embedding_policy::configured_selection(existing, options)?
        .map(|selection| selection.model)
        .ok_or_else(|| {
            EngineError::internal("index embedding selection must precede model acquisition")
        })
}

pub(in crate::pipelines) fn resolve_remote_api_key(
    config: &serde_json::Value,
    reference: &str,
    http: Option<&HttpEmbeddingConfig>,
    workspace: Option<&Workspace>,
    saved_runtime: Option<&ModelConfig<EncryptedApiKey>>,
    explicit_key: Option<&str>,
    endpoint: Option<&str>,
) -> Result<Option<String>, EngineError> {
    let provider = reference.split('/').next().unwrap_or_default();
    if provider == "local" || http.is_some_and(|model| model.auth == HttpAuth::None) {
        return Ok(None);
    }
    if let Some(key) = explicit_key {
        return Ok(Some(key.to_owned()));
    }
    if let (Some(workspace), Some(saved), Some(endpoint)) = (workspace, saved_runtime, endpoint)
        && let Some(key) = saved_api_key_for_endpoint(reference, workspace, saved, endpoint)?
    {
        return Ok(Some(key));
    }
    if let Some(http) = http {
        return Ok(http.api_key_env.as_deref().and_then(non_empty_env_var));
    }
    Ok(
        crate::config::string(config, &["providers", provider, "apiKey"])
            .or_else(environment_api_key),
    )
}

fn non_empty_env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub(super) fn environment_api_key() -> Option<String> {
    ["ZVEC_GREP_API_KEY", "DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        .into_iter()
        .find_map(non_empty_env_var)
}

pub(in crate::pipelines) fn assert_embedding_compatible(
    existing: Option<&IndexDescriptor>,
    model: &EmbeddingModelInfo,
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

/// Use the same selection for consent and execution, including full scans after rule changes.
pub(crate) fn indexing_scan(
    root: &Path,
    existing: Option<&Workspace>,
    options: &mut IndexOptions,
) -> Result<crate::domain::ScanRules, EngineError> {
    options.scan.normalize_ignore_paths(root)?;
    if options.rebuild
        || !existing.is_some_and(Workspace::index_enabled)
        || options.reset_paths
        || options.scan != crate::api::index::options::ScanRulesUpdate::default()
    {
        options.changes.clear();
    }
    resolve_scan(existing.map(Workspace::scan), options)
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

/// Persisted API keys belong to one remote destination. Compare canonical URLs
/// so that an API base URL and its resolved embedding endpoint can share a key.
fn saved_key_matches_endpoint(
    reference: &str,
    stored: Option<&str>,
    selected: &str,
) -> Result<bool, EngineError> {
    match stored {
        Some(endpoint) if endpoint == selected => Ok(true),
        Some(endpoint) => {
            Ok(crate::authorization::remote_endpoint(reference, Some(endpoint))? == selected)
        }
        None => Ok(false),
    }
}

fn saved_api_key_for_endpoint(
    reference: &str,
    workspace: &Workspace,
    config: &ModelConfig<EncryptedApiKey>,
    endpoint: &str,
) -> Result<Option<String>, EngineError> {
    match config.api_key.as_ref() {
        Some(key)
            if saved_key_matches_endpoint(reference, config.endpoint.as_deref(), endpoint)? =>
        {
            Ok(Some(decrypt_api_key(key, workspace.layout(), reference)?))
        }
        _ => Ok(None),
    }
}

fn saved_embedding_config(
    layout: &WorkspaceLayout,
    existing: Option<&Workspace>,
    options: &IndexOptions,
    reference: &str,
    routed: bool,
    http: Option<&HttpEmbeddingConfig>,
) -> Result<ModelConfig<EncryptedApiKey>, EngineError> {
    let mut stored = existing
        .and_then(|workspace| {
            workspace
                .index()
                .descriptor()
                .and_then(|index| index.embedding_configs().get(reference))
                .cloned()
        })
        .unwrap_or_default();
    stored.cache_dir = None;
    if reference.starts_with("local/") {
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
        return Ok(stored);
    }
    let mut save_key = options.api_key.as_deref();
    if let Some(http) = http {
        if http.auth == HttpAuth::None {
            stored.api_key = None;
            save_key = None;
        }
        stored.http = Some(http.clone());
    }
    let requested_endpoint = options
        .endpoint
        .as_deref()
        .or_else(|| {
            options
                .embedding
                .as_ref()
                .and_then(|spec| spec.endpoint.as_deref())
        })
        .or(stored.endpoint.as_deref());
    // Save the configured destination even when this pass has no fragments for the route.
    if !routed && save_key.is_none() && stored.api_key.is_none() {
        stored.endpoint = requested_endpoint.map(str::to_owned);
        return Ok(stored);
    }
    let endpoint = crate::authorization::remote_endpoint(reference, requested_endpoint)?;
    if let Some(key) = save_key {
        stored.api_key = Some(encrypt_api_key(layout, reference, key)?);
    } else if stored.api_key.is_some()
        && !saved_key_matches_endpoint(reference, stored.endpoint.as_deref(), &endpoint)?
    {
        stored.api_key = None;
    }
    stored.endpoint = Some(endpoint);
    Ok(stored)
}

fn workspace_index_info(
    workspace: &Workspace,
    options: &InfoOptions,
    indexed: bool,
) -> WorkspaceIndexInfo {
    let mut info = WorkspaceIndexInfo::from_workspace(workspace);
    if options.include_status
        && options.include_counts
        && indexed
        && let (Some(embedding), Some(descriptor)) =
            (&mut info.embedding, workspace.index().descriptor())
    {
        embedding.input_budget =
            embedding_input_budget(descriptor.default_embedding_model().max_input_tokens());
        for index in &mut info.content_indexes {
            index.embedding.input_budget = descriptor
                .model_for(index.kind)
                .and_then(|model| embedding_input_budget(model.max_input_tokens()));
        }
    }
    info
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
        domain::{IndexDescriptor, IndexState, Workspace, WorkspaceLayout, model::Device},
    };

    use super::{ModelRuntimeManager, WorkspaceIndexService};

    #[test]
    fn extraction_model_cache_is_independent_of_embedding_backend() {
        let directory = tempdir().expect("cache preferences");
        let runtime = directory.path().join("runtime");
        let explicit = directory.path().join("explicit");
        let global = directory.path().join("global");
        let config = serde_json::json!({"defaults": {"modelCacheDir": global}});
        for reference in [
            "local/potion-code-16m-v2",
            "http/fixture",
            "qwen/text-embedding-v4",
        ] {
            let mut options = IndexOptions {
                runtime_model_cache: Some(runtime.clone()),
                model_cache: Some(explicit.clone()),
                embedding: Some(EmbeddingModelSpec {
                    reference: reference.into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: Device::Auto,
                }),
                ..Default::default()
            };
            let resolve = |options: &IndexOptions| super::index_model_cache(&config, None, options);
            assert_eq!(resolve(&options), Some(runtime.clone()), "{reference}");
            options.runtime_model_cache = None;
            assert_eq!(resolve(&options), Some(explicit.clone()), "{reference}");
            options.model_cache = None;
            assert_eq!(
                resolve(&options),
                std::env::var_os("ZVEC_GREP_MODEL_CACHE")
                    .map(PathBuf::from)
                    .or_else(|| Some(global.clone())),
                "{reference}",
            );
        }
    }

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
        options
            .normalize_model_cache()
            .expect("normalize cache paths");
        assert_eq!(
            options.runtime_model_cache,
            Some(std::path::absolute("temporary-models").expect("absolute temporary cache"))
        );
        assert_eq!(
            options.model_cache,
            Some(std::path::absolute("embedding-models").expect("absolute embedding cache"))
        );
        let default = options.embedding.as_ref().expect("default model");
        assert!(default.cache_dir.is_none());
        let reference = default.reference.clone();
        let other = "local/bekko-256-int8";
        options.model_cache = None;
        options.embedding_routes.insert(
            crate::domain::ContentKind::Code,
            EmbeddingModelSpec {
                reference: other.into(),
                cache_dir: Some("saved-models".into()),
                ..default.clone()
            },
        );
        let (_, preflight) = super::super::embedding_requests(None, &options, &reference)
            .expect("single routed cache applies to both models");
        let expected = Some(std::path::absolute("saved-models").expect("absolute shared cache"));
        assert_eq!(preflight[&reference].model_cache, expected);
        assert_eq!(preflight[other].model_cache, expected);
        options.embedding.as_mut().expect("default model").cache_dir = Some("another-cache".into());
        let error = options
            .normalize_model_cache()
            .expect_err("distinct model cache paths");
        assert!(
            error
                .message()
                .contains("conflicting model cache directories")
        );

        options.embedding.as_mut().expect("default model").cache_dir = None;
        options.model_cache = Some("another-cache".into());
        let error = options
            .normalize_model_cache()
            .expect_err("workspace and route disagree");
        assert!(
            error
                .message()
                .contains("conflicting model cache directories")
        );
        options.model_cache.clone_from(&expected);
        options
            .normalize_model_cache()
            .expect("matching workspace and route paths");
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
            let error = options
                .normalize_model_cache()
                .expect_err("saved cache path");
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
        temporary
            .normalize_model_cache()
            .expect("temporary native path");
        assert_eq!(
            temporary.runtime_model_cache,
            Some(std::path::absolute(&cache_dir).expect("absolute native path"))
        );

        let mut overridden = IndexOptions {
            model_cache: Some("saved-models".into()),
            embedding: Some(embedding),
            ..Default::default()
        };
        overridden
            .normalize_model_cache()
            .expect_err("all saved paths must be valid");
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
        let mut previous = serde_json::to_value(manifest).expect("manifest JSON");
        previous["indexVersion"] = serde_json::json!(super::CURRENT_INDEX_VERSION - 1);
        if let Some(cache) = previous
            .as_object_mut()
            .expect("manifest")
            .remove("modelCacheDir")
        {
            let reference = manifest
                .index()
                .descriptor()
                .expect("index")
                .default_embedding_model()
                .model()
                .reference();
            previous["index"]["descriptor"]["embeddingConfigs"][reference]["cacheDir"] = cache;
        }
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
        let plan = super::EmbeddingPlan::new(
            None,
            &options,
            &options
                .embedding
                .as_ref()
                .expect("explicit model")
                .reference,
        )
        .expect("model metadata");
        let workspace = super::index_workspace_state(
            directory.path(),
            &layout,
            None,
            &plan,
            &options,
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
        let mut query = crate::api::context::ContextOptions {
            allow_remote: true,
            ..Default::default()
        };
        let acquire_query = crate::pipelines::indexed_search::service::acquire_search_model;
        let error = acquire_query(&models, &loaded, &query)
            .err()
            .expect("query to the stored endpoint needs its saved key");
        assert!(error.message().contains("credentials.key"));
        query.endpoint = Some("https://new.example.test/v1".into());
        if let Err(error) = acquire_query(&models, &loaded, &query) {
            assert!(!error.message().contains("credentials.key"), "{error}");
        }
        let mut request = IndexOptions {
            root: options.root.clone(),
            allow_remote: true,
            ..IndexOptions::default()
        };
        let error = super::acquire_model(&models, Some(&loaded), &request, None)
            .err()
            .expect("selected saved key must decrypt");
        assert!(error.message().contains("credentials.key"));
        request.runtime_api_key = Some("override-private-token".into());
        let model = super::acquire_model(&models, Some(&loaded), &request, None)
            .expect("runtime key bypasses missing saved key");
        let stored = super::saved_embedding_config(
            &layout,
            Some(&loaded),
            &request,
            &model.info().model().reference(),
            true,
            None,
        )
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

        check_index_endpoint_change(&models, &loaded, request);
        models.close();
    }

    #[test]
    fn every_embedding_provider_persists_model_cache_without_temporary_overrides() {
        for entry in crate::models::list_embedding_models() {
            assert_model_cache_persistence(entry.embedding_info(), super::ModelConfig::default());
        }
        for auth in ["none", "bearer"] {
            let http: super::HttpEmbeddingConfig = serde_json::from_value(serde_json::json!({
                "model": "served-model", "dimension": 4, "maxInputTokens": 128, "auth": auth
            }))
            .expect("HTTP descriptor");
            let info = http.model_info("http/fixture").expect("HTTP model");
            assert_model_cache_persistence(
                info,
                super::ModelConfig {
                    http: Some(http),
                    endpoint: Some("http://127.0.0.1:9/v1/embeddings".into()),
                    ..Default::default()
                },
            );
        }
    }

    fn assert_model_cache_persistence(
        model: super::EmbeddingModelInfo,
        config: super::ModelConfig<super::EncryptedApiKey>,
    ) {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let reference = model.model().reference();
        let cache = directory.path().join("models");
        let replacement = directory.path().join("replacement-models");
        let endpoint = config.endpoint.clone();
        let descriptor = IndexDescriptor::new(
            model,
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::from([(reference.clone(), config)]),
            false,
            crate::domain::FTS_CONFIG,
        )
        .expect("index descriptor");
        let mut workspace = None;
        for (options, expected) in [
            (IndexOptions::default(), None),
            (
                IndexOptions {
                    runtime_model_cache: Some(directory.path().join("temporary-models")),
                    ..Default::default()
                },
                None,
            ),
            (
                IndexOptions {
                    model_cache: Some(cache.clone()),
                    ..Default::default()
                },
                Some(&cache),
            ),
            (IndexOptions::default(), Some(&cache)),
            (
                IndexOptions {
                    runtime_model_cache: Some(directory.path().join("temporary-models")),
                    ..Default::default()
                },
                Some(&cache),
            ),
            (
                IndexOptions {
                    model_cache: Some(replacement.clone()),
                    ..Default::default()
                },
                Some(&replacement),
            ),
            (IndexOptions::default(), Some(&replacement)),
        ] {
            let plan = super::EmbeddingPlan {
                proposed: descriptor.clone(),
                requests: std::collections::BTreeMap::from([(
                    reference.clone(),
                    IndexOptions {
                        endpoint: endpoint.clone(),
                        ..options.clone()
                    },
                )]),
                active: std::collections::BTreeSet::from([reference.clone()]),
            };
            let updated = super::index_workspace_state(
                directory.path(),
                &layout,
                workspace.as_ref(),
                &plan,
                &options,
                crate::domain::ScanRules::default(),
                true,
            )
            .expect("workspace");
            assert_eq!(
                updated.model_cache_dir(),
                expected.map(PathBuf::as_path),
                "{reference}"
            );
            let saved = &updated
                .index()
                .descriptor()
                .expect("index")
                .embedding_configs()[&reference];
            assert_eq!(saved.cache_dir, None, "cache is shared, not per model");
            super::write_workspace_manifest(layout.manifest_path(), &updated).expect("write");
            workspace = super::read_workspace_manifest(directory.path(), &layout).expect("read");
            assert_eq!(workspace.as_ref(), Some(&updated));
        }
    }

    fn check_index_endpoint_change(
        models: &ModelRuntimeManager,
        loaded: &Workspace,
        mut request: IndexOptions,
    ) {
        // Changing only the destination must not read the old destination's key,
        // even when the model reference and workspace are unchanged.
        request.runtime_api_key = None;
        request.endpoint = Some("https://new.example.test/v1".into());
        if let Err(error) = super::acquire_model(models, Some(loaded), &request, None) {
            assert!(error.message().contains("requires an API key"), "{error}");
        }
        let layout = loaded.layout();
        let new_endpoint = "https://new.example.test/v1/embeddings";
        let reference = "qwen/text-embedding-v4";
        for active in [true, false] {
            let changed = super::saved_embedding_config(
                layout,
                Some(loaded),
                &request,
                reference,
                active,
                None,
            )
            .expect("change destination");
            assert_eq!(changed.endpoint.as_deref(), Some(new_endpoint));
            assert!(changed.api_key.is_none());
        }
        request.api_key = Some("replacement-key".into());
        let changed =
            super::saved_embedding_config(layout, Some(loaded), &request, reference, false, None)
                .expect("save explicit replacement for inactive model");
        assert_eq!(changed.endpoint.as_deref(), Some(new_endpoint));
        assert_eq!(
            super::decrypt_api_key(
                changed.api_key.as_ref().expect("saved key"),
                layout,
                reference
            )
            .expect("decrypt replacement"),
            "replacement-key"
        );
    }

    fn bearer_http_workspace(root: &Path) -> Workspace {
        let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        std::fs::create_dir(layout.storage_dir()).expect("storage directory");
        let http: super::HttpEmbeddingConfig = serde_json::from_value(serde_json::json!({
            "model": "served-model", "dimension": 4, "maxInputTokens": 128,
            "auth": "bearer", "apiKeyEnv": "ZG_TEST_UNUSED_ENV"
        }))
        .expect("HTTP descriptor");
        let model_info = http.model_info("http/private").expect("model info");
        let encrypted =
            super::encrypt_api_key(&layout, "http/private", "saved-http-token").expect("encrypt");
        let saved_config = super::ModelConfig {
            api_key: Some(encrypted),
            endpoint: Some("https://example.test/v1/embeddings".into()),
            http: Some(http),
            ..Default::default()
        };
        let descriptor = IndexDescriptor::new(
            model_info,
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::from([("http/private".into(), saved_config)]),
            false,
            crate::domain::FTS_CONFIG,
        )
        .expect("bearer HTTP index may store encrypted API key");
        Workspace::new(
            root,
            layout,
            crate::domain::ScanRules::default(),
            crate::workspace::CURRENT_INDEX_VERSION,
            Some(uuid::Uuid::new_v4().to_string()),
            1,
            1,
            IndexState::Enabled(Box::new(descriptor)),
        )
        .expect("workspace")
    }

    #[test]
    fn http_bearer_key_precedence_and_workspace_persistence_follow_qwen_policy() {
        let directory = tempdir().expect("workspace");
        let workspace = bearer_http_workspace(directory.path());
        let layout = workspace.layout();
        let descriptor = workspace.index().descriptor().expect("HTTP descriptor");
        let saved_config = &descriptor.embedding_configs()["http/private"];
        let http = saved_config.http.as_ref().expect("HTTP definition");

        // Global provider keys must never be applied to http/ models.
        let global = serde_json::json!({"providers": {"http": {"apiKey": "provider-key"}}});
        let resolve = |ws, saved, explicit| {
            super::resolve_remote_api_key(
                &global,
                "http/private",
                Some(http),
                ws,
                saved,
                explicit,
                saved_config.endpoint.as_deref(),
            )
            .expect("resolve bearer key")
        };
        assert_eq!(
            resolve(Some(&workspace), Some(saved_config), Some("override")),
            Some("override".into())
        );
        assert_eq!(
            resolve(Some(&workspace), Some(saved_config), None),
            Some("saved-http-token".into())
        );
        assert_eq!(resolve(None, None, None), None);

        // runtime_api_key overrides without altering the saved encrypted key.
        let models = ModelRuntimeManager::new();
        let mut request = IndexOptions {
            root: Some(directory.path().to_path_buf()),
            runtime_api_key: Some("temporary-runtime-key".into()),
            allow_remote: true,
            ..IndexOptions::default()
        };
        let _lease =
            super::acquire_model(&models, Some(&workspace), &request, Some(http)).expect("lease");
        let save = |request: &IndexOptions| {
            super::saved_embedding_config(
                layout,
                Some(&workspace),
                request,
                "http/private",
                true,
                Some(http),
            )
            .expect("save HTTP configuration")
        };
        assert_eq!(save(&request).api_key, saved_config.api_key);
        request.api_key = Some("rotated-saved-key".into());
        let rotated = save(&request);
        assert_eq!(
            super::decrypt_api_key(
                rotated.api_key.as_ref().expect("rotated"),
                layout,
                "http/private"
            )
            .expect("decrypt"),
            "rotated-saved-key"
        );

        // auth: none rejects persisted API keys in index metadata.
        let mut none_http = http.clone();
        none_http.auth = super::HttpAuth::None;
        none_http.api_key_env = None;
        let none_config = super::ModelConfig {
            api_key: saved_config.api_key.clone(),
            endpoint: Some("https://example.test/v1/embeddings".into()),
            http: Some(none_http),
            ..Default::default()
        };
        assert!(
            IndexDescriptor::new(
                descriptor.default_embedding_model().clone(),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::from([("http/private".into(), none_config)]),
                false,
                crate::domain::FTS_CONFIG,
            )
            .is_err()
        );
        models.close();
    }

    #[test]
    fn rejects_incompatible_index_versions() {
        crate::workspace::manifest::require_current_index_version(super::CURRENT_INDEX_VERSION)
            .expect("current index");
        for version in [
            0,
            super::CURRENT_INDEX_VERSION - 1,
            super::CURRENT_INDEX_VERSION + 1,
            u32::MAX,
        ] {
            assert_ne!(
                version,
                super::CURRENT_INDEX_VERSION,
                "samples must stay incompatible with the current version"
            );
            let error = crate::workspace::manifest::require_current_index_version(version)
                .expect_err("incompatible index format");
            assert_eq!(error.code(), crate::EngineError::INDEX_INCOMPATIBLE);
            assert!(
                error
                    .help()
                    .expect("rebuild guidance")
                    .contains("zg --index --rebuild")
            );
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
            super::CURRENT_INDEX_VERSION,
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
            let finalizing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let captured_finalizing = finalizing.clone();
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
                                assert_eq!(progress.phase, IndexProgressPhase::Finalizing);
                                assert!(!layout.manifest_path().exists());
                            }
                            if progress.phase == IndexProgressPhase::Finalizing {
                                captured_finalizing
                                    .store(true, std::sync::atomic::Ordering::Release);
                            }
                            if progress.phase == IndexProgressPhase::Done {
                                assert!(
                                    captured_finalizing.load(std::sync::atomic::Ordering::Acquire)
                                );
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
            assert!(finalizing.load(std::sync::atomic::Ordering::Acquire));
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
                include_counts: false,
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
                    include_counts: false,
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
                    include_counts: false,
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
            include_counts: false,
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
    async fn model_cache_survives_switching_embedding_models() {
        for legacy in [false, true] {
            let directory = tempdir().expect("workspace");
            let cache = directory.path().join("shared-models");
            std::fs::create_dir(&cache).expect("cached models");
            let service = WorkspaceIndexService::new();
            let models = ModelRuntimeManager::new();
            let mut options = empty_index_options(directory.path());
            options.model_cache = Some(cache.clone());
            service
                .index(&models, options.clone())
                .await
                .expect("model A");
            let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
            let mut active = super::read_workspace_manifest(directory.path(), &layout)
                .expect("read")
                .expect("workspace");
            if legacy {
                write_previous_index_version(layout.storage_dir(), &active);
                active = crate::workspace::manifest::inspect_workspace_manifest(
                    directory.path(),
                    &layout,
                )
                .expect("legacy manifest")
                .into_workspace(true)
                .expect("rebuild settings")
                .expect("workspace");
            }
            options.rebuild = true;
            options.model_cache = None;
            options.embedding.as_mut().expect("model B").reference =
                "local/all-minilm-l6-v2".into();
            let expected = std::env::var_os("ZVEC_GREP_MODEL_CACHE")
                .map_or_else(|| cache.clone(), PathBuf::from);
            assert_eq!(
                super::index_model_cache(&serde_json::json!({}), Some(&active), &options),
                Some(expected.clone()),
            );
            service
                .index(&models, options)
                .await
                .expect("rebuild with model B");
            let rebuilt = super::read_workspace_manifest(directory.path(), &layout)
                .expect("read")
                .expect("workspace");
            assert_eq!(
                super::index_model_cache(
                    &serde_json::json!({}),
                    Some(&rebuilt),
                    &IndexOptions::default()
                ),
                Some(expected.clone()),
            );
            assert_eq!(rebuilt.model_cache_dir(), Some(cache.as_path()));
            let serialized = serde_json::to_value(&rebuilt).expect("manifest");
            assert_eq!(serialized["modelCacheDir"], serde_json::json!(cache));
            assert!(!serialized.to_string().contains("\"cacheDir\""));
            let acquire = crate::pipelines::indexed_search::service::acquire_search_model;
            assert_eq!(
                models.snapshot().cached_runtimes,
                0,
                "empty indexes defer embedding"
            );
            let explicit = acquire(
                &models,
                &rebuilt,
                &crate::api::context::ContextOptions {
                    model_cache: Some(expected),
                    ..Default::default()
                },
            )
            .expect("query with explicit cache");
            let query = acquire(
                &models,
                &rebuilt,
                &crate::api::context::ContextOptions::default(),
            )
            .expect("query uses shared cache");
            assert_eq!(
                models.snapshot().cached_runtimes,
                1,
                "same runtime for saved cache"
            );
            drop((explicit, query));
            models.close();
        }
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
        assert_eq!(refreshed.model_cache_dir(), manifest.model_cache_dir());
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
        assert_eq!(
            saved["modelCacheDir"],
            serde_json::json!(directory.path().join("saved-models"))
        );
        for runtime in saved["index"]["descriptor"]["embeddingConfigs"]
            .as_object()
            .expect("runtimes")
            .values()
        {
            assert_eq!(runtime["device"], "auto");
            assert!(runtime.get("cacheDir").is_none());
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
        assert_eq!(replaced.model_cache_dir(), Some(replacement.as_path()));
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
                index.graph_indexing(),
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
                    include_status: false,
                    include_counts: false,
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
    async fn routed_model_persists_until_cleared_by_a_rebuild() {
        use crate::domain::ContentKind;
        let dir = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(dir.path());
        let mut code = options.embedding.clone().expect("default model");
        code.reference = "local/bekko-256-int8".into();
        options.embedding_routes.insert(ContentKind::Code, code);
        service
            .index(&models, options)
            .await
            .expect("index with override");
        let refresh = IndexOptions {
            root: Some(dir.path().into()),
            ..Default::default()
        };
        service
            .index(&models, refresh.clone())
            .await
            .expect("inherit saved override");
        let info_options = InfoOptions {
            root: refresh.root.clone(),
            include_status: true,
            include_counts: true,
        };
        let before = service
            .info(info_options.clone())
            .await
            .expect("index info")
            .workspace_index
            .expect("published index");
        let text = before
            .content_indexes
            .iter()
            .find(|i| i.kind == ContentKind::Text)
            .expect("text index");
        let code = before
            .content_indexes
            .iter()
            .find(|i| i.kind == ContentKind::Code)
            .expect("code index");
        assert!(text.inherited && !code.inherited);
        assert_eq!(code.embedding.model, "bekko-256-int8");
        assert!(code.embedding.input_budget.is_some());
        let mut clear = IndexOptions {
            clear_embedding_routes: vec![ContentKind::Code],
            ..refresh
        };
        let error = service
            .index(&models, clear.clone())
            .await
            .expect_err("model change needs a rebuild");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            service
                .info(info_options.clone())
                .await
                .expect("index info")
                .workspace_index
                .expect("unchanged index"),
            before
        );
        clear.rebuild = true;
        service
            .index(&models, clear)
            .await
            .expect("clear with rebuild");
        let after = service
            .info(info_options)
            .await
            .expect("index info")
            .workspace_index
            .expect("rebuilt index");
        assert!(
            after
                .content_indexes
                .iter()
                .all(|i| i.inherited && i.embedding.model == text.embedding.model)
        );
    }

    #[test]
    fn fully_overridden_default_uses_no_remote_runtime_or_credentials() {
        use crate::domain::ContentKind;
        let dir = tempdir().expect("workspace");
        let workspace = bearer_http_workspace(dir.path());
        // A missing encryption key catches any attempt to read the dormant credential.
        std::fs::remove_file(workspace.layout().credentials_key_path()).expect("remove key");
        for (reference, existing) in [
            ("qwen/text-embedding-v4", None),
            ("http/private", Some(&workspace)),
        ] {
            let mut options = empty_index_options(dir.path());
            let local = options.embedding.clone().expect("local model");
            let remote = options.embedding.as_mut().expect("default model");
            remote.reference = reference.into();
            remote.device = Device::Auto;
            for kind in [ContentKind::Text, ContentKind::Code] {
                options.embedding_routes.insert(kind, local.clone());
            }
            let plan =
                super::EmbeddingPlan::new(existing, &options, reference).expect("local routes");
            assert_eq!(
                plan.proposed.default_embedding_model().model().reference(),
                reference
            );
            assert_eq!(plan.active.iter().collect::<Vec<_>>(), [&local.reference]);
            if let Some(existing) = existing {
                let updated = super::index_workspace_state(
                    dir.path(),
                    existing.layout(),
                    Some(existing),
                    &plan,
                    &options,
                    existing.scan().clone(),
                    true,
                )
                .expect("publish dormant HTTP definition");
                let saved = existing.index().descriptor().expect("original index");
                let updated = updated.index().descriptor().expect("updated index");
                assert_eq!(
                    updated.embedding_configs()[reference],
                    saved.embedding_configs()[reference]
                );
            }
        }
    }

    /// Reads the saved graph switch the way production does: from the published
    /// index descriptor, which is its only home.
    fn graph_indexing_enabled(workspace: &Workspace) -> bool {
        workspace
            .index()
            .descriptor()
            .is_some_and(IndexDescriptor::graph_indexing)
    }

    #[tokio::test]
    async fn workspace_setting_controls_graph_writes_and_cached_and_uncached_reads() {
        use crate::api::relationships::{Direction, RelationshipOptions};
        for graph_enabled in [false, true] {
            let directory = tempdir().expect("workspace");
            let engine = crate::ZvecGrep::new();
            engine
                .index(IndexOptions {
                    graph_indexing: Some(graph_enabled),
                    ..empty_index_options(directory.path())
                })
                .await
                .expect("index empty workspace");
            let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
            let workspace = super::read_workspace_manifest(directory.path(), &layout)
                .expect("manifest")
                .expect("published index");
            assert_eq!(
                workspace
                    .layout()
                    .index_dir()
                    .join("graph/graph.sqlite")
                    .exists(),
                graph_enabled
            );
            for cached in [false, true] {
                if cached {
                    engine.enable_read_session_cache().expect("enable cache");
                }
                for direction in [Direction::In, Direction::Out] {
                    let result = engine
                        .relationships(
                            direction,
                            RelationshipOptions {
                                root: Some(directory.path().to_path_buf()),
                                symbol: "missing".into(),
                                ..RelationshipOptions::default()
                            },
                        )
                        .await;
                    if graph_enabled {
                        let result = result.expect("enabled graph read");
                        assert!(result.matches.is_empty());
                        assert!(result.suggestions.is_empty());
                    } else {
                        let error = result.expect_err("disabled graph read");
                        assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
                        assert!(
                            error
                                .help()
                                .is_some_and(|help| help.contains("zg --index --rebuild --graph"))
                        );
                    }
                }
            }
            engine.close();
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
                    include_counts: false,
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
                    include_status: false,
                    include_counts: false,
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
                    include_status: true,
                    include_counts: false,
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
            include_counts: false,
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
                    include_counts: false,
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

    // One workspace has to be walked through reject, apply, reject, apply to
    // prove the published index survives each rejected switch.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn graph_indexing_switch_requires_an_explicit_rebuild() {
        let directory = tempdir().expect("workspace");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let published = |layout: &WorkspaceLayout| {
            super::read_workspace_manifest(
                layout.storage_dir().parent().expect("source root"),
                layout,
            )
            .expect("manifest read")
            .expect("published workspace")
        };

        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("initial index");
        let initial = published(&layout);
        assert!(
            !graph_indexing_enabled(&initial),
            "graph indexing stays off unless the request asks for it"
        );
        assert!(
            !layout.index_dir().join("graph").exists(),
            "a graph-free workspace must not create graph storage"
        );

        // An ordinary refresh keeps the saved setting, so it does not rebuild.
        service
            .index(&models, empty_index_options(directory.path()))
            .await
            .expect("refresh");
        let refreshed = published(&layout);
        assert_eq!(refreshed.storage_generation(), initial.storage_generation());
        assert!(!graph_indexing_enabled(&refreshed));

        // Flipping the switch is a configuration change, not a refresh: without
        // an explicit rebuild it is rejected and the published index is kept.
        let error = service
            .index(
                &models,
                IndexOptions {
                    graph_indexing: Some(true),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("enabling graph indexing needs an explicit rebuild");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            error.message(),
            "code graph indexing would change from disabled to enabled"
        );
        assert_eq!(
            error.help(),
            Some("Rerun the same indexing command with `--rebuild`.")
        );
        let rejected = published(&layout);
        assert!(
            !graph_indexing_enabled(&rejected),
            "the saved switch stays off"
        );
        assert_eq!(
            rejected.storage_generation(),
            initial.storage_generation(),
            "a rejected switch must not replace the published index"
        );
        assert!(!layout.index_dir().join("graph").exists());

        // The explicit rebuild applies the switch; a graph only describes the
        // files indexed alongside it, so the generation changes.
        service
            .index(
                &models,
                IndexOptions {
                    graph_indexing: Some(true),
                    rebuild: true,
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("enable graph indexing");
        let enabled = published(&layout);
        assert!(graph_indexing_enabled(&enabled));
        assert_ne!(enabled.storage_generation(), initial.storage_generation());
        assert!(
            layout.index_dir().join("graph").exists(),
            "an enabled workspace keeps graph storage"
        );

        // The reverse direction is rejected the same way and keeps the graph.
        let error = service
            .index(
                &models,
                IndexOptions {
                    graph_indexing: Some(false),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("disabling graph indexing needs an explicit rebuild");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(
            error.message(),
            "code graph indexing would change from enabled to disabled"
        );
        assert_eq!(
            error.help(),
            Some("Rerun the same indexing command with `--rebuild`.")
        );
        let still_enabled = published(&layout);
        assert!(
            graph_indexing_enabled(&still_enabled),
            "the saved switch stays on"
        );
        assert_eq!(
            still_enabled.storage_generation(),
            enabled.storage_generation(),
            "a rejected switch must not replace the published index"
        );
        assert!(layout.index_dir().join("graph").exists());

        // Disabling with an explicit rebuild drops the stale graph with the
        // previous generation.
        service
            .index(
                &models,
                IndexOptions {
                    graph_indexing: Some(false),
                    rebuild: true,
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("disable graph indexing");
        let disabled = published(&layout);
        assert!(!graph_indexing_enabled(&disabled));
        assert_ne!(disabled.storage_generation(), enabled.storage_generation());
        assert!(
            !layout.index_dir().join("graph").exists(),
            "disabling graph indexing must not leave the old graph behind"
        );
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
                    include_counts: false,
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
    async fn zero_index_concurrency_is_rejected_for_every_backend() {
        let service = WorkspaceIndexService::new();
        for reference in [
            "local/all-minilm-l6-v2",
            "local/embeddinggemma-300m",
            "local/potion-code-16m-v2",
            "qwen/text-embedding-v4",
        ] {
            let directory = tempdir().expect("workspace");
            let models = ModelRuntimeManager::new();
            let mut options = empty_index_options(directory.path());
            let model = options.embedding.as_mut().expect("model");
            model.reference = reference.into();
            model.cache_dir = Some(directory.path().join("model-cache"));
            options.embedding_concurrency = Some(0);
            let result = service.index(&models, options).await;
            assert_eq!(
                result.expect_err("zero concurrency").code(),
                crate::EngineError::INVALID_ARGUMENT,
                "{reference}"
            );
            assert_eq!(models.snapshot().cached_runtimes, 0);
            assert!(!directory.path().join("model-cache").exists());
            models.close();
        }
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
    async fn input_budget_diagnostics_use_saved_limits_without_loading_models_or_changing_metadata()
    {
        let directory = tempdir().expect("workspace");
        let root = directory.path();
        let cache = root.join("model-cache");
        let service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(root);
        options.embedding.as_mut().expect("model").cache_dir = Some(cache.clone());
        service.index(&models, options).await.expect("empty index");
        models.close();
        assert!(!cache.exists(), "empty indexing must not prepare a model");
        let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        let original = std::fs::read(layout.manifest_path()).expect("saved manifest");

        // A saved limit that differs from the catalog proves that diagnostics
        // describe this index rather than constructing a new model runtime.
        for saved_limit in [Some(17), None] {
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&original).expect("manifest JSON");
            manifest["index"]["descriptor"]["defaultEmbeddingModel"]["maxInputTokens"] =
                serde_json::json!(saved_limit);
            let bytes = serde_json::to_vec(&manifest).expect("updated metadata");
            std::fs::write(layout.manifest_path(), &bytes).expect("save test limit");
            for (include_status, include_counts) in
                [(true, false), (false, true), (true, true), (true, false)]
            {
                let info = service
                    .info(InfoOptions {
                        root: Some(root.to_path_buf()),
                        include_status,
                        include_counts,
                    })
                    .await
                    .expect("read-only inspection");
                let budget = info
                    .workspace_index
                    .expect("workspace")
                    .embedding
                    .expect("embedding")
                    .input_budget;
                assert_eq!(
                    budget,
                    if include_status && include_counts {
                        super::embedding_input_budget(saved_limit)
                    } else {
                        None
                    }
                );
                if let Some(budget) = budget {
                    assert_eq!(budget.max_input_tokens, 17);
                    assert_eq!(budget.normal.max_length, 31);
                    assert_eq!(budget.normal.overlap, 4);
                }
                assert_eq!(
                    std::fs::read(layout.manifest_path()).expect("manifest"),
                    bytes
                );
                assert!(!cache.exists(), "inspection must not prepare a model");
            }
        }
        std::fs::write(layout.manifest_path(), &original).expect("restore model metadata");
        std::fs::remove_dir_all(layout.index_dir()).expect("remove test index");
        let missing = service
            .info(InfoOptions {
                root: Some(root.to_path_buf()),
                include_status: true,
                include_counts: true,
            })
            .await
            .expect("missing index inspection");
        assert!(!missing.indexed);
        assert_eq!(
            missing
                .workspace_index
                .expect("workspace")
                .embedding
                .expect("model")
                .input_budget,
            None
        );
    }

    #[tokio::test]
    async fn only_explicit_info_counts_fragments_once_and_isolates_count_failures() {
        use std::sync::atomic::Ordering;

        let directory = tempdir().expect("workspace");
        let root = directory.path();
        let mut service = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        service
            .index(&models, empty_index_options(root))
            .await
            .expect("empty index");
        assert_eq!(service.fragment_count_reads.load(Ordering::Acquire), 0);
        let options = InfoOptions {
            root: Some(root.to_path_buf()),
            include_status: true,
            include_counts: false,
        };
        for expected in 1..=2 {
            let plain = service
                .info(options.clone())
                .await
                .expect("ordinary status");
            assert_eq!(plain.status.expect("status").fragments_indexed, None);
            assert_eq!(
                service.fragment_count_reads.load(Ordering::Acquire),
                expected - 1
            );
            let diagnostic = service
                .info(InfoOptions {
                    include_counts: true,
                    ..options.clone()
                })
                .await
                .expect("explicit counts");
            assert_eq!(
                diagnostic.status.expect("status").fragments_indexed,
                Some(0)
            );
            assert_eq!(
                service.fragment_count_reads.load(Ordering::Acquire),
                expected
            );
        }

        service.fail_fragment_count = true;
        service
            .info(options.clone())
            .await
            .expect("ordinary status skips failed count");
        let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        let wait = super::LockWait::new(None, None).expect("lock wait");
        assert!(
            !service
                .workspace_needs_refresh(root, &layout, &wait, None)
                .await
                .expect("freshness skips failed count")
        );
        assert_eq!(service.fragment_count_reads.load(Ordering::Acquire), 2);
        let error = service
            .info(InfoOptions {
                include_counts: true,
                ..options.clone()
            })
            .await
            .expect_err("explicit count failure");
        assert!(error.message().contains("injected fragment count failure"));
        assert_eq!(service.fragment_count_reads.load(Ordering::Acquire), 3);
        let plain = service
            .info(options)
            .await
            .expect("ordinary status after failure");
        assert_eq!(plain.status.expect("status").fragments_indexed, None);
        assert_eq!(service.fragment_count_reads.load(Ordering::Acquire), 3);
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
        options.graph_indexing = Some(true);
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
            include_counts: false,
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
            serialized["modelCacheDir"],
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
        assert_eq!(error.code(), crate::EngineError::INDEX_INCOMPATIBLE);
        assert!(
            error
                .help()
                .expect("rebuild guidance")
                .contains("zg --index --rebuild")
        );

        service
            .index(
                &models,
                IndexOptions {
                    root: options.root.clone(),
                    rebuild: true,
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("v2 rebuild inherits saved configuration without overrides");
        let rebuilt = super::read_workspace_manifest(
            layout.storage_dir().parent().expect("source root"),
            &layout,
        )
        .expect("manifest read")
        .expect("manifest");
        assert_eq!(rebuilt.index_version(), super::CURRENT_INDEX_VERSION);
        assert_eq!(rebuilt.source_root(), manifest.source_root());
        assert_eq!(rebuilt.scan(), manifest.scan());
        assert!(graph_indexing_enabled(&rebuilt));
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
