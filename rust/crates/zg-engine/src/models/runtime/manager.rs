//! Process-level ownership and reuse of embedding model runtimes.

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    fmt,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::compute::ModelComputeRuntime;
use crate::domain::{
    Content,
    model::{Device, EmbeddingModelInfo, EmbeddingResult, ModelConfig},
};
use crate::models::{
    backends::create_embedding_model,
    spi::{
        EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions, ModelError,
        ModelProgressReporter,
    },
};

type ModelFactory = dyn Fn(&str, ModelConfig, ModelComputeRuntime) -> Result<Arc<dyn EmbeddingModel>, ModelError>
    + Send
    + Sync;

/// Shared owner for model runtimes used by one `ZvecGrep` process instance.
#[derive(Clone)]
pub(crate) struct ModelRuntimeManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    factory: Arc<ModelFactory>,
    compute_runtime: ModelComputeRuntime,
    state: Mutex<ManagerState>,
    policy: CachePolicy,
}

#[derive(Default)]
struct ManagerState {
    closed: bool,
    entries: HashMap<ModelRuntimeKey, CachedRuntime>,
    maintenance: Option<SyncSender<()>>,
}

#[derive(Clone, Copy)]
struct CachePolicy {
    idle_timeout: Duration,
    capacity: usize,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_mins(15),
            capacity: 1,
        }
    }
}

struct CachedRuntime {
    entry: Arc<ModelRuntimeEntry>,
    // Protected by the manager lock, together with lease acquisition and release.
    idle_since: Option<Instant>,
}

struct ModelRuntimeEntry {
    runtime: Arc<ModelRuntime>,
    leases: AtomicUsize,
}

struct ModelRuntime {
    model: Arc<dyn EmbeddingModel>,
    active_embeddings: AtomicUsize,
}

/// Configuration that determines whether two callers may share one model.
pub(crate) struct ModelRuntimeRequest {
    reference: String,
    options: ModelConfig,
    embedding_concurrency: Option<usize>,
}

impl ModelRuntimeRequest {
    pub(crate) fn new(
        reference: impl Into<String>,
        options: ModelConfig,
        embedding_concurrency: Option<usize>,
    ) -> Self {
        Self {
            reference: reference.into(),
            options,
            embedding_concurrency,
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct ModelRuntimeKey {
    reference: String,
    api_key_fingerprint: Option<u64>,
    endpoint: Option<String>,
    model_cache_dir: Option<PathBuf>,
    device: Option<Device>,
}

/// A counted handle to a shared model runtime.
pub(crate) struct ModelRuntimeLease {
    key: ModelRuntimeKey,
    entry: Arc<ModelRuntimeEntry>,
    manager: Weak<ManagerInner>,
    operation: Arc<OperationConcurrency>,
}

struct OperationConcurrency {
    limit: usize,
    permits: Arc<Semaphore>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModelRuntimeSnapshot {
    pub(crate) cached_runtimes: usize,
    pub(crate) active_leases: usize,
    pub(crate) active_embeddings: usize,
}

impl ModelRuntimeManager {
    pub(crate) fn new() -> Self {
        Self::with_factory(|reference, options, compute| {
            create_embedding_model(reference, Some(options), compute)
        })
    }

    fn with_factory(
        factory: impl Fn(
            &str,
            ModelConfig,
            ModelComputeRuntime,
        ) -> Result<Arc<dyn EmbeddingModel>, ModelError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::with_policy(Arc::new(factory), CachePolicy::default())
    }

    fn with_policy(factory: Arc<ModelFactory>, policy: CachePolicy) -> Self {
        let compute_runtime = ModelComputeRuntime::shared();
        Self {
            inner: Arc::new(ManagerInner {
                factory,
                compute_runtime,
                state: Mutex::new(ManagerState::default()),
                policy,
            }),
        }
    }

    /// Returns a counted lease, reusing an existing runtime with the same key.
    /// Returns a counted lease, reusing an existing runtime with the same key.
    pub(crate) fn acquire(
        &self,
        request: ModelRuntimeRequest,
    ) -> Result<ModelRuntimeLease, ModelError> {
        let ModelRuntimeRequest {
            reference,
            options,
            embedding_concurrency,
        } = request;
        validate_embedding_concurrency(embedding_concurrency)?;
        let key = ModelRuntimeKey::new(&reference, &options);
        // Declare retired handles before the guard so even an error releases the
        // manager lock before running native model destructors.
        let mut retired = Vec::new();
        let mut state = self.lock_state();
        if state.closed {
            return Err(manager_closed());
        }
        self.inner.start_maintenance(&mut state)?;
        retired.extend(state.retire_idle(self.inner.policy, Instant::now()));

        let entry = if let Some(cached) = state.entries.get_mut(&key) {
            cached.idle_since = None;
            Arc::clone(&cached.entry)
        } else {
            // Model construction is intentionally performed while holding the
            // short-lived manager lock. Backends load heavy resources lazily,
            // so this guarantees a single instance without blocking on I/O.
            let model =
                (self.inner.factory)(&reference, options, self.inner.compute_runtime.clone())?;
            model.info().validate().map_err(|error| {
                ModelError::internal("Embedding model returned invalid metadata").with_cause(error)
            })?;
            let entry = Arc::new(ModelRuntimeEntry {
                runtime: Arc::new(ModelRuntime {
                    model,
                    active_embeddings: AtomicUsize::new(0),
                }),
                leases: AtomicUsize::new(0),
            });
            state.entries.insert(
                key.clone(),
                CachedRuntime {
                    entry: Arc::clone(&entry),
                    idle_since: None,
                },
            );
            entry
        };
        let concurrency = resolve_embedding_concurrency(
            embedding_concurrency,
            entry.runtime.model.concurrency_defaults(),
        );
        entry.leases.fetch_add(1, Ordering::AcqRel);
        retired.extend(state.retire_idle(self.inner.policy, Instant::now()));
        state.wake_maintenance();
        let lease = ModelRuntimeLease {
            key,
            entry,
            manager: Arc::downgrade(&self.inner),
            operation: Arc::new(OperationConcurrency {
                limit: concurrency,
                permits: Arc::new(Semaphore::new(concurrency)),
            }),
        };
        drop(state);
        drop(retired);
        Ok(lease)
    }

    /// Stops new acquisitions and retires runtimes without active leases.
    /// Stops new acquisitions and retires runtimes without active leases.
    pub(crate) fn close(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        // Disconnecting the channel stops maintenance without retaining the manager
        // or waiting for model destructors while holding its state lock.
        state.maintenance.take();
        let retired = state.retire_idle(self.inner.policy, Instant::now());
        drop(state);
        drop(retired);
    }

    pub(crate) fn snapshot(&self) -> ModelRuntimeSnapshot {
        let state = self.lock_state();
        ModelRuntimeSnapshot {
            cached_runtimes: state.entries.len(),
            active_leases: state
                .entries
                .values()
                .map(|cached| cached.entry.leases.load(Ordering::Acquire))
                .sum(),
            active_embeddings: state
                .entries
                .values()
                .map(|cached| {
                    cached
                        .entry
                        .runtime
                        .active_embeddings
                        .load(Ordering::Acquire)
                })
                .sum(),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ManagerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl ManagerState {
    fn retire_idle(&mut self, policy: CachePolicy, now: Instant) -> Vec<Arc<ModelRuntimeEntry>> {
        let mut candidates = self
            .entries
            .iter()
            .filter_map(|(key, cached)| {
                cached
                    .idle_since
                    .filter(|_| {
                        cached.entry.leases.load(Ordering::Acquire) == 0
                            && cached
                                .entry
                                .runtime
                                .active_embeddings
                                .load(Ordering::Acquire)
                                == 0
                    })
                    .map(|idle_since| (key.clone(), idle_since))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|(_, idle_since)| *idle_since);
        let mut retired = Vec::new();
        for (key, idle_since) in candidates {
            if !self.closed
                && now.saturating_duration_since(idle_since) < policy.idle_timeout
                && self.entries.len() <= policy.capacity
            {
                break;
            }
            if let Some(cached) = self.entries.remove(&key) {
                retired.push(cached.entry);
            }
        }
        retired
    }

    fn wake_maintenance(&self) {
        if let Some(sender) = &self.maintenance {
            // One pending wakeup is sufficient; release/acquire must never block.
            let _ = sender.try_send(());
        }
    }
}

impl ManagerInner {
    fn start_maintenance(self: &Arc<Self>, state: &mut ManagerState) -> Result<(), ModelError> {
        if state.maintenance.is_none() {
            let (sender, receiver) = mpsc::sync_channel(1);
            let weak = Arc::downgrade(self);
            // A native waiter also works for synchronous callers and engines used
            // across multiple Tokio runtimes. It owns no strong manager reference.
            std::thread::Builder::new()
                .name("model-cache".into())
                .spawn(move || maintain_cache(&weak, &receiver))
                .map_err(|error| {
                    ModelError::internal("Unable to start model cache maintenance")
                        .with_cause(error)
                })?;
            state.maintenance = Some(sender);
        }
        Ok(())
    }
}

fn maintain_cache(manager: &Weak<ManagerInner>, receiver: &Receiver<()>) {
    loop {
        let Some(inner) = manager.upgrade() else {
            return;
        };
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return;
        }
        let retired = state.retire_idle(inner.policy, Instant::now());
        let deadline = state
            .entries
            .values()
            .filter_map(|cached| {
                cached
                    .idle_since
                    .map(|idle_since| idle_since + inner.policy.idle_timeout)
            })
            .min();
        drop(state);
        drop(inner);
        drop(retired);
        // With no idle model there is no timer. Closing or dropping the manager
        // disconnects the channel and wakes this thread immediately.
        match deadline {
            Some(deadline) => {
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => {
                if receiver.recv().is_err() {
                    return;
                }
            }
        }
    }
}

impl Default for ModelRuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ModelRuntimeManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRuntimeManager")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl ModelRuntimeLease {
    pub(crate) fn matches_request(&self, request: &ModelRuntimeRequest) -> bool {
        self.key == ModelRuntimeKey::new(&request.reference, &request.options)
    }

    pub(crate) fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        self.entry.runtime.model.concurrency_defaults()
    }

    pub(crate) fn info(&self) -> &EmbeddingModelInfo {
        self.entry.runtime.model.info()
    }

    pub(crate) async fn embed(
        &self,
        inputs: &[Vec<Content>],
        mut options: EmbeddingOptions,
        progress: Option<ModelProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        let _permit = self
            .acquire_operation_permit(options.signal.as_ref())
            .await?;
        let _active = ActiveEmbeddingGuard::new(&self.entry.runtime.active_embeddings);
        options.execution_concurrency = self.operation.limit;
        if let Some(reporter) = progress {
            let model_progress = options.on_progress.take();
            let operation = Arc::clone(&self.operation);
            options.on_progress = Some(Arc::new(move |progress| {
                if let Some(model_progress) = &model_progress {
                    model_progress(progress.clone());
                }
                reporter.report(progress, operation.limit);
            }));
        }
        self.entry.runtime.model.embed(inputs, options).await
    }

    async fn acquire_operation_permit(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<OwnedSemaphorePermit, ModelError> {
        if let Some(signal) = signal {
            tokio::select! {
                permit = Arc::clone(&self.operation.permits).acquire_owned() => {
                    permit.map_err(|error| {
                        ModelError::internal(
                            "embedding concurrency limiter closed unexpectedly",
                        )
                            .with_cause(error)
                    })
                }
                () = signal.cancelled() => {
                    Err(ModelError::cancelled(
                        "Embedding was cancelled while waiting for compute capacity",
                    ))
                }
            }
        } else {
            Arc::clone(&self.operation.permits)
                .acquire_owned()
                .await
                .map_err(|error| {
                    ModelError::internal("embedding concurrency limiter closed unexpectedly")
                        .with_cause(error)
                })
        }
    }
}

impl Drop for ModelRuntimeLease {
    fn drop(&mut self) {
        let Some(manager) = self.manager.upgrade() else {
            let previous = self.entry.leases.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "model runtime lease count underflow");
            return;
        };
        let mut state = manager.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Serialize the final release with acquisition and eviction so an old
        // release cannot mark a newly acquired model idle or retire its replacement.
        let previous = self.entry.leases.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "model runtime lease count underflow");
        if previous != 1 {
            return;
        }
        if let Some(cached) = state.entries.get_mut(&self.key)
            && Arc::ptr_eq(&cached.entry, &self.entry)
        {
            cached.idle_since = Some(Instant::now());
        }
        let retired = state.retire_idle(manager.policy, Instant::now());
        state.wake_maintenance();
        drop(state);
        drop(retired);
    }
}

struct ActiveEmbeddingGuard<'a> {
    active: &'a AtomicUsize,
}

impl<'a> ActiveEmbeddingGuard<'a> {
    fn new(active: &'a AtomicUsize) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self { active }
    }
}

impl Drop for ActiveEmbeddingGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ModelRuntimeKey {
    fn new(reference: &str, options: &ModelConfig) -> Self {
        Self {
            reference: reference.to_owned(),
            api_key_fingerprint: options.api_key.as_deref().map(secret_fingerprint),
            endpoint: options.endpoint.clone(),
            model_cache_dir: options.cache_dir.clone(),
            device: options.device,
        }
    }
}

fn secret_fingerprint(secret: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    "zvec-grep-model-runtime-api-key".hash(&mut hasher);
    secret.hash(&mut hasher);
    hasher.finish()
}

fn validate_embedding_concurrency(concurrency: Option<usize>) -> Result<(), ModelError> {
    if concurrency == Some(0) {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            "Embedding concurrency must be greater than zero",
            None,
        ));
    }
    Ok(())
}

fn resolve_embedding_concurrency(
    requested: Option<usize>,
    defaults: EmbeddingConcurrencyDefaults,
) -> usize {
    requested.unwrap_or(defaults.initial).max(1)
}

fn manager_closed() -> ModelError {
    ModelError::new(
        crate::EngineError::RESOURCE_CLOSED,
        "embedding model runtime manager has been closed",
        None,
    )
}

#[cfg(test)]
mod tests;
