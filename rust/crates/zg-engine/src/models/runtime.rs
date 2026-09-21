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

use super::{
    ModelProgressReporter,
    compute::ModelComputeRuntime,
    factory::create_embedding_model,
    spi::{EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions, ModelError},
};
use crate::domain::{
    Content,
    model::{Device, EmbeddingModelInfo, EmbeddingResult, ModelConfig},
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
    pub(super) fn new_impl(
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
    pub(super) fn new_impl() -> Self {
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
    pub(super) fn acquire_impl(
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
    pub(super) fn close_impl(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        // Disconnecting the channel stops maintenance without retaining the manager
        // or waiting for model destructors while holding its state lock.
        state.maintenance.take();
        let retired = state.retire_idle(self.inner.policy, Instant::now());
        drop(state);
        drop(retired);
    }

    pub(super) fn snapshot_impl(&self) -> ModelRuntimeSnapshot {
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
    pub(super) fn matches_request_impl(&self, request: &ModelRuntimeRequest) -> bool {
        self.key == ModelRuntimeKey::new(&request.reference, &request.options)
    }

    pub(super) fn concurrency_defaults_impl(&self) -> EmbeddingConcurrencyDefaults {
        self.entry.runtime.model.concurrency_defaults()
    }

    pub(super) fn info_impl(&self) -> &EmbeddingModelInfo {
        self.entry.runtime.model.info()
    }

    pub(super) async fn embed_impl(
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
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use tokio::sync::{Barrier, Semaphore as TokioSemaphore};

    use crate::domain::model::Metric;

    use super::*;
    use crate::domain::model::EmbeddingPurpose;

    struct ConcurrentFixtureModel {
        info: EmbeddingModelInfo,
        barrier: Barrier,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    impl ConcurrentFixtureModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".to_owned(),
                        name: "fixture".to_owned(),
                        endpoint: None,
                    },
                    dimension: 1,
                    metric: Metric::Cosine,
                    max_batch_size: 8,
                    max_input_tokens: Some(32),
                    max_image_bytes: None,
                },
                barrier: Barrier::new(2),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl EmbeddingModel for ConcurrentFixtureModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
            EmbeddingConcurrencyDefaults {
                initial: 2,
                maximum: 2,
            }
        }

        async fn embed(
            &self,
            inputs: &[Vec<Content>],
            _options: EmbeddingOptions,
        ) -> Result<EmbeddingResult, ModelError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active.fetch_max(active, Ordering::AcqRel);
            self.barrier.wait().await;
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(EmbeddingResult {
                vectors: inputs.iter().map(|_| vec![1.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    struct ProgressFixtureModel {
        info: EmbeddingModelInfo,
    }

    struct GatedFixtureModel {
        info: EmbeddingModelInfo,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        started: AtomicUsize,
        release: TokioSemaphore,
    }

    impl GatedFixtureModel {
        fn new() -> Self {
            let model = ConcurrentFixtureModel::new();
            Self {
                info: model.info,
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                release: TokioSemaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl EmbeddingModel for GatedFixtureModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        async fn embed(
            &self,
            inputs: &[Vec<Content>],
            _options: EmbeddingOptions,
        ) -> Result<EmbeddingResult, ModelError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active.fetch_max(active, Ordering::AcqRel);
            self.started.fetch_add(1, Ordering::AcqRel);
            let permit = self
                .release
                .acquire()
                .await
                .map_err(|error| ModelError::internal(error.to_string()))?;
            permit.forget();
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(EmbeddingResult {
                vectors: inputs.iter().map(|_| vec![1.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    impl ProgressFixtureModel {
        fn new() -> Self {
            let model = ConcurrentFixtureModel::new();
            Self { info: model.info }
        }
    }

    #[async_trait]
    impl EmbeddingModel for ProgressFixtureModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        async fn embed(
            &self,
            inputs: &[Vec<Content>],
            options: EmbeddingOptions,
        ) -> Result<EmbeddingResult, ModelError> {
            if let Some(on_progress) = options.on_progress {
                on_progress(crate::domain::model::ModelProgress::Preparing {
                    model: self.info.model.reference(),
                });
                on_progress(crate::domain::model::ModelProgress::Downloading {
                    model: self.info.model.reference(),
                    downloaded_bytes: Some(4),
                    total_bytes: Some(8),
                });
                on_progress(crate::domain::model::ModelProgress::Warning {
                    model: self.info.model.reference(),
                    message: "fixture warning".to_owned(),
                });
                on_progress(crate::domain::model::ModelProgress::Ready {
                    model: self.info.model.reference(),
                });
            }
            Ok(EmbeddingResult {
                vectors: inputs.iter().map(|_| vec![1.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    fn fixture_cache(policy: CachePolicy) -> (ModelRuntimeManager, Arc<AtomicUsize>) {
        let creations = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&creations);
        let manager = ModelRuntimeManager::with_policy(
            Arc::new(move |_, _, _| {
                count.fetch_add(1, Ordering::AcqRel);
                Ok(Arc::new(ProgressFixtureModel::new()))
            }),
            policy,
        );
        (manager, creations)
    }

    fn acquire_fixture(manager: &ModelRuntimeManager, reference: &str) -> ModelRuntimeLease {
        manager
            .acquire(ModelRuntimeRequest::new(
                reference,
                ModelConfig::default(),
                None,
            ))
            .expect("fixture lease")
    }

    fn reap_at(manager: &ModelRuntimeManager, now: Instant) {
        let retired = manager.lock_state().retire_idle(manager.inner.policy, now);
        drop(retired);
    }

    fn idle_since(manager: &ModelRuntimeManager, reference: &str) -> Instant {
        let key = ModelRuntimeKey::new(reference, &ModelConfig::default());
        manager.lock_state().entries[&key]
            .idle_since
            .expect("idle model")
    }

    #[test]
    fn idle_ttl_starts_at_final_release_and_reacquisition_resets_it() {
        let (manager, creations) = fixture_cache(CachePolicy::default());
        let first = acquire_fixture(&manager, "first");
        let model = Arc::downgrade(&first.entry.runtime.model);
        let second = acquire_fixture(&manager, "first");
        let ttl = manager.inner.policy.idle_timeout;
        reap_at(&manager, Instant::now() + ttl);
        drop(first);
        reap_at(&manager, Instant::now() + ttl);
        assert!(
            model.upgrade().is_some(),
            "a remaining lease prevents eviction"
        );
        drop(second);
        let first_idle = idle_since(&manager, "first");
        reap_at(
            &manager,
            first_idle + ttl.saturating_sub(Duration::from_nanos(1)),
        );
        assert!(model.upgrade().is_some());
        let reacquired = acquire_fixture(&manager, "first");
        assert_eq!(creations.load(Ordering::Acquire), 1);
        reap_at(&manager, first_idle + ttl);
        assert!(
            model.upgrade().is_some(),
            "the previous deadline cannot evict a new lease"
        );
        drop(reacquired);
        let last_idle = idle_since(&manager, "first");
        reap_at(
            &manager,
            last_idle + ttl.saturating_sub(Duration::from_nanos(1)),
        );
        assert!(model.upgrade().is_some());
        reap_at(&manager, last_idle + ttl);
        assert!(model.upgrade().is_none());
        let replacement = acquire_fixture(&manager, "first");
        assert_eq!(creations.load(Ordering::Acquire), 2);
        drop(replacement);
        manager.close();
    }

    #[test]
    fn capacity_is_soft_for_active_models_and_trims_on_acquisition_and_release() {
        let (manager, creations) = fixture_cache(CachePolicy::default());
        let first = acquire_fixture(&manager, "first");
        let first_model = Arc::downgrade(&first.entry.runtime.model);
        let second = acquire_fixture(&manager, "second");
        let second_model = Arc::downgrade(&second.entry.runtime.model);
        assert_eq!(manager.snapshot().cached_runtimes, 2);
        assert_eq!(manager.snapshot().active_leases, 2);
        drop(first);
        assert!(first_model.upgrade().is_none());
        assert!(second_model.upgrade().is_some());
        assert_eq!(manager.snapshot().cached_runtimes, 1);
        drop(second);
        let hit = acquire_fixture(&manager, "second");
        assert_eq!(creations.load(Ordering::Acquire), 2);
        drop(hit);
        let third = acquire_fixture(&manager, "third");
        assert!(
            second_model.upgrade().is_none(),
            "a new model evicts the idle cached model"
        );
        assert_eq!(manager.snapshot().cached_runtimes, 1);
        drop(third);
        manager.close();
    }

    #[test]
    fn capacity_retires_the_least_recently_used_idle_model() {
        let (manager, _) = fixture_cache(CachePolicy {
            capacity: 2,
            ..CachePolicy::default()
        });
        let first = acquire_fixture(&manager, "first");
        let first_model = Arc::downgrade(&first.entry.runtime.model);
        drop(first);
        let second = acquire_fixture(&manager, "second");
        let second_model = Arc::downgrade(&second.entry.runtime.model);
        drop(second);
        let hit = acquire_fixture(&manager, "first");
        drop(hit);
        let third = acquire_fixture(&manager, "third");
        assert!(first_model.upgrade().is_some());
        assert!(second_model.upgrade().is_none());
        assert_eq!(manager.snapshot().cached_runtimes, 2);
        drop(third);
        manager.close();
    }

    #[test]
    fn shared_lease_owners_remain_protected_until_the_last_owner_releases() {
        for policy in [
            CachePolicy {
                idle_timeout: Duration::ZERO,
                ..CachePolicy::default()
            },
            CachePolicy {
                capacity: 0,
                ..CachePolicy::default()
            },
        ] {
            let (manager, _) = fixture_cache(policy);
            // Writer sessions share their owning Arc with borrowed queries.
            let writer = Arc::new(acquire_fixture(&manager, "shared"));
            let borrower = Arc::clone(&writer);
            let model = Arc::downgrade(&writer.entry.runtime.model);
            drop(writer);
            reap_at(&manager, Instant::now() + Duration::from_secs(3600));
            assert!(model.upgrade().is_some());
            manager.close();
            assert!(model.upgrade().is_some());
            drop(borrower);
            assert!(model.upgrade().is_none());
            assert_eq!(manager.snapshot(), ModelRuntimeSnapshot::default());
        }
    }

    struct DisposalFixture {
        inner: ProgressFixtureModel,
        on_drop: Box<dyn Fn() + Send + Sync>,
    }

    #[async_trait]
    impl EmbeddingModel for DisposalFixture {
        fn info(&self) -> &EmbeddingModelInfo {
            self.inner.info()
        }

        async fn embed(
            &self,
            inputs: &[Vec<Content>],
            options: EmbeddingOptions,
        ) -> Result<EmbeddingResult, ModelError> {
            self.inner.embed(inputs, options).await
        }
    }

    impl Drop for DisposalFixture {
        fn drop(&mut self) {
            (self.on_drop)();
        }
    }

    #[test]
    fn maintenance_expires_idle_models_without_another_request_or_tokio_runtime() {
        let (disposed, observed) = mpsc::channel();
        let manager = ModelRuntimeManager::with_policy(
            Arc::new(move |_, _, _| {
                let disposed = disposed.clone();
                Ok(Arc::new(DisposalFixture {
                    inner: ProgressFixtureModel::new(),
                    on_drop: Box::new(move || {
                        let _ = disposed.send(());
                    }),
                }))
            }),
            CachePolicy {
                idle_timeout: Duration::from_millis(20),
                ..CachePolicy::default()
            },
        );
        let lease = acquire_fixture(&manager, "first");
        let model = Arc::downgrade(&lease.entry.runtime.model);
        assert!(
            observed.recv_timeout(Duration::from_millis(50)).is_err(),
            "active model survives its TTL"
        );
        drop(lease);
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("background expiration");
        assert!(model.upgrade().is_none());
        assert_eq!(manager.snapshot(), ModelRuntimeSnapshot::default());
        manager.close();
    }

    #[test]
    fn dropping_the_manager_releases_idle_models_without_waiting_for_the_ttl() {
        let (disposed, observed) = mpsc::channel();
        let manager = ModelRuntimeManager::with_factory(move |_, _, _| {
            let disposed = disposed.clone();
            Ok(Arc::new(DisposalFixture {
                inner: ProgressFixtureModel::new(),
                on_drop: Box::new(move || {
                    let _ = disposed.send(());
                }),
            }))
        });
        drop(acquire_fixture(&manager, "first"));
        let weak = Arc::downgrade(&manager.inner);
        drop(manager);
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("manager drop releases cached model");
        assert!(
            weak.upgrade().is_none(),
            "maintenance must not retain its owner"
        );
    }

    #[test]
    fn slow_model_disposal_does_not_hold_the_cache_lock() {
        let (entered, observed) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let gate = Arc::new(Mutex::new(wait_for_release));
        let manager = ModelRuntimeManager::with_factory(move |reference, _, _| {
            let entered = entered.clone();
            let gate = Arc::clone(&gate);
            let blocks = reference == "first";
            Ok(Arc::new(DisposalFixture {
                inner: ProgressFixtureModel::new(),
                on_drop: Box::new(move || {
                    if blocks {
                        let _ = entered.send(());
                        let _ = gate
                            .lock()
                            .expect("gate")
                            .recv_timeout(Duration::from_secs(5));
                    }
                }),
            }))
        });
        drop(acquire_fixture(&manager, "first"));
        let evicting = manager.clone();
        let eviction = std::thread::spawn(move || acquire_fixture(&evicting, "second"));
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("disposal starts");
        let (acquired, hit) = mpsc::channel();
        let acquiring = manager.clone();
        let acquisition = std::thread::spawn(move || {
            let lease = acquire_fixture(&acquiring, "second");
            acquired.send(lease).expect("send hit");
        });
        let result = hit.recv_timeout(Duration::from_secs(2));
        release.send(()).expect("unblock disposal");
        acquisition.join().expect("acquisition thread");
        let first = eviction.join().expect("eviction thread");
        let second = result.expect("cache hit must not wait for disposal");
        assert!(Arc::ptr_eq(&first.entry, &second.entry));
        drop((first, second));
        manager.close();
    }

    #[test]
    fn concurrent_final_releases_cannot_mark_new_leases_idle() {
        let (manager, _) = fixture_cache(CachePolicy::default());
        for _ in 0..64 {
            let previous = acquire_fixture(&manager, "first");
            let barrier = std::sync::Barrier::new(2);
            let replacement = std::thread::scope(|scope| {
                let acquisition = scope.spawn(|| {
                    barrier.wait();
                    acquire_fixture(&manager, "first")
                });
                barrier.wait();
                drop(previous);
                acquisition.join().expect("acquisition")
            });
            reap_at(&manager, Instant::now() + Duration::from_secs(3600));
            let shared = acquire_fixture(&manager, "first");
            assert!(Arc::ptr_eq(&replacement.entry, &shared.entry));
            assert_eq!(manager.snapshot().active_leases, 2);
            drop((replacement, shared));
        }
        manager.close();
    }

    #[test]
    fn invalid_model_info_is_rejected_before_caching_or_leasing() {
        for field in [
            "provider",
            "name",
            "dimension",
            "max_batch_size",
            "max_input_tokens",
            "max_image_bytes",
        ] {
            let creations = Arc::new(AtomicUsize::new(0));
            let observed_creations = Arc::clone(&creations);
            let manager = ModelRuntimeManager::with_factory(move |_, _, _| {
                let mut model = ConcurrentFixtureModel::new();
                if observed_creations.fetch_add(1, Ordering::AcqRel) == 0 {
                    match field {
                        "provider" => model.info.model.provider = " ".to_owned(),
                        "name" => model.info.model.name.clear(),
                        "dimension" => model.info.dimension = 0,
                        "max_batch_size" => model.info.max_batch_size = 0,
                        "max_input_tokens" => model.info.max_input_tokens = Some(0),
                        "max_image_bytes" => model.info.max_image_bytes = Some(0),
                        _ => unreachable!(),
                    }
                } else {
                    // Optional limits may be absent; this does not invalidate the model.
                    model.info.max_input_tokens = None;
                    model.info.max_image_bytes = None;
                }
                Ok(Arc::new(model))
            });
            let request =
                || ModelRuntimeRequest::new("local/fixture", ModelConfig::default(), None);
            let error = manager.acquire(request()).err().expect("invalid metadata");
            assert_eq!(error.code(), crate::EngineError::INTERNAL);
            assert!(error.cause().expect("validation detail").contains(field));
            assert_eq!(manager.snapshot(), ModelRuntimeSnapshot::default());
            let lease = manager
                .acquire(request())
                .expect("retry constructs a valid model");
            assert_eq!(creations.load(Ordering::Acquire), 2);
            assert_eq!(manager.snapshot().active_leases, 1);
            assert_eq!(lease.info().model.reference(), "local/fixture");
        }
    }

    #[tokio::test]
    async fn reuses_one_runtime_and_allows_shared_concurrent_embeddings() {
        let creations = Arc::new(AtomicUsize::new(0));
        let fixture = Arc::new(ConcurrentFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory({
            let creations = Arc::clone(&creations);
            let fixture = Arc::clone(&fixture);
            move |_reference, _options, _compute| {
                creations.fetch_add(1, Ordering::AcqRel);
                Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
            }
        });

        let first_manager = manager.clone();
        let second_manager = manager.clone();
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(move || {
                first_manager.acquire(ModelRuntimeRequest::new(
                    "local/fixture",
                    ModelConfig::default(),
                    None,
                ))
            });
            let second = scope.spawn(move || {
                second_manager.acquire(ModelRuntimeRequest::new(
                    "local/fixture",
                    ModelConfig::default(),
                    None,
                ))
            });
            (
                first.join().expect("first acquisition should not panic"),
                second.join().expect("second acquisition should not panic"),
            )
        });
        let first = first.expect("first runtime should be acquired");
        let second = second.expect("second runtime should be acquired");

        assert_eq!(creations.load(Ordering::Acquire), 1);
        assert!(Arc::ptr_eq(&first.entry.runtime, &second.entry.runtime));
        assert_eq!(first.info().model.reference(), "local/fixture");
        assert_eq!(
            manager.snapshot(),
            ModelRuntimeSnapshot {
                cached_runtimes: 1,
                active_leases: 2,
                active_embeddings: 0,
            }
        );

        let first_contents = vec![vec![Content::Text("first".to_owned())]];
        let second_contents = vec![vec![Content::Text("second".to_owned())]];
        let first_embedding = first.embed(
            &first_contents,
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                ..EmbeddingOptions::default()
            },
            None,
        );
        let second_embedding = second.embed(&second_contents, EmbeddingOptions::default(), None);
        let (first_result, second_result) = tokio::join!(first_embedding, second_embedding);
        assert_eq!(
            first_result
                .expect("first embedding should complete")
                .vectors,
            [[1.0]]
        );
        assert_eq!(
            second_result
                .expect("second embedding should complete")
                .vectors,
            [[1.0]]
        );
        assert_eq!(fixture.maximum_active.load(Ordering::Acquire), 2);
        assert_eq!(manager.snapshot().active_embeddings, 0);

        drop(first);
        assert_eq!(manager.snapshot().active_leases, 1);
        assert_eq!(manager.snapshot().cached_runtimes, 1);

        manager.close();
        assert_eq!(manager.snapshot().active_leases, 1);
        assert_eq!(manager.snapshot().cached_runtimes, 1);
        drop(second);
        assert_eq!(manager.snapshot().active_leases, 0);
        assert_eq!(manager.snapshot().cached_runtimes, 0);
        let error = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                ModelConfig::default(),
                None,
            ))
            .err()
            .expect("closed manager should reject acquisition");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_CLOSED);
    }

    #[tokio::test]
    async fn forwards_model_progress_to_both_callbacks_with_effective_concurrency() {
        use crate::domain::model::ModelProgress;

        let fixture = Arc::new(ProgressFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory(move |_reference, _options, _compute| {
            Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                ModelConfig::default(),
                Some(1),
            ))
            .expect("fixture runtime should be acquired");
        let model_events = Arc::new(StdMutex::new(Vec::new()));
        let captured_model_events = Arc::clone(&model_events);
        let reported_events = Arc::new(StdMutex::new(Vec::new()));
        let captured_reported_events = Arc::clone(&reported_events);
        let inputs = [vec![Content::Text("fixture".to_owned())]];

        lease
            .embed(
                &inputs,
                EmbeddingOptions {
                    on_progress: Some(Arc::new(move |progress| {
                        captured_model_events
                            .lock()
                            .expect("model event lock should not be poisoned")
                            .push(progress);
                    })),
                    ..EmbeddingOptions::default()
                },
                Some(ModelProgressReporter::new(move |progress, concurrency| {
                    captured_reported_events
                        .lock()
                        .expect("reported event lock should not be poisoned")
                        .push((progress, concurrency));
                })),
            )
            .await
            .expect("fixture embedding should complete");

        let model_events = model_events
            .lock()
            .expect("model event lock should not be poisoned");
        assert_eq!(
            *model_events,
            [
                ModelProgress::Preparing {
                    model: "local/fixture".to_owned(),
                },
                ModelProgress::Downloading {
                    model: "local/fixture".to_owned(),
                    downloaded_bytes: Some(4),
                    total_bytes: Some(8),
                },
                ModelProgress::Warning {
                    model: "local/fixture".to_owned(),
                    message: "fixture warning".to_owned(),
                },
                ModelProgress::Ready {
                    model: "local/fixture".to_owned(),
                },
            ]
        );
        let reported_events = reported_events
            .lock()
            .expect("reported event lock should not be poisoned");
        assert_eq!(
            *reported_events,
            model_events
                .iter()
                .cloned()
                .map(|event| (event, 1))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn user_concurrency_limits_tasks_without_splitting_the_shared_runtime() {
        let fixture = Arc::new(GatedFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory({
            let fixture = Arc::clone(&fixture);
            move |_reference, _options, _compute| Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                ModelConfig::default(),
                Some(2),
            ))
            .expect("fixture runtime should be acquired");
        let inputs = [vec![Content::Text("fixture".to_owned())]];

        let embeddings = async {
            futures_util::future::join_all(
                (0..4).map(|_| lease.embed(&inputs, EmbeddingOptions::default(), None)),
            )
            .await
        };
        let observe_limit = async {
            while fixture.started.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(fixture.started.load(Ordering::Acquire), 2);
            assert_eq!(fixture.maximum_active.load(Ordering::Acquire), 2);
            let pressure = acquire_fixture(&manager, "other-model");
            reap_at(&manager, Instant::now() + Duration::from_secs(3600));
            assert_eq!(manager.snapshot().cached_runtimes, 2);
            assert_eq!(manager.snapshot().active_embeddings, 2);
            drop(pressure);
            assert_eq!(manager.snapshot().cached_runtimes, 1);
            fixture.release.add_permits(4);
        };
        let (results, ()) = tokio::join!(embeddings, observe_limit);

        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(fixture.started.load(Ordering::Acquire), 4);
        assert_eq!(fixture.maximum_active.load(Ordering::Acquire), 2);
        assert_eq!(manager.snapshot().cached_runtimes, 1);
    }

    #[tokio::test]
    async fn cancelled_task_does_not_wait_for_an_operation_permit() {
        let fixture = Arc::new(GatedFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory({
            let fixture = Arc::clone(&fixture);
            move |_reference, _options, _compute| Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                ModelConfig::default(),
                Some(1),
            ))
            .expect("fixture runtime should be acquired");
        let inputs = [vec![Content::Text("fixture".to_owned())]];
        let signal = CancellationToken::new();

        let first = lease.embed(&inputs, EmbeddingOptions::default(), None);
        let cancelled = async {
            while fixture.started.load(Ordering::Acquire) < 1 {
                tokio::task::yield_now().await;
            }
            signal.cancel();
            let result = lease
                .embed(
                    &inputs,
                    EmbeddingOptions {
                        signal: Some(signal),
                        ..EmbeddingOptions::default()
                    },
                    None,
                )
                .await;
            fixture.release.add_permits(1);
            result
        };
        let (first_result, cancelled_result) = tokio::join!(first, cancelled);

        assert!(first_result.is_ok());
        let error = cancelled_result.expect_err("queued embedding should be cancelled");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(fixture.started.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn dropping_an_embedding_future_releases_activity_but_keeps_its_model_lease() {
        let fixture = Arc::new(GatedFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory(move |_, _, _| {
            Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = acquire_fixture(&manager, "first");
        let inputs = [vec![Content::Text("fixture".into())]];
        let mut embedding = Box::pin(lease.embed(&inputs, EmbeddingOptions::default(), None));
        tokio::select! {
            biased;
            _ = &mut embedding => panic!("embedding is gated"),
            () = std::future::ready(()) => {},
        }
        let pressure = acquire_fixture(&manager, "second");
        assert_eq!(manager.snapshot().active_embeddings, 1);
        drop(embedding);
        assert_eq!(manager.snapshot().active_embeddings, 0);
        reap_at(&manager, Instant::now() + Duration::from_secs(3600));
        assert_eq!(manager.snapshot().cached_runtimes, 2);
        drop(lease);
        assert_eq!(manager.snapshot().cached_runtimes, 1);
        drop(pressure);
        manager.close();
    }

    #[test]
    fn concurrency_policy_prefers_user_and_preserves_catalog_backend_defaults() {
        let manager = ModelRuntimeManager::new();
        for (reference, initial, maximum) in [
            ("local/potion-code-16m-v2", 2, 2),
            ("local/embeddinggemma-300m", 1, 1),
            ("local/all-minilm-l6-v2", 1, 1),
            ("qwen/text-embedding-v4", 8, 12),
            ("qwen/qwen3-vl-embedding", 4, 8),
        ] {
            let config = ModelConfig {
                api_key: Some("fixture".into()),
                ..ModelConfig::default()
            };
            let lease = manager
                .acquire(ModelRuntimeRequest::new(reference, config.clone(), None))
                .expect("catalog model should be created without loading resources");
            assert_eq!(lease.operation.limit, initial, "{reference}");
            assert_eq!(lease.concurrency_defaults().maximum, maximum, "{reference}");
            let explicit = manager
                .acquire(ModelRuntimeRequest::new(reference, config, Some(24)))
                .expect("explicit concurrency");
            assert_eq!(explicit.operation.limit, 24, "{reference}");
            assert!(Arc::ptr_eq(&lease.entry, &explicit.entry));
        }
    }

    #[test]
    fn rejects_zero_user_concurrency_before_constructing_a_runtime() {
        let creations = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&creations);
        let manager = ModelRuntimeManager::with_factory(move |_reference, _options, _compute| {
            captured.fetch_add(1, Ordering::AcqRel);
            Ok(Arc::new(ProgressFixtureModel::new()) as Arc<dyn EmbeddingModel>)
        });

        let error = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                ModelConfig::default(),
                Some(0),
            ))
            .err()
            .expect("zero concurrency should be rejected");

        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert_eq!(creations.load(Ordering::Acquire), 0);
    }

    #[test]
    fn runtime_key_separates_resource_affecting_options_without_storing_api_keys() {
        let base = ModelRuntimeKey::new(
            "local/fixture",
            &ModelConfig {
                api_key: Some("secret-a".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
            },
        );
        let same = ModelRuntimeKey::new(
            "local/fixture",
            &ModelConfig {
                api_key: Some("secret-a".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
            },
        );
        let different = ModelRuntimeKey::new(
            "local/fixture",
            &ModelConfig {
                api_key: Some("secret-b".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
            },
        );

        assert!(base == same);
        assert!(base != different);
        assert_eq!(base.api_key_fingerprint, same.api_key_fingerprint);
        assert_ne!(base.api_key_fingerprint, different.api_key_fingerprint);
    }
}
