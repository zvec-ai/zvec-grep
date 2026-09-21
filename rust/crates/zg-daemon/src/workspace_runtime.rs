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
        index::{
            IndexOptions, IndexResult,
            options::{ScanRulesUpdate, WorkspaceChange as IndexChange},
        },
        info::{InfoOptions, InfoResult, result::IndexStatusSnapshot},
    },
};
use zg_host_native::{
    AllowAllPaths, HostError, HostErrorSite, RootSpec, TaskControl, WatchRequest, WorkspaceChange,
    WorkspaceWatchSessionPort, WorkspaceWatcherFactoryPort,
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
    pending_watcher_configuration: Mutex<Option<(uuid::Uuid, IndexOptions)>>,
    index_status: Mutex<CachedIndexStatus>,
    watcher: tokio::sync::Mutex<Option<WatcherHandle>>,
    watcher_active: AtomicBool,
    dirty_revision: AtomicU64,
    indexed_revision: AtomicU64,
}

/// The epoch prevents a scan started before a mutation from restoring an invalidated snapshot.
#[derive(Default)]
struct CachedIndexStatus {
    epoch: u64,
    snapshot: Option<IndexStatusSnapshot>,
    job: Option<(uuid::Uuid, JobState)>,
}

impl CachedIndexStatus {
    fn invalidate(&mut self) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        self.snapshot = None;
        self.epoch
    }

    fn record(
        &mut self,
        epoch: u64,
        info: &InfoResult,
        job: Option<(uuid::Uuid, JobState)>,
    ) -> bool {
        if self.epoch != epoch {
            return false;
        }
        self.job = job;
        self.snapshot = Some(IndexStatusSnapshot {
            status: info.index_status(),
            stats: info.status.clone(),
            checked_epoch_ms: u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
        });
        true
    }
}

impl WorkspaceRuntime {
    fn invalidate_status(&self) {
        lock(&self.index_status).invalidate();
    }
}

struct WatcherHandle {
    configured_job: Option<uuid::Uuid>,
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
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let signal = options.signal.clone().unwrap_or_default();
        loop {
            let result = self.engine.index(options.clone()).await;
            if !result
                .as_ref()
                .is_err_and(|error| error.code() == EngineError::RESOURCE_BUSY)
                || tokio::time::Instant::now() >= deadline
            {
                return result;
            }
            // Resident reads may briefly own the lock. Retry only lock contention,
            // retaining cancellation and the original one-operation request.
            tokio::select! {
                () = signal.cancelled() => return Err(EngineError::cancelled("index request was cancelled")),
                () = tokio::time::sleep_until((tokio::time::Instant::now()
                    + std::time::Duration::from_millis(50)).min(deadline)) => {}
            }
        }
    }

    async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        self.engine.drop_index(options).await
    }
}

struct EngineWatcherFactory {
    engine: Arc<ZvecGrep>,
}

#[async_trait::async_trait]
impl WorkspaceWatcherFactoryPort for EngineWatcherFactory {
    async fn watch(
        &self,
        request: &WatchRequest,
        control: &TaskControl,
    ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
        self.engine
            .watch_workspace(&request.root.path, control)
            .await
            .map_err(|error| HostError::storage_failure("engine-file-selection", error.to_string()))
    }
}

impl WorkspaceRuntimeManager {
    pub(crate) fn native(engine: Arc<ZvecGrep>) -> Self {
        Self::new(
            Arc::new(ZvecGrepIndexExecutor {
                engine: Arc::clone(&engine),
            }),
            Arc::new(EngineWatcherFactory { engine }),
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
        let reporter = options.on_progress.take();
        if options
            .signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(WorkspaceRuntimeError::Engine(EngineError::cancelled(
                "index request was cancelled",
            )));
        }
        if !wait {
            // A submitted background job outlives the request that created it.
            options.signal = None;
        }
        let canonical_root = canonical_root(options.root.as_deref())?;
        options.root = Some(canonical_root.clone());
        let runtime = self.runtime(canonical_root.clone(), &options);
        let reconfigure = options.reset_paths || options.scan != ScanRulesUpdate::default();
        let template = index_template(&options);
        runtime.invalidate_status();
        let target_revision = runtime.dirty_revision.load(Ordering::Acquire);
        let submitted = self
            .inner
            .scheduler
            .submit(canonical_root, options, JobReason::Manual)?;
        {
            let manager = self.clone();
            let job_id = submitted.job.id;
            let template = template.clone();
            tokio::spawn(async move {
                let Ok(completed) = manager.inner.scheduler.wait(job_id).await else {
                    return;
                };
                manager.invalidate_status(&completed.job.canonical_root);
                if completed.job.state == JobState::Succeeded {
                    let _ = manager
                        .on_index_succeeded(completed.job, target_revision, template, reconfigure)
                        .await;
                }
            });
        }
        if !wait {
            return Ok(RuntimeIndexSubmission {
                job: submitted.job,
                reused: submitted.reused,
                result: None,
            });
        }

        let completed = self
            .inner
            .scheduler
            .wait_with_progress(submitted.job.id, reporter)
            .await?;
        runtime.invalidate_status();
        if completed.job.state == JobState::Succeeded
            && let Err(error) = self
                .on_index_succeeded(
                    completed.job.clone(),
                    target_revision,
                    template,
                    reconfigure,
                )
                .await
        {
            warn!(%error, root = %completed.job.canonical_root.display(), "index succeeded but watcher activation failed");
        }
        if completed.job.state == JobState::Succeeded {
            // Policy installation acknowledges its reconciliation before returning.
            // A caller waiting for indexing must not race that followup writer.
            self.inner
                .scheduler
                .wait_for_root_idle(&runtime.canonical_root)
                .await;
            if let Err(error) = self.ensure_watching(Arc::clone(&runtime)).await {
                warn!(%error, "index completed with pending watcher configuration");
            }
            self.inner
                .scheduler
                .wait_for_root_idle(&runtime.canonical_root)
                .await;
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
        let reporter = options.on_progress.take();
        if !wait {
            options.signal = None;
        }
        options.changes = vec![IndexChange::Rescan];
        if wait {
            self.ensure_watching(Arc::clone(&runtime))
                .await
                .map_err(WorkspaceRuntimeError::into_engine_error)?;
            self.flush_watcher(&runtime).await?;
        }
        runtime.invalidate_status();
        let revision = runtime.dirty_revision.load(Ordering::Acquire);
        let submitted = self
            .inner
            .scheduler
            .submit(root.clone(), options, JobReason::Watch)
            .map_err(|error| WorkspaceRuntimeError::from(error).into_engine_error())?;
        {
            let manager = self.clone();
            let runtime = Arc::clone(&runtime);
            let job_id = submitted.job.id;
            tokio::spawn(async move {
                if let Ok(completed) = manager.inner.scheduler.wait(job_id).await {
                    runtime.invalidate_status();
                    if completed.job.state != JobState::Succeeded {
                        return;
                    }
                    runtime
                        .indexed_revision
                        .fetch_max(revision, Ordering::AcqRel);
                    if let Err(error) = manager.ensure_watching(runtime).await {
                        warn!(%error, "search refresh watcher activation failed");
                    }
                }
            });
        }
        if !wait {
            return Ok(());
        }
        let completed = self
            .inner
            .scheduler
            .wait_with_progress(submitted.job.id, reporter)
            .await
            .map_err(|error| WorkspaceRuntimeError::from(error).into_engine_error())?;
        runtime.invalidate_status();
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

    /// Explicit inspection always reads disk. The runtime keeps the observation for cheap
    /// runtime snapshots, never as permission to skip a later freshness check.
    pub(crate) async fn info(
        &self,
        engine: &ZvecGrep,
        options: InfoOptions,
    ) -> Result<InfoResult, EngineError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(EngineError::resource_closed(
                "workspace runtimes have been closed",
            ));
        }
        if !options.include_status {
            return engine.info(options).await;
        }
        // Resolve a subdirectory to the owning workspace before taking a cache epoch.
        let metadata = engine
            .info(InfoOptions {
                root: options.root.clone(),
                include_status: false,
            })
            .await?;
        let runtime = self.runtime(
            metadata.root.clone(),
            &IndexOptions {
                root: Some(metadata.root.clone()),
                ..IndexOptions::default()
            },
        );
        let epoch = lock(&runtime.index_status).invalidate();
        let observed_job = self
            .job_for_root(&runtime.canonical_root)
            .map(|job| (job.id, job.state));
        let mut info = engine
            .info(InfoOptions {
                root: Some(metadata.root),
                include_status: true,
            })
            .await?;
        let current_job = self.job_for_root(&info.root).map(|job| (job.id, job.state));
        let busy = current_job
            .is_some_and(|(_, state)| matches!(state, JobState::Queued | JobState::Running));
        // Never restore ready after a watcher notification, job submission or concurrent scan.
        if busy
            || current_job != observed_job
            || !lock(&runtime.index_status).record(epoch, &info, current_job)
        {
            info.status = None;
        }
        Ok(info)
    }

    fn invalidate_status(&self, root: &Path) {
        if let Some(runtime) = lock(&self.inner.runtimes).get(root) {
            runtime.invalidate_status();
        }
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
        self.invalidate_status(&canonical_root);
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
            let template = index_template(options);
            Arc::new(WorkspaceRuntime {
                canonical_root,
                index_template: Mutex::new(template),
                pending_watcher_configuration: Mutex::new(None),
                index_status: Mutex::new(CachedIndexStatus::default()),
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
        template: IndexOptions,
        reconfigure: bool,
    ) -> Result<(), WorkspaceRuntimeError> {
        let runtime = lock(&self.inner.runtimes).get(&job.canonical_root).cloned();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        runtime
            .indexed_revision
            .fetch_max(revision, Ordering::AcqRel);
        let result = if reconfigure {
            self.configure_watcher(Arc::clone(&runtime), Some((job.id, template)))
                .await
        } else {
            *lock(&runtime.index_template) = template;
            self.ensure_watching(Arc::clone(&runtime)).await
        };
        if result.is_err() {
            // A successor may already hold the workspace write lock. Keep the old
            // session alive and retry the pending policy after the writer drains.
            let manager = self.clone();
            tokio::spawn(async move {
                manager
                    .inner
                    .scheduler
                    .wait_for_root_idle(&runtime.canonical_root)
                    .await;
                if let Err(error) = manager.ensure_watching(runtime).await {
                    warn!(%error, "pending watcher configuration could not be activated");
                }
            });
        }
        result
    }

    fn ensure_watching(
        &self,
        runtime: Arc<WorkspaceRuntime>,
    ) -> futures::future::BoxFuture<'_, Result<(), WorkspaceRuntimeError>> {
        // Watch completion may trigger replacement of its own session; erase the
        // future type to break the configure -> watch loop -> configure type cycle.
        Box::pin(self.configure_watcher(runtime, None))
    }

    async fn configure_watcher(
        &self,
        runtime: Arc<WorkspaceRuntime>,
        completed: Option<(uuid::Uuid, IndexOptions)>,
    ) -> Result<(), WorkspaceRuntimeError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut watcher = runtime.watcher.lock().await;
        if let Some(completed) = completed {
            *lock(&runtime.pending_watcher_configuration) = Some(completed);
        }
        let pending = lock(&runtime.pending_watcher_configuration).clone();
        let configured_job = pending.as_ref().map(|(id, _)| *id);
        if let Some(handle) = watcher.as_ref()
            && !handle.task.is_finished()
            && (configured_job.is_none() || handle.configured_job == configured_job)
        {
            lock(&runtime.pending_watcher_configuration).take();
            return Ok(());
        }
        let cancellation = self.inner.shutdown.child_token();
        let session = self
            .inner
            .watcher_factory
            .watch(
                &WatchRequest {
                    // The native factory resolves saved engine policy; test factories
                    // can still use this request solely as a root identifier.
                    root: RootSpec::new(runtime.canonical_root.clone(), Arc::new(AllowAllPaths)),
                },
                &TaskControl::new(cancellation.clone()),
            )
            .await?;
        // Prepare the new policy before retiring the current session. A failed
        // initialization leaves the previous watcher running.
        if let Some(previous) = watcher.take() {
            previous.cancellation.cancel();
            if let Err(error) = previous.session.close().await {
                warn!(%error, "previous watcher close failed during replacement");
            }
            if let Err(error) = previous.task.await {
                warn!(%error, "previous watcher task failed during replacement");
            }
        }
        if let Some((_, template)) = pending {
            *lock(&runtime.index_template) = template;
            lock(&runtime.pending_watcher_configuration).take();
        }
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
            configured_job,
            barrier,
            cancellation,
            session,
            task,
        });
        // Cover the interval between indexing and installing a watcher, including
        // first activation. A barrier ensures the daemon has queued the reconciliation.
        if let Some(handle) = watcher.as_ref() {
            handle.session.flush().await?;
            let (acknowledge, acknowledged) = oneshot::channel();
            handle
                .barrier
                .send(acknowledge)
                .await
                .map_err(|_| EngineError::resource_closed("workspace watcher has stopped"))?;
            acknowledged.await.map_err(|_| {
                EngineError::resource_closed("workspace watcher barrier was interrupted")
            })?;
        }
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
            result: submitted.result,
        })
    }

    async fn info(
        &self,
        engine: &ZvecGrep,
        options: InfoOptions,
    ) -> Result<InfoResult, EngineError> {
        WorkspaceRuntimeManager::info(self, engine, options).await
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
            on_progress: request.on_progress.clone(),
            signal: request.signal.clone(),
            allow_remote: request.allow_remote,
            authorized_remote: request.authorized_remote.clone(),
            api_key: request.api_key.clone(),
            endpoint: request.endpoint.clone(),
            embedding_concurrency: request.embedding_concurrency,
            device: request.device,
            model_cache: request.model_cache.clone(),
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
        info.compatibility.ensure_compatible()?;
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
        let runtime = lock(&self.inner.runtimes).get(&canonical_root).cloned()?;
        let snapshot = WorkspaceRuntimeManager::runtime_snapshot(self, &canonical_root);
        let job = self.job_for_root(&canonical_root);
        let index_status = if job
            .as_ref()
            .is_some_and(|job| matches!(job.state, JobState::Queued | JobState::Running))
        {
            None
        } else {
            let cache = lock(&runtime.index_status);
            let current_job = job.as_ref().map(|job| (job.id, job.state));
            (cache.job == current_job)
                .then(|| cache.snapshot.clone())
                .flatten()
        };
        Some(IndexRuntimeSnapshot {
            index_status,
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
        runtime.invalidate_status();
        let target_revision = runtime.dirty_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let mut options = lock(&runtime.index_template).clone();
        // One-operation consent must never authorize later watcher jobs.
        options.allow_remote = false;
        options.authorized_remote.clear();
        options.name = None;
        options.signal = None;
        options.on_progress = None;
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
        let manager = WorkspaceRuntimeManager {
            inner: Arc::clone(&inner),
        };
        let weak_runtime = Arc::downgrade(&runtime);
        tokio::spawn(async move {
            let Ok(completed) = scheduler.wait(submitted.job.id).await else {
                return;
            };
            if let Some(runtime) = weak_runtime.upgrade() {
                runtime.invalidate_status();
                if completed.job.state != JobState::Succeeded {
                    return;
                }
                runtime
                    .indexed_revision
                    .fetch_max(target_revision, Ordering::AcqRel);
                if let Err(error) = manager.ensure_watching(runtime).await {
                    warn!(%error, "watch successor could not activate pending configuration");
                }
            }
        });
    }
    if let Some(runtime) = weak_runtime.upgrade() {
        runtime.invalidate_status();
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

fn index_template(options: &IndexOptions) -> IndexOptions {
    let mut template = options.clone();
    // Saved workspace settings are authoritative for subsequent watch jobs.
    // Configuration patches and one-operation controls must never be replayed.
    template.name = None;
    template.signal = None;
    template.on_progress = None;
    template.allow_remote = false;
    template.authorized_remote.clear();
    template.rebuild = false;
    template.reset_paths = false;
    template.changes.clear();
    template.scan = ScanRulesUpdate::default();
    template.embedding = None;
    template
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
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
        fail: AtomicBool,
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
            if self.fail.load(Ordering::Acquire) {
                return Err(EngineError::internal("fixture index failed"));
            }
            Ok(IndexResult::default())
        }

        async fn drop_index(&self, _options: InfoOptions) -> Result<bool, EngineError> {
            self.drops.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }
    }

    struct ManualWatcherFactory {
        receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<WorkspaceChangeBatch>>>,
        watches: Mutex<Vec<WatchRequest>>,
        closes: Arc<AtomicUsize>,
    }

    struct ManualWatchSession {
        receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<WorkspaceChangeBatch>>>,
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
            Ok(Arc::new(ManualWatchSession {
                receiver: Arc::clone(&self.receiver),
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

    struct BusyWatcherFactory {
        inner: ManualWatcherFactory,
        busy: AtomicBool,
        blocked: tokio::sync::Notify,
    }

    #[async_trait]
    impl WorkspaceWatcherFactoryPort for BusyWatcherFactory {
        async fn watch(
            &self,
            request: &WatchRequest,
            control: &TaskControl,
        ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
            if self.busy.load(Ordering::Acquire) {
                self.blocked.notify_one();
                return Err(HostError::storage_failure(
                    "engine-file-selection",
                    "workspace is locked by successor",
                ));
            }
            self.inner.watch(request, control).await
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

    fn inspected_info() -> zg_engine::api::info::InfoResult {
        use zg_engine::api::info::result::{
            IndexCompatibility, IndexStats, InfoSource, WorkspaceIndexPolicy,
        };
        zg_engine::api::info::InfoResult {
            root: "/workspace".into(),
            indexed: true,
            compatibility: IndexCompatibility::Compatible { version: 2 },
            index_policy: WorkspaceIndexPolicy::Enabled,
            home: "/workspace/.zvec-grep".into(),
            index_path: "/workspace/.zvec-grep/storage".into(),
            source: InfoSource::Index,
            workspace_index: None,
            status: Some(IndexStats::default()),
            suggestion: None,
        }
    }

    #[test]
    fn invalidation_rejects_a_scan_that_started_before_a_change() {
        use zg_engine::api::info::result::IndexStatus;
        let mut cache = super::CachedIndexStatus::default();
        assert!(cache.snapshot.is_none());
        let scan = cache.invalidate();
        cache.invalidate(); // A watcher notification or write arrives while scanning.
        assert!(!cache.record(scan, &inspected_info(), None));
        assert!(cache.snapshot.is_none());
        let new_scan = cache.invalidate();
        assert!(cache.record(new_scan, &inspected_info(), None));
        let snapshot = cache.snapshot.as_ref().expect("fresh inspection");
        assert_eq!(snapshot.status, IndexStatus::Ready);
        assert!(snapshot.checked_epoch_ms > 0);
        assert!(snapshot.stats.is_some());
        cache.invalidate();
        assert!(cache.snapshot.is_none());
    }

    #[tokio::test]
    async fn inspection_populates_runtime_memory_and_drop_removes_it() {
        use zg_engine::api::info::result::IndexStatus;
        use zg_transport_mcp::IndexOperationProvider;
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let engine = zg_engine::ZvecGrep::new();
        let manager = WorkspaceRuntimeManager::native(Arc::new(zg_engine::ZvecGrep::new()));
        let options = InfoOptions {
            root: Some(root.clone()),
            include_status: true,
        };
        assert!(IndexOperationProvider::runtime_snapshot(&manager, &root).is_none());
        let reply = manager
            .info(&engine, options.clone())
            .await
            .expect("inspection");
        assert_eq!(reply.index_status(), IndexStatus::Uninitialized);
        let snapshot = IndexOperationProvider::runtime_snapshot(&manager, &root).expect("runtime");
        assert_eq!(
            snapshot.index_status.expect("inspection").status,
            IndexStatus::Uninitialized
        );
        assert!(!snapshot.watcher_active);
        assert!(
            !root.join(".zvec-grep").exists(),
            "inspection must not create storage"
        );
        manager.drop_index(options).await.expect("drop");
        assert!(IndexOperationProvider::runtime_snapshot(&manager, &root).is_none());
        manager.shutdown_all().await.expect("shutdown");
        engine.close();
    }

    #[tokio::test]
    async fn failed_rebuild_invalidates_the_previous_status() {
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let (_sender, receiver) = mpsc::channel(4);
        let manager = WorkspaceRuntimeManager::new(
            Arc::new(FailingExecutor),
            Arc::new(ManualWatcherFactory {
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            }),
            SchedulerConfig::default(),
        );
        let runtime = manager.runtime(root.clone(), &IndexOptions::default());
        let epoch = super::lock(&runtime.index_status).invalidate();
        assert!(super::lock(&runtime.index_status).record(epoch, &inspected_info(), None));
        let submitted = manager
            .submit_index(
                IndexOptions {
                    root: Some(root),
                    rebuild: true,
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("job result");
        assert_eq!(submitted.job.state, crate::job_scheduler::JobState::Failed);
        assert!(super::lock(&runtime.index_status).snapshot.is_none());
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn wait_refresh_propagates_job_failure() {
        let workspace = tempdir().expect("workspace");
        let (_sender, receiver) = mpsc::channel(4);
        let manager = WorkspaceRuntimeManager::new(
            Arc::new(FailingExecutor),
            Arc::new(ManualWatcherFactory {
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
                    name: Some("explicit-workspace-name".into()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("index should complete");
        assert_eq!(indexed.job.canonical_root, canonical_root);
        assert_eq!(manager.snapshot().active_runtimes, 1);
        assert!(manager.runtime_snapshot(&canonical_root).watcher_active);
        let runtime = manager.runtime(canonical_root.clone(), &IndexOptions::default());
        assert!(super::lock(&runtime.index_template).name.is_none());
        assert_eq!(
            watchers
                .watches
                .lock()
                .expect("watches should be readable")
                .len(),
            1
        );

        let epoch = super::lock(&runtime.index_status).invalidate();
        assert!(super::lock(&runtime.index_status).record(epoch, &inspected_info(), None));

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
        assert!(super::lock(&runtime.index_status).snapshot.is_none());
        {
            let calls = executor.calls.lock().expect("calls should be readable");
            assert_eq!(calls[0].name.as_deref(), Some("explicit-workspace-name"));
            assert!(calls[1].name.is_none());
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
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
    #[tokio::test]
    async fn manual_updates_replace_the_watcher_once_without_replaying_configuration() {
        use zg_engine::api::index::options::ScanRulesUpdate;
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let executor = Arc::new(RecordingExecutor::default());
        let (sender, receiver) = mpsc::channel(4);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
                    root: Some(root.clone()),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("initial index");
        manager
            .submit_index(
                IndexOptions {
                    root: Some(root.clone()),
                    rebuild: true,
                    reset_paths: true,
                    allow_remote: true,
                    authorized_remote: vec![zg_engine::authorization::IndexAuthorization {
                        root: root.clone(),
                        workspace_roots: vec![root.clone()],
                        model: "qwen/text-embedding-v4".into(),
                        endpoint: "https://once.example.test/embedding".into(),
                        endpoint_host: "once.example.test".into(),
                    }],
                    scan: ScanRulesUpdate {
                        globs: Some(vec!["*.rs".into()]),
                        hidden: Some(false),
                        max_depth: Some(None),
                        ..ScanRulesUpdate::default()
                    },
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("manual reconfiguration");
        assert_eq!(watchers.watches.lock().expect("watches").len(), 2);
        assert_eq!(watchers.closes.load(Ordering::Acquire), 1);
        sender
            .send(WorkspaceChangeBatch {
                changes: vec![WorkspaceChange::Upsert("changed.rs".into())],
            })
            .await
            .expect("watch change");
        tokio::time::timeout(Duration::from_secs(2), async {
            while executor.calls.lock().expect("calls").len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("watch job starts");
        manager.inner.scheduler.wait_for_root_idle(&root).await;
        {
            let calls = executor.calls.lock().expect("calls");
            let watched = &calls[2];
            assert_eq!(watched.scan, ScanRulesUpdate::default());
            assert!(!watched.rebuild);
            assert!(!watched.reset_paths);
            assert!(!watched.allow_remote);
            assert!(watched.authorized_remote.is_empty());
            assert_eq!(
                watched.changes,
                vec![IndexChange::Upsert("changed.rs".into())]
            );
        }
        assert_eq!(
            watchers.watches.lock().expect("watches").len(),
            2,
            "watch completion must not restart its own session"
        );
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pending_policy_retries_after_a_busy_successor_without_losing_the_old_watcher() {
        use zg_engine::api::index::options::ScanRulesUpdate;
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let (started, mut calls) = mpsc::unbounded_channel();
        let executor = Arc::new(GatedExecutor {
            started,
            release: tokio::sync::Semaphore::new(0),
        });
        let (_sender, receiver) = mpsc::channel(4);
        let watchers = Arc::new(BusyWatcherFactory {
            inner: ManualWatcherFactory {
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            },
            busy: AtomicBool::new(false),
            blocked: tokio::sync::Notify::new(),
        });
        let manager = WorkspaceRuntimeManager::new(
            executor.clone(),
            watchers.clone(),
            SchedulerConfig::default(),
        );
        let runtime = manager.runtime(root.clone(), &IndexOptions::default());
        manager
            .ensure_watching(runtime.clone())
            .await
            .expect("old watcher");
        watchers.busy.store(true, Ordering::Release);
        manager
            .submit_index(
                IndexOptions {
                    root: Some(root.clone()),
                    embedding_concurrency: Some(5),
                    scan: ScanRulesUpdate {
                        hidden: Some(true),
                        ..ScanRulesUpdate::default()
                    },
                    ..IndexOptions::default()
                },
                false,
            )
            .await
            .expect("manual reconfiguration");
        calls.recv().await.expect("manual writer starts");
        let successor = manager
            .inner
            .scheduler
            .submit(
                root.clone(),
                IndexOptions {
                    root: Some(root.clone()),
                    changes: vec![IndexChange::Rescan],
                    ..IndexOptions::default()
                },
                crate::job_scheduler::JobReason::Watch,
            )
            .expect("watch successor");
        executor.release.add_permits(1);
        calls.recv().await.expect("successor starts");
        tokio::time::timeout(Duration::from_secs(2), watchers.blocked.notified())
            .await
            .expect("policy refresh observes busy writer");
        assert_eq!(watchers.inner.closes.load(Ordering::Acquire), 0);
        assert!(super::lock(&runtime.pending_watcher_configuration).is_some());
        watchers.busy.store(false, Ordering::Release);
        executor.release.add_permits(1);
        manager
            .inner
            .scheduler
            .wait(successor.job.id)
            .await
            .expect("successor completes");
        tokio::time::timeout(Duration::from_secs(2), async {
            while watchers.inner.watches.lock().expect("watches").len() < 2
                || super::lock(&runtime.pending_watcher_configuration).is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending policy is retried after idle");
        assert_eq!(watchers.inner.closes.load(Ordering::Acquire), 1);
        assert_eq!(
            super::lock(&runtime.index_template).embedding_concurrency,
            Some(5)
        );
        assert!(super::lock(&runtime.pending_watcher_configuration).is_none());
        manager.shutdown_all().await.expect("shutdown");
    }

    #[tokio::test]
    async fn failed_manual_update_preserves_the_live_watcher_and_execution_template() {
        use zg_engine::api::index::options::ScanRulesUpdate;
        let workspace = tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        let executor = Arc::new(RecordingExecutor::default());
        let (_sender, receiver) = mpsc::channel(4);
        let watchers = Arc::new(ManualWatcherFactory {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
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
                    root: Some(root.clone()),
                    embedding_concurrency: Some(2),
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("initial index");
        executor.fail.store(true, Ordering::Release);
        let failed = manager
            .submit_index(
                IndexOptions {
                    root: Some(root.clone()),
                    embedding_concurrency: Some(99),
                    scan: ScanRulesUpdate {
                        hidden: Some(true),
                        ..ScanRulesUpdate::default()
                    },
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("failed result");
        assert_eq!(failed.job.state, crate::job_scheduler::JobState::Failed);
        assert_eq!(watchers.watches.lock().expect("watches").len(), 1);
        assert_eq!(watchers.closes.load(Ordering::Acquire), 0);
        let runtime = manager.runtime(root.clone(), &IndexOptions::default());
        assert_eq!(
            super::lock(&runtime.index_template).embedding_concurrency,
            Some(2)
        );
        assert!(manager.runtime_snapshot(&root).watcher_active);
        manager.shutdown_all().await.expect("shutdown");
    }
}
