//! Resident workspace runtimes and native watcher orchestration.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zg_engine::{
    EngineError, ErrorSite, ZvecGrep,
    api::{
        context::{ContextOptions, ContextResult, options::RefreshPolicy},
        index::{IndexOptions, IndexResult, options::WorkspaceChange as IndexChange},
        info::InfoOptions,
    },
};
use zg_host_native::{
    DiscoveryOptions as HostDiscoveryOptions, HostError, HostErrorSite, NativeWatcherFactory,
    RootSpec, TaskControl, WatchRequest, WorkspaceChange, WorkspaceWatchSessionPort,
    WorkspaceWatcherFactoryPort,
};

use crate::job_scheduler::{
    IndexExecutor, IndexJobCompletion, IndexJobScheduler, IndexJobSnapshot, JobReason, JobState,
    SchedulerConfig, SchedulerError, SchedulerSnapshot,
};
use zg_transport_mcp::{
    IndexOperationError, IndexOperationProvider, IndexOperationResult, IndexOperationState,
    IndexRuntimeSnapshot,
};

#[derive(Clone)]
pub(crate) struct WorkspaceRuntimeManager {
    inner: Arc<RuntimeManagerInner>,
}

struct RuntimeManagerInner {
    executor: Arc<dyn IndexExecutor>,
    scheduler: IndexJobScheduler,
    watcher_factory: Arc<dyn WorkspaceWatcherFactoryPort>,
    runtimes: Mutex<HashMap<PathBuf, Arc<WorkspaceRuntime>>>,
    shutdown: CancellationToken,
    closed: AtomicBool,
}

struct WorkspaceRuntime {
    canonical_root: PathBuf,
    index_template: Mutex<IndexOptions>,
    watcher: tokio::sync::Mutex<Option<WatcherHandle>>,
    watcher_active: AtomicBool,
    dirty_revision: AtomicU64,
    indexed_revision: AtomicU64,
}

struct WatcherHandle {
    barrier: mpsc::Sender<oneshot::Sender<()>>,
    cancellation: CancellationToken,
    session: Arc<dyn WorkspaceWatchSessionPort>,
    task: JoinHandle<()>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeIndexSubmission {
    pub job: IndexJobSnapshot,
    pub reused: bool,
    pub result: Option<IndexResult>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeManagerSnapshot {
    pub active_runtimes: usize,
    pub jobs: SchedulerSnapshot,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkspaceRuntimeSnapshot {
    pub watcher_active: bool,
    pub dirty_revision: u64,
    pub indexed_revision: u64,
}

#[derive(Debug, Error)]
pub(crate) enum WorkspaceRuntimeError {
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error("workspace watcher failed: {0}")]
    Watcher(#[from] HostError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl WorkspaceRuntimeError {
    pub(crate) fn into_engine_error(self) -> EngineError {
        match self {
            Self::Scheduler(SchedulerError::Closed) => {
                EngineError::resource_closed("daemon index scheduler has been closed")
            }
            Self::Scheduler(SchedulerError::QueueFull) => {
                EngineError::resource_busy("daemon index queue is full")
            }
            Self::Scheduler(SchedulerError::UnknownJob(id)) => {
                EngineError::internal(format!("daemon index job {id} disappeared"))
            }
            Self::Watcher(error) => map_host_error(error),
            Self::Engine(error) => error,
        }
    }
}

struct ZvecGrepIndexExecutor {
    engine: Arc<ZvecGrep>,
}

#[async_trait::async_trait]
impl IndexExecutor for ZvecGrepIndexExecutor {
    async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
        self.engine.index(options).await
    }

    async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        self.engine.drop_index(options).await
    }
}

impl WorkspaceRuntimeManager {
    pub(crate) fn native(engine: Arc<ZvecGrep>) -> Self {
        Self::new(
            Arc::new(ZvecGrepIndexExecutor { engine }),
            Arc::new(NativeWatcherFactory::default()),
            SchedulerConfig::default(),
        )
    }

    pub(crate) fn new(
        executor: Arc<dyn IndexExecutor>,
        watcher_factory: Arc<dyn WorkspaceWatcherFactoryPort>,
        scheduler_config: SchedulerConfig,
    ) -> Self {
        Self {
            inner: Arc::new(RuntimeManagerInner {
                scheduler: IndexJobScheduler::new(Arc::clone(&executor), scheduler_config),
                executor,
                watcher_factory,
                runtimes: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) async fn submit_index(
        &self,
        mut options: IndexOptions,
        wait: bool,
    ) -> Result<RuntimeIndexSubmission, WorkspaceRuntimeError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(WorkspaceRuntimeError::Scheduler(SchedulerError::Closed));
        }
        let canonical_root = canonical_root(options.root.as_deref())?;
        options.root = Some(canonical_root.clone());
        let runtime = self.runtime(canonical_root.clone(), &options);
        *lock(&runtime.index_template) = options.clone();
        let target_revision = runtime.dirty_revision.load(Ordering::Acquire);
        let submitted = self
            .inner
            .scheduler
            .submit(canonical_root, options, JobReason::Manual)?;
        if !wait {
            let manager = self.clone();
            let job_id = submitted.job.id;
            tokio::spawn(async move {
                let Ok(completed) = manager.inner.scheduler.wait(job_id).await else {
                    return;
                };
                if completed.job.state == JobState::Succeeded {
                    let _ = manager
                        .on_index_succeeded(completed.job, target_revision)
                        .await;
                }
            });
            return Ok(RuntimeIndexSubmission {
                job: submitted.job,
                reused: submitted.reused,
                result: None,
            });
        }

        let completed = self.inner.scheduler.wait(submitted.job.id).await?;
        if completed.job.state == JobState::Succeeded
            && let Err(error) = self
                .on_index_succeeded(completed.job.clone(), target_revision)
                .await
        {
            warn!(%error, root = %completed.job.canonical_root.display(), "index succeeded but watcher activation failed");
        }
        Ok(submission(completed, submitted.reused))
    }

    async fn refresh_index(&self, options: IndexOptions, wait: bool) -> Result<(), EngineError> {
        let root = canonical_root(options.root.as_deref())
            .map_err(WorkspaceRuntimeError::into_engine_error)?;
        let runtime = self.runtime(root.clone(), &options);
        // Watch submissions always queue a successor to an already running job.
        // A full reconciliation also covers changes still in watcher debounce.
        let mut options = options;
        options.changes = vec![IndexChange::Rescan];
        if wait {
            self.ensure_watching(Arc::clone(&runtime))
                .await
                .map_err(WorkspaceRuntimeError::into_engine_error)?;
            self.flush_watcher(&runtime).await?;
        }
        let revision = runtime.dirty_revision.load(Ordering::Acquire);
        let submitted = self
            .inner
            .scheduler
            .submit(root.clone(), options, JobReason::Watch)
            .map_err(|error| WorkspaceRuntimeError::from(error).into_engine_error())?;
        if !wait {
            let manager = self.clone();
            tokio::spawn(async move {
                if let Ok(completed) = manager.inner.scheduler.wait(submitted.job.id).await
                    && completed.job.state == JobState::Succeeded
                {
                    runtime
                        .indexed_revision
                        .fetch_max(revision, Ordering::AcqRel);
                    if let Err(error) = manager.ensure_watching(runtime).await {
                        warn!(%error, "search refresh watcher activation failed");
                    }
                }
            });
            return Ok(());
        }
        let completed = self
            .inner
            .scheduler
            .wait(submitted.job.id)
            .await
            .map_err(|error| WorkspaceRuntimeError::from(error).into_engine_error())?;
        ensure_refresh_succeeded(&completed.job)?;
        runtime
            .indexed_revision
            .fetch_max(revision, Ordering::AcqRel);
        self.ensure_watching(runtime)
            .await
            .map_err(WorkspaceRuntimeError::into_engine_error)?;
        // Include successor jobs submitted by the watcher while reconciliation ran.
        self.inner.scheduler.wait_for_root_idle(&root).await;
        if let Some(job) = self.job_for_root(&root) {
            ensure_refresh_succeeded(&job)?;
        }
        Ok(())
    }

    async fn flush_watcher(&self, runtime: &WorkspaceRuntime) -> Result<(), EngineError> {
        let watcher = runtime.watcher.lock().await;
        let Some(handle) = watcher.as_ref() else {
            return Err(EngineError::resource_closed(
                "workspace watcher is not active",
            ));
        };
        handle.session.flush().await.map_err(map_host_error)?;
        let (sender, receiver) = oneshot::channel();
        handle
            .barrier
            .send(sender)
            .await
            .map_err(|_| EngineError::resource_closed("workspace watcher has stopped"))?;
        receiver
            .await
            .map_err(|_| EngineError::resource_closed("workspace watcher barrier was interrupted"))
    }

    pub(crate) fn snapshot(&self) -> RuntimeManagerSnapshot {
        RuntimeManagerSnapshot {
            active_runtimes: lock(&self.inner.runtimes).len(),
            jobs: self.inner.scheduler.snapshot(),
        }
    }

    pub(crate) fn runtime_snapshot(&self, canonical_root: &Path) -> WorkspaceRuntimeSnapshot {
        lock(&self.inner.runtimes).get(canonical_root).map_or_else(
            WorkspaceRuntimeSnapshot::default,
            |runtime| WorkspaceRuntimeSnapshot {
                watcher_active: runtime.watcher_active.load(Ordering::Acquire),
                dirty_revision: runtime.dirty_revision.load(Ordering::Acquire),
                indexed_revision: runtime.indexed_revision.load(Ordering::Acquire),
            },
        )
    }

    pub(crate) fn job_for_root(&self, canonical_root: &Path) -> Option<IndexJobSnapshot> {
        self.inner
            .scheduler
            .get_by_root(&canonical_root.to_path_buf())
    }

    pub(crate) async fn drop_index(
        &self,
        mut options: InfoOptions,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let canonical_root = canonical_root(options.root.as_deref())?;
        options.root = Some(canonical_root.clone());
        self.stop_watching(&canonical_root).await?;
        self.inner.scheduler.cancel_root(&canonical_root);
        self.inner
            .scheduler
            .wait_for_root_idle(&canonical_root)
            .await;
        let removed = self.inner.executor.drop_index(options).await?;
        lock(&self.inner.runtimes).remove(&canonical_root);
        Ok(removed)
    }

    pub(crate) async fn shutdown_all(&self) -> Result<(), WorkspaceRuntimeError> {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.shutdown.cancel();
        let roots = lock(&self.inner.runtimes)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for root in roots {
            if let Err(error) = self.stop_watching(&root).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.inner.scheduler.shutdown().await;
        lock(&self.inner.runtimes).clear();
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn runtime(&self, canonical_root: PathBuf, options: &IndexOptions) -> Arc<WorkspaceRuntime> {
        let mut runtimes = lock(&self.inner.runtimes);
        Arc::clone(runtimes.entry(canonical_root.clone()).or_insert_with(|| {
            Arc::new(WorkspaceRuntime {
                canonical_root,
                index_template: Mutex::new(options.clone()),
                watcher: tokio::sync::Mutex::new(None),
                watcher_active: AtomicBool::new(false),
                dirty_revision: AtomicU64::new(1),
                indexed_revision: AtomicU64::new(0),
            })
        }))
    }

    async fn on_index_succeeded(
        &self,
        job: IndexJobSnapshot,
        revision: u64,
    ) -> Result<(), WorkspaceRuntimeError> {
        let runtime = lock(&self.inner.runtimes).get(&job.canonical_root).cloned();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        runtime
            .indexed_revision
            .fetch_max(revision, Ordering::AcqRel);
        self.ensure_watching(runtime).await
    }

    async fn ensure_watching(
        &self,
        runtime: Arc<WorkspaceRuntime>,
    ) -> Result<(), WorkspaceRuntimeError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut watcher = runtime.watcher.lock().await;
        if watcher.is_some() {
            return Ok(());
        }
        let template = lock(&runtime.index_template).clone();
        let cancellation = self.inner.shutdown.child_token();
        let session = self
            .inner
            .watcher_factory
            .watch(
                &WatchRequest {
                    root: RootSpec {
                        path: runtime.canonical_root.clone(),
                        recursive: true,
                        discovery: host_discovery(&template),
                    },
                },
                &TaskControl::new(cancellation.clone()),
            )
            .await?;
        runtime.watcher_active.store(true, Ordering::Release);
        let weak_inner = Arc::downgrade(&self.inner);
        let weak_runtime = Arc::downgrade(&runtime);
        let task_session = Arc::clone(&session);
        let task_cancellation = cancellation.clone();
        let (barrier, barriers) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            watch_loop(
                weak_inner,
                weak_runtime,
                task_session,
                task_cancellation,
                barriers,
            )
            .await;
        });
        *watcher = Some(WatcherHandle {
            barrier,
            cancellation,
            session,
            task,
        });
        Ok(())
    }

    async fn stop_watching(&self, canonical_root: &Path) -> Result<(), WorkspaceRuntimeError> {
        let runtime = lock(&self.inner.runtimes).get(canonical_root).cloned();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        let handle = runtime.watcher.lock().await.take();
        let Some(handle) = handle else {
            return Ok(());
        };
        handle.cancellation.cancel();
        let close_result = handle.session.close().await;
        let join_result = handle.task.await;
        runtime.watcher_active.store(false, Ordering::Release);
        close_result?;
        join_result.map_err(|error| {
            WorkspaceRuntimeError::Watcher(HostError::internal(format!(
                "workspace watcher orchestration failed: {error}"
            )))
        })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl IndexOperationProvider for WorkspaceRuntimeManager {
    async fn submit_index(
        &self,
        options: IndexOptions,
        wait: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        let submitted = WorkspaceRuntimeManager::submit_index(self, options, wait)
            .await
            .map_err(WorkspaceRuntimeError::into_engine_error)?;
        Ok(IndexOperationResult {
            root: submitted.job.canonical_root,
            job_id: submitted.job.id.to_string(),
            state: operation_state(submitted.job.state),
            reused: submitted.reused,
            error: submitted.job.error.map(|error| IndexOperationError {
                report: error.report,
                retryable: error.retryable,
            }),
        })
    }

    async fn search(
        &self,
        engine: &ZvecGrep,
        mut request: ContextOptions,
    ) -> Result<ContextResult, EngineError> {
        if request.rg {
            return engine.context(request).await;
        }
        let policy = request.refresh.unwrap_or(if request.auto_update {
            RefreshPolicy::Background
        } else {
            RefreshPolicy::Off
        });
        request.auto_update = false;
        request.refresh = Some(RefreshPolicy::Off);
        if policy == RefreshPolicy::Off {
            let mut reply = engine.context(request).await?;
            reply.freshness = Some("served_from_current_index".to_owned());
            reply.background_refresh = Some("off".to_owned());
            return Ok(reply);
        }
        let options = IndexOptions {
            root: request.root.clone(),
            allow_remote: request.allow_remote,
            api_key: request.api_key.clone(),
            endpoint: request.endpoint.clone(),
            embedding_concurrency: request.embedding_concurrency,
            device: request.device,
            model_cache: request.model_cache.clone(),
            embedding: request.authorization_model.as_ref().map(|reference| {
                zg_engine::api::index::options::EmbeddingModelSpec {
                    reference: reference.clone(),
                    revision: None,
                    cache_dir: request.model_cache.clone(),
                    endpoint: request.endpoint.clone(),
                    device: request
                        .device
                        .unwrap_or(zg_engine::api::index::options::Device::Auto),
                }
            }),
            ..IndexOptions::default()
        };
        if policy == RefreshPolicy::Background {
            // Query first so our own refresh cannot lock out the current-index read.
            // A successful query also proves that this operation is not creating an index.
            let mut reply = engine.context(request).await?;
            let mut options = options;
            options.root = Some(reply.root.clone());
            reply.freshness = Some("served_from_current_index".to_owned());
            reply.background_refresh = Some(match self.refresh_index(options, false).await {
                Ok(()) => "scheduled".to_owned(),
                Err(error) => {
                    warn!(%error, "search background refresh could not be scheduled");
                    "failed".to_owned()
                }
            });
            return Ok(reply);
        }
        let requested_root = canonical_root(request.root.as_deref())
            .map_err(WorkspaceRuntimeError::into_engine_error)?;
        let root = lock(&self.inner.runtimes)
            .keys()
            .filter(|root| requested_root.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
            .unwrap_or(requested_root);
        self.inner.scheduler.wait_for_root_idle(&root).await;
        let info = engine
            .info(InfoOptions {
                root: request.root.clone(),
                include_status: false,
            })
            .await?;
        if !info.indexed {
            // Preserve the engine's missing-index error without silently creating one.
            return engine.context(request).await;
        }
        let mut options = options;
        options.root = Some(info.root);
        self.refresh_index(options, true).await?;
        let mut reply = engine.context(request).await?;
        reply.freshness = Some("fresh".to_owned());
        reply.background_refresh = Some("idle".to_owned());
        Ok(reply)
    }

    async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        WorkspaceRuntimeManager::drop_index(self, options)
            .await
            .map_err(WorkspaceRuntimeError::into_engine_error)
    }

    fn runtime_snapshot(&self, root: &Path) -> Option<IndexRuntimeSnapshot> {
        let canonical_root = std::fs::canonicalize(root).ok()?;
        let _runtime = lock(&self.inner.runtimes).get(&canonical_root).cloned()?;
        let snapshot = WorkspaceRuntimeManager::runtime_snapshot(self, &canonical_root);
        let job = self.job_for_root(&canonical_root);
        Some(IndexRuntimeSnapshot {
            watcher_active: snapshot.watcher_active,
            dirty_revision: snapshot.dirty_revision,
            indexed_revision: snapshot.indexed_revision,
            active_job_id: job.as_ref().map(|job| job.id.to_string()),
            job_state: job.as_ref().map(|job| operation_state(job.state)),
            progress: job.as_ref().and_then(|job| job.progress.clone()),
            error: job.and_then(|job| {
                job.error.map(|error| IndexOperationError {
                    report: error.report,
                    retryable: error.retryable,
                })
            }),
        })
    }
}

fn ensure_refresh_succeeded(job: &IndexJobSnapshot) -> Result<(), EngineError> {
    if job.state == JobState::Succeeded {
        Ok(())
    } else {
        Err(job.error.as_ref().map_or_else(
            || EngineError::cancelled("index refresh did not complete successfully"),
            |error| EngineError::from_report(error.report.clone()),
        ))
    }
}

impl Drop for RuntimeManagerInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn watch_loop(
    weak_inner: Weak<RuntimeManagerInner>,
    weak_runtime: Weak<WorkspaceRuntime>,
    session: Arc<dyn WorkspaceWatchSessionPort>,
    cancellation: CancellationToken,
    mut barriers: mpsc::Receiver<oneshot::Sender<()>>,
) {
    let control = TaskControl::new(cancellation);
    loop {
        let changes = tokio::select! {
            biased;
            batch = session.next_changes(&control) => batch,
            Some(acknowledge) = barriers.recv() => {
                let _ = acknowledge.send(());
                continue;
            }
        };
        let batch = match changes {
            Ok(batch) => batch,
            Err(HostError::Cancelled { .. } | HostError::ResourceClosed { .. }) => break,
            Err(error) => {
                warn!(%error, "workspace watcher session stopped");
                break;
            }
        };
        if batch.changes.is_empty() {
            continue;
        }
        let (Some(inner), Some(runtime)) = (weak_inner.upgrade(), weak_runtime.upgrade()) else {
            break;
        };
        if inner.closed.load(Ordering::Acquire) {
            break;
        }
        let target_revision = runtime.dirty_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let mut options = lock(&runtime.index_template).clone();
        // One-operation consent must never authorize later watcher jobs.
        options.allow_remote = false;
        options.root = Some(runtime.canonical_root.clone());
        options.rebuild = false;
        options.changes = batch.changes.into_iter().map(map_change).collect();
        let Ok(submitted) =
            inner
                .scheduler
                .submit(runtime.canonical_root.clone(), options, JobReason::Watch)
        else {
            continue;
        };
        let scheduler = inner.scheduler.clone();
        let weak_runtime = Arc::downgrade(&runtime);
        tokio::spawn(async move {
            let Ok(completed) = scheduler.wait(submitted.job.id).await else {
                return;
            };
            if completed.job.state == JobState::Succeeded
                && let Some(runtime) = weak_runtime.upgrade()
            {
                runtime
                    .indexed_revision
                    .fetch_max(target_revision, Ordering::AcqRel);
            }
        });
    }
    if let Some(runtime) = weak_runtime.upgrade() {
        runtime.watcher_active.store(false, Ordering::Release);
    }
}

fn submission(completed: IndexJobCompletion, reused: bool) -> RuntimeIndexSubmission {
    RuntimeIndexSubmission {
        job: completed.job,
        reused,
        result: completed.result,
    }
}

fn canonical_root(root: Option<&Path>) -> Result<PathBuf, WorkspaceRuntimeError> {
    let requested = root.unwrap_or_else(|| Path::new("."));
    std::fs::canonicalize(requested).map_err(|error| {
        let message = format!(
            "failed to resolve workspace root {}: {error}",
            requested.display()
        );
        let error = match error.kind() {
            std::io::ErrorKind::NotFound => EngineError::not_found(message),
            std::io::ErrorKind::PermissionDenied => EngineError::permission_denied(message),
            std::io::ErrorKind::WouldBlock => EngineError::resource_busy(message),
            std::io::ErrorKind::TimedOut => EngineError::deadline_exceeded(message),
            _ => EngineError::storage_failure(message),
        };
        WorkspaceRuntimeError::Engine(error)
    })
}

fn host_discovery(options: &IndexOptions) -> HostDiscoveryOptions {
    HostDiscoveryOptions {
        include_paths: options.discovery.include_paths.clone(),
        exclude_paths: options.discovery.exclude_paths.clone(),
        globs: options.discovery.globs.clone(),
        insensitive_globs: options.discovery.insensitive_globs.clone(),
        file_types: options.discovery.file_types.clone(),
        excluded_file_types: options.discovery.excluded_file_types.clone(),
        hidden: options.discovery.hidden,
        no_ignore: options.discovery.no_ignore,
        ignore_files: options.discovery.ignore_files.clone(),
        max_depth: options.discovery.max_depth,
        max_file_size_bytes: options.discovery.max_file_size_bytes,
        follow: options.discovery.follow,
    }
}

fn map_change(change: WorkspaceChange) -> IndexChange {
    match change {
        WorkspaceChange::Upsert(path) => IndexChange::Upsert(path),
        WorkspaceChange::Delete(path) => IndexChange::Delete(path),
        WorkspaceChange::RescanDirectory(path) => IndexChange::RescanDirectory(path),
        WorkspaceChange::DeletePrefix(path) => IndexChange::DeletePrefix(path),
        WorkspaceChange::Rescan => IndexChange::Rescan,
    }
}

fn map_host_error(error: HostError) -> EngineError {
    match error {
        HostError::InvalidArgument { message, origin } => {
            host_engine_error(EngineError::invalid_argument(message), origin)
        }
        HostError::StorageFailure {
            component,
            message,
            origin,
        } => host_engine_error(
            EngineError::storage_failure(format!("{component} failed: {message}")),
            origin,
        ),
        HostError::Cancelled { message, origin } => {
            host_engine_error(EngineError::cancelled(message), origin)
        }
        HostError::DeadlineExceeded { message, origin } => {
            host_engine_error(EngineError::deadline_exceeded(message), origin)
        }
        HostError::ResourceClosed { message, origin } => {
            host_engine_error(EngineError::resource_closed(message), origin)
        }
        HostError::Internal { message, origin } => {
            host_engine_error(EngineError::internal(message), origin)
        }
    }
}

fn host_engine_error(error: EngineError, origin: HostErrorSite) -> EngineError {
    error.with_origin(ErrorSite::new(
        origin.file(),
        origin.line(),
        origin.column(),
    ))
}

const fn operation_state(state: JobState) -> IndexOperationState {
    match state {
        JobState::Queued => IndexOperationState::Queued,
        JobState::Running => IndexOperationState::Running,
        JobState::Succeeded => IndexOperationState::Succeeded,
        JobState::Failed => IndexOperationState::Failed,
        JobState::Cancelled => IndexOperationState::Cancelled,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use zg_engine::{
        EngineError,
        api::index::{IndexOptions, IndexResult, options::WorkspaceChange as IndexChange},
        api::info::InfoOptions,
    };
    use zg_host_native::{
        HostError, TaskControl, WatchRequest, WorkspaceChange, WorkspaceChangeBatch,
        WorkspaceWatchSessionPort, WorkspaceWatcherFactoryPort,
    };

    use crate::job_scheduler::{IndexExecutor, SchedulerConfig};

    use super::WorkspaceRuntimeManager;

    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<IndexOptions>>,
        drops: AtomicUsize,
    }

    struct FailingExecutor;

    #[async_trait]
    impl IndexExecutor for FailingExecutor {
        async fn index(&self, _options: IndexOptions) -> Result<IndexResult, EngineError> {
            Err(EngineError::internal("fixture index failed"))
        }
    }

    #[async_trait]
    impl IndexExecutor for RecordingExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            self.calls
                .lock()
                .expect("calls should be writable")
                .push(options);
            Ok(IndexResult::default())
        }

        async fn drop_index(&self, _options: InfoOptions) -> Result<bool, EngineError> {
            self.drops.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }
    }

    struct ManualWatcherFactory {
        receiver: Mutex<Option<mpsc::Receiver<WorkspaceChangeBatch>>>,
        watches: Mutex<Vec<WatchRequest>>,
        closes: Arc<AtomicUsize>,
    }

    struct ManualWatchSession {
        receiver: tokio::sync::Mutex<mpsc::Receiver<WorkspaceChangeBatch>>,
        closes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WorkspaceWatcherFactoryPort for ManualWatcherFactory {
        async fn watch(
            &self,
            request: &WatchRequest,
            _control: &TaskControl,
        ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
            self.watches
                .lock()
                .expect("watches should be writable")
                .push(request.clone());
            let receiver = self
                .receiver
                .lock()
                .expect("receiver should be writable")
                .take()
                .expect("only one watcher should be created");
            Ok(Arc::new(ManualWatchSession {
                receiver: tokio::sync::Mutex::new(receiver),
                closes: Arc::clone(&self.closes),
            }))
        }
    }

    #[async_trait]
    impl WorkspaceWatchSessionPort for ManualWatchSession {
        async fn next_changes(
            &self,
            control: &TaskControl,
        ) -> Result<WorkspaceChangeBatch, HostError> {
            let mut receiver = self.receiver.lock().await;
            tokio::select! {
                () = control.cancellation.cancelled() => Err(HostError::cancelled(
                    "workspace watcher operation was cancelled",
                )),
                batch = receiver.recv() => batch.ok_or_else(|| HostError::resource_closed(
                    "workspace watcher change stream has been closed",
                )),
            }
        }

        async fn close(&self) -> Result<(), HostError> {
            self.closes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct GatedExecutor {
        started: mpsc::UnboundedSender<IndexOptions>,
        release: tokio::sync::Semaphore,
    }

    #[async_trait]
    impl IndexExecutor for GatedExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            self.started.send(options).expect("job should be observed");
            self.release
                .acquire()
                .await
                .expect("gate should stay open")
                .forget();
            Ok(IndexResult::default())
        }
    }

    #[tokio::test]
    async fn wait_refresh_propagates_job_failure() {
        let workspace = tempdir().expect("workspace");
        let (_sender, receiver) = mpsc::channel(4);
        let manager = WorkspaceRuntimeManager::new(
            Arc::new(FailingExecutor),
            Arc::new(ManualWatcherFactory {
                receiver: Mutex::new(Some(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            }),
            SchedulerConfig::default(),
        );
        let error = manager
            .refresh_index(
                IndexOptions {
                    root: Some(workspace.path().to_path_buf()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect_err("failed refresh cannot report freshness");
        assert_eq!(error.code(), EngineError::INTERNAL);
        assert_eq!(error.message(), "fixture index failed");
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn background_refresh_returns_before_indexing_finishes() {
        let workspace = tempdir().expect("workspace");
        let (started, mut jobs) = mpsc::unbounded_channel();
        let executor = Arc::new(GatedExecutor {
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (_sender, receiver) = mpsc::channel(4);
        let manager = WorkspaceRuntimeManager::new(
            executor.clone(),
            Arc::new(ManualWatcherFactory {
                receiver: Mutex::new(Some(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            }),
            SchedulerConfig::default(),
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            manager.refresh_index(
                IndexOptions {
                    root: Some(workspace.path().to_path_buf()),
                    ..IndexOptions::default()
                },
                false,
            ),
        )
        .await
        .expect("background must return without a permit")
        .expect("scheduled");
        jobs.recv().await.expect("refresh should start");
        assert_eq!(manager.snapshot().jobs.running, 1);
        executor.release.add_permits(1);
        manager
            .inner
            .scheduler
            .wait_for_root_idle(&workspace.path().canonicalize().expect("root"))
            .await;
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn wait_refresh_drains_running_and_watcher_successor_jobs() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let (started, mut jobs) = mpsc::unbounded_channel();
        let executor = Arc::new(GatedExecutor {
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (sender, receiver) = mpsc::channel(4);
        let manager = WorkspaceRuntimeManager::new(
            executor.clone(),
            Arc::new(ManualWatcherFactory {
                receiver: Mutex::new(Some(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            }),
            SchedulerConfig::default(),
        );
        let options = IndexOptions {
            root: Some(root.clone()),
            ..IndexOptions::default()
        };
        manager
            .submit_index(options.clone(), false)
            .await
            .expect("initial job");
        jobs.recv().await.expect("initial job started");
        let waiting_manager = manager.clone();
        let waiting =
            tokio::spawn(async move { waiting_manager.refresh_index(options, true).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.snapshot().jobs.queued == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("refresh must queue behind the running job");
        assert!(!waiting.is_finished());
        executor.release.add_permits(1);
        let refresh = jobs.recv().await.expect("refresh started");
        assert_eq!(refresh.changes, [IndexChange::Rescan]);
        sender
            .send(WorkspaceChangeBatch {
                changes: vec![WorkspaceChange::Upsert(PathBuf::from("late.rs"))],
            })
            .await
            .expect("watcher event");
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.snapshot().jobs.queued == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("watcher successor should queue");
        executor.release.add_permits(1);
        jobs.recv().await.expect("watcher successor started");
        assert!(
            !waiting.is_finished(),
            "wait must include watcher successors"
        );
        executor.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("wait finishes")
            .expect("task")
            .expect("fresh");
        assert_eq!(manager.snapshot().jobs.running, 0);
        assert_eq!(manager.snapshot().jobs.queued, 0);
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn successful_index_starts_one_watcher_and_changes_submit_a_narrow_job() {
        let workspace = tempdir().expect("workspace should be created");
        let canonical_root = std::fs::canonicalize(workspace.path()).expect("root should resolve");
        let executor = Arc::new(RecordingExecutor::default());
        let (sender, receiver) = mpsc::channel(4);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Mutex::new(Some(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        });
        let manager = WorkspaceRuntimeManager::new(
            executor.clone(),
            watchers.clone(),
            SchedulerConfig::default(),
        );

        let indexed = manager
            .submit_index(
                IndexOptions {
                    root: Some(workspace.path().to_path_buf()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("index should complete");
        assert_eq!(indexed.job.canonical_root, canonical_root);
        assert_eq!(manager.snapshot().active_runtimes, 1);
        assert!(manager.runtime_snapshot(&canonical_root).watcher_active);
        assert_eq!(
            watchers
                .watches
                .lock()
                .expect("watches should be readable")
                .len(),
            1
        );

        sender
            .send(WorkspaceChangeBatch {
                changes: vec![WorkspaceChange::Upsert(PathBuf::from("src/lib.rs"))],
            })
            .await
            .expect("watch change should be delivered");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if executor
                    .calls
                    .lock()
                    .expect("calls should be readable")
                    .len()
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("watch index should run");
        {
            let calls = executor.calls.lock().expect("calls should be readable");
            assert_eq!(
                calls[1].changes,
                [IndexChange::Upsert(PathBuf::from("src/lib.rs"))]
            );
        }

        sender
            .send(WorkspaceChangeBatch {
                changes: vec![WorkspaceChange::Rescan],
            })
            .await
            .expect("reconcile should be delivered");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if executor
                    .calls
                    .lock()
                    .expect("calls should be readable")
                    .len()
                    == 3
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconcile index should run");
        assert_eq!(
            executor.calls.lock().expect("calls should be readable")[2].changes,
            [IndexChange::Rescan]
        );
    }

    #[tokio::test]
    async fn failed_index_keeps_the_runtime_visible_without_starting_a_watcher() {
        let workspace = tempdir().expect("workspace should be created");
        let (_sender, receiver) = mpsc::channel(1);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Mutex::new(Some(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        });
        let manager = WorkspaceRuntimeManager::new(
            Arc::new(FailingExecutor),
            watchers.clone(),
            SchedulerConfig::default(),
        );

        let failed = manager
            .submit_index(
                IndexOptions {
                    root: Some(workspace.path().to_path_buf()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("failed job should remain inspectable");

        assert_eq!(failed.job.state, crate::job_scheduler::JobState::Failed);
        assert_eq!(manager.snapshot().active_runtimes, 1);
        assert!(
            watchers
                .watches
                .lock()
                .expect("watches should be readable")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn drop_stops_the_watcher_releases_the_runtime_and_then_drops_storage() {
        let workspace = tempdir().expect("workspace should be created");
        let executor = Arc::new(RecordingExecutor::default());
        let (_sender, receiver) = mpsc::channel(1);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Mutex::new(Some(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        });
        let manager = WorkspaceRuntimeManager::new(
            executor.clone(),
            watchers.clone(),
            SchedulerConfig::default(),
        );
        manager
            .submit_index(
                IndexOptions {
                    root: Some(workspace.path().to_path_buf()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("index should activate the runtime");

        let removed = manager
            .drop_index(InfoOptions {
                root: Some(workspace.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("drop should complete");

        assert!(removed);
        assert_eq!(watchers.closes.load(Ordering::Acquire), 1);
        assert_eq!(executor.drops.load(Ordering::Acquire), 1);
        assert_eq!(manager.snapshot().active_runtimes, 0);
    }

    #[tokio::test]
    async fn shutdown_closes_all_watchers_and_rejects_later_jobs() {
        let workspace = tempdir().expect("workspace should be created");
        let executor = Arc::new(RecordingExecutor::default());
        let (_sender, receiver) = mpsc::channel(1);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Mutex::new(Some(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        });
        let manager =
            WorkspaceRuntimeManager::new(executor, watchers.clone(), SchedulerConfig::default());
        let options = IndexOptions {
            root: Some(workspace.path().to_path_buf()),
            ..IndexOptions::default()
        };
        manager
            .submit_index(options.clone(), true)
            .await
            .expect("index should activate a watcher");

        manager.shutdown_all().await.expect("shutdown should drain");

        assert_eq!(watchers.closes.load(Ordering::Acquire), 1);
        assert_eq!(manager.snapshot().active_runtimes, 0);
        assert!(manager.submit_index(options, false).await.is_err());
    }
}
