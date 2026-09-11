//! Process-level ownership and reuse of embedding model runtimes.

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    fmt,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::api::index::{
    options::Device,
    progress::{
        IndexEmbeddingProgress, IndexEmbeddingStage, IndexProgress, IndexProgressPhase,
        IndexProgressReporter,
    },
};

use super::{
    compute::ModelComputeRuntime,
    factory::create_embedding_model,
    spi::{
        CreateEmbeddingModelOptions, EmbeddingInput, EmbeddingInputKind, EmbeddingModel,
        EmbeddingModelInfo, EmbeddingOptions, EmbeddingResult, ModelError,
    },
};

type ModelFactory = dyn Fn(&str, CreateEmbeddingModelOptions) -> Result<Arc<dyn EmbeddingModel>, ModelError>
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
}

#[derive(Default)]
struct ManagerState {
    closed: bool,
    entries: HashMap<ModelRuntimeKey, Arc<ModelRuntimeEntry>>,
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
    options: CreateEmbeddingModelOptions,
    embedding_concurrency: Option<usize>,
}

impl ModelRuntimeRequest {
    pub(super) fn new_impl(
        reference: impl Into<String>,
        options: CreateEmbeddingModelOptions,
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
    device: Option<RuntimeDevice>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum RuntimeDevice {
    Auto,
    Cpu,
    Metal,
    Vulkan,
    Cuda,
}

impl From<Device> for RuntimeDevice {
    fn from(device: Device) -> Self {
        match device {
            Device::Auto => Self::Auto,
            Device::Cpu => Self::Cpu,
            Device::Metal => Self::Metal,
            Device::Vulkan => Self::Vulkan,
            Device::Cuda => Self::Cuda,
        }
    }
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
        Self::with_factory(|reference, options| create_embedding_model(reference, Some(options)))
    }

    fn with_factory(
        factory: impl Fn(
            &str,
            CreateEmbeddingModelOptions,
        ) -> Result<Arc<dyn EmbeddingModel>, ModelError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let compute_runtime = ModelComputeRuntime::shared();
        Self {
            inner: Arc::new(ManagerInner {
                factory: Arc::new(factory),
                compute_runtime,
                state: Mutex::new(ManagerState::default()),
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
            mut options,
            embedding_concurrency,
        } = request;
        validate_embedding_concurrency(embedding_concurrency)?;
        let key = ModelRuntimeKey::new(&reference, &options);
        let mut state = self.lock_state();
        if state.closed {
            return Err(manager_closed());
        }

        let entry = if let Some(entry) = state.entries.get(&key) {
            Arc::clone(entry)
        } else {
            // Model construction is intentionally performed while holding the
            // short-lived manager lock. Backends load heavy resources lazily,
            // so this guarantees a single instance without blocking on I/O.
            options.compute_runtime = Some(self.inner.compute_runtime.clone());
            let model = (self.inner.factory)(&reference, options)?;
            let entry = Arc::new(ModelRuntimeEntry {
                runtime: Arc::new(ModelRuntime {
                    model,
                    active_embeddings: AtomicUsize::new(0),
                }),
                leases: AtomicUsize::new(0),
            });
            state.entries.insert(key.clone(), Arc::clone(&entry));
            entry
        };
        let concurrency =
            resolve_embedding_concurrency(embedding_concurrency, entry.runtime.model.info());
        entry.leases.fetch_add(1, Ordering::AcqRel);
        Ok(ModelRuntimeLease {
            key,
            entry,
            manager: Arc::downgrade(&self.inner),
            operation: Arc::new(OperationConcurrency {
                limit: concurrency,
                permits: Arc::new(Semaphore::new(concurrency)),
            }),
        })
    }

    /// Stops new acquisitions and retires runtimes without active leases.
    pub(super) fn close_impl(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        state
            .entries
            .retain(|_, entry| entry.leases.load(Ordering::Acquire) > 0);
    }

    pub(super) fn snapshot_impl(&self) -> ModelRuntimeSnapshot {
        let state = self.lock_state();
        ModelRuntimeSnapshot {
            cached_runtimes: state.entries.len(),
            active_leases: state
                .entries
                .values()
                .map(|entry| entry.leases.load(Ordering::Acquire))
                .sum(),
            active_embeddings: state
                .entries
                .values()
                .map(|entry| entry.runtime.active_embeddings.load(Ordering::Acquire))
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
    pub(super) fn info_impl(&self) -> &EmbeddingModelInfo {
        self.entry.runtime.model.info()
    }

    pub(super) async fn embed_impl(
        &self,
        inputs: &[EmbeddingInput],
        mut options: EmbeddingOptions,
        index_progress: Option<IndexProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        let _permit = self
            .acquire_operation_permit(options.signal.as_ref())
            .await?;
        let _active = ActiveEmbeddingGuard::new(&self.entry.runtime.active_embeddings);
        options.execution_concurrency = self.operation.limit;
        if let Some(reporter) = index_progress {
            let model_progress = options.on_progress.take();
            let operation = Arc::clone(&self.operation);
            options.on_progress = Some(Arc::new(move |progress| {
                if let Some(model_progress) = &model_progress {
                    model_progress(progress.clone());
                }
                reporter.report(index_progress_from_model(&operation, progress));
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

fn index_progress_from_model(
    operation: &OperationConcurrency,
    progress: super::spi::EmbeddingModelProgress,
) -> IndexProgress {
    use super::spi::EmbeddingModelProgress;

    let (stage, model, downloaded_bytes, total_bytes, message) = match progress {
        EmbeddingModelProgress::Preparing { model } => {
            (IndexEmbeddingStage::Preparing, model, None, None, None)
        }
        EmbeddingModelProgress::Downloading {
            model,
            downloaded_bytes,
            total_bytes,
        } => (
            IndexEmbeddingStage::Downloading,
            model,
            downloaded_bytes,
            total_bytes,
            None,
        ),
        EmbeddingModelProgress::Warning { model, message } => (
            IndexEmbeddingStage::Warning,
            model,
            None,
            None,
            Some(message),
        ),
        EmbeddingModelProgress::Ready { model } => {
            (IndexEmbeddingStage::Ready, model, None, None, None)
        }
    };
    IndexProgress {
        phase: IndexProgressPhase::Indexing,
        files_total: None,
        files_indexed: None,
        files_failed: None,
        detail: Some(format!("downloading {model}")),
        embedding: Some(IndexEmbeddingProgress {
            concurrency: Some(operation.limit),
            max_concurrency: Some(operation.limit),
            retryable_failures: None,
            stage: Some(stage),
            model: Some(model),
            downloaded_bytes,
            total_bytes,
            message,
        }),
    }
}

impl Drop for ModelRuntimeLease {
    fn drop(&mut self) {
        let previous = self.entry.leases.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "model runtime lease count underflow");
        if previous != 1 {
            return;
        }
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let mut state = manager.state.lock().unwrap_or_else(PoisonError::into_inner);
        let should_remove = state.closed
            && state
                .entries
                .get(&self.key)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry));
        if should_remove {
            state.entries.remove(&self.key);
        }
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
    fn new(reference: &str, options: &CreateEmbeddingModelOptions) -> Self {
        Self {
            reference: reference.to_owned(),
            api_key_fingerprint: options.api_key.as_deref().map(secret_fingerprint),
            endpoint: options.endpoint.clone(),
            model_cache_dir: options.model_cache_dir.clone(),
            device: options.device.map(Into::into),
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

fn resolve_embedding_concurrency(requested: Option<usize>, info: &EmbeddingModelInfo) -> usize {
    requested
        .or(info.default_concurrency)
        .unwrap_or_else(|| {
            if info.provider == "qwen" {
                if info.input_kinds.contains(&EmbeddingInputKind::Image) {
                    4
                } else {
                    8
                }
            } else {
                1
            }
        })
        .max(1)
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

    use crate::models::spi::{EmbeddingInputKind, EmbeddingMetric};

    use super::*;
    use crate::models::spi::{EmbeddingModelLimits, EmbeddingPurpose};

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
                    reference: "local/fixture".to_owned(),
                    provider: "local".to_owned(),
                    name: "fixture".to_owned(),
                    dimension: 1,
                    metric: EmbeddingMetric::Cosine,
                    endpoint: None,
                    default_concurrency: Some(2),
                    input_kinds: vec![EmbeddingInputKind::Text],
                    limits: EmbeddingModelLimits {
                        max_batch_size: 8,
                        max_input_tokens: Some(32),
                        max_image_bytes: None,
                    },
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

        async fn embed(
            &self,
            inputs: &[EmbeddingInput],
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
            inputs: &[EmbeddingInput],
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
            inputs: &[EmbeddingInput],
            options: EmbeddingOptions,
        ) -> Result<EmbeddingResult, ModelError> {
            if let Some(on_progress) = options.on_progress {
                on_progress(super::super::spi::EmbeddingModelProgress::Preparing {
                    model: self.info.reference.clone(),
                });
                on_progress(super::super::spi::EmbeddingModelProgress::Downloading {
                    model: self.info.reference.clone(),
                    downloaded_bytes: Some(4),
                    total_bytes: Some(8),
                });
                on_progress(super::super::spi::EmbeddingModelProgress::Warning {
                    model: self.info.reference.clone(),
                    message: "fixture warning".to_owned(),
                });
                on_progress(super::super::spi::EmbeddingModelProgress::Ready {
                    model: self.info.reference.clone(),
                });
            }
            Ok(EmbeddingResult {
                vectors: inputs.iter().map(|_| vec![1.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn reuses_one_runtime_and_allows_shared_concurrent_embeddings() {
        let creations = Arc::new(AtomicUsize::new(0));
        let fixture = Arc::new(ConcurrentFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory({
            let creations = Arc::clone(&creations);
            let fixture = Arc::clone(&fixture);
            move |_reference, _options| {
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
                    CreateEmbeddingModelOptions::default(),
                    None,
                ))
            });
            let second = scope.spawn(move || {
                second_manager.acquire(ModelRuntimeRequest::new(
                    "local/fixture",
                    CreateEmbeddingModelOptions::default(),
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
        assert_eq!(first.info().reference, "local/fixture");
        assert_eq!(
            manager.snapshot(),
            ModelRuntimeSnapshot {
                cached_runtimes: 1,
                active_leases: 2,
                active_embeddings: 0,
            }
        );

        let first_contents = vec![EmbeddingInput::text("first".to_owned())];
        let second_contents = vec![EmbeddingInput::text("second".to_owned())];
        let first_embedding = first.embed(
            &first_contents,
            EmbeddingOptions {
                purpose: Some(EmbeddingPurpose::Query),
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
                CreateEmbeddingModelOptions::default(),
                None,
            ))
            .err()
            .expect("closed manager should reject acquisition");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_CLOSED);
    }

    #[tokio::test]
    async fn maps_private_model_progress_to_public_index_progress() {
        let fixture = Arc::new(ProgressFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory(move |_reference, _options| {
            Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                CreateEmbeddingModelOptions::default(),
                Some(1),
            ))
            .expect("fixture runtime should be acquired");
        let model_events = Arc::new(StdMutex::new(Vec::new()));
        let captured_model_events = Arc::clone(&model_events);
        let index_events = Arc::new(StdMutex::new(Vec::new()));
        let captured_index_events = Arc::clone(&index_events);
        let inputs = [EmbeddingInput::text("fixture".to_owned())];

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
                Some(IndexProgressReporter::new(move |progress| {
                    captured_index_events
                        .lock()
                        .expect("index event lock should not be poisoned")
                        .push(progress);
                })),
            )
            .await
            .expect("fixture embedding should complete");

        assert_eq!(
            model_events
                .lock()
                .expect("model event lock should not be poisoned")
                .len(),
            4
        );
        let events = index_events
            .lock()
            .expect("index event lock should not be poisoned");
        assert_eq!(events.len(), 4);
        assert_eq!(
            events
                .iter()
                .map(|event| {
                    event
                        .embedding
                        .as_ref()
                        .and_then(|embedding| embedding.stage)
                })
                .collect::<Vec<_>>(),
            [
                Some(IndexEmbeddingStage::Preparing),
                Some(IndexEmbeddingStage::Downloading),
                Some(IndexEmbeddingStage::Warning),
                Some(IndexEmbeddingStage::Ready),
            ]
        );
        for event in events.iter() {
            assert_eq!(event.phase, IndexProgressPhase::Indexing);
            assert_eq!(event.detail.as_deref(), Some("downloading local/fixture"));
            let embedding = event
                .embedding
                .as_ref()
                .expect("model progress should be nested under embedding");
            assert_eq!(embedding.concurrency, Some(1));
            assert_eq!(embedding.max_concurrency, Some(1));
            assert_eq!(embedding.model.as_deref(), Some("local/fixture"));
        }
        let downloading = events[1]
            .embedding
            .as_ref()
            .expect("download event should include embedding progress");
        assert_eq!(downloading.downloaded_bytes, Some(4));
        assert_eq!(downloading.total_bytes, Some(8));
        assert_eq!(
            events[2]
                .embedding
                .as_ref()
                .and_then(|embedding| embedding.message.as_deref()),
            Some("fixture warning")
        );
    }

    #[tokio::test]
    async fn user_concurrency_limits_tasks_without_splitting_the_shared_runtime() {
        let fixture = Arc::new(GatedFixtureModel::new());
        let manager = ModelRuntimeManager::with_factory({
            let fixture = Arc::clone(&fixture);
            move |_reference, _options| Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                CreateEmbeddingModelOptions::default(),
                Some(2),
            ))
            .expect("fixture runtime should be acquired");
        let inputs = [EmbeddingInput::text("fixture".to_owned())];

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
            move |_reference, _options| Ok(Arc::clone(&fixture) as Arc<dyn EmbeddingModel>)
        });
        let lease = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                CreateEmbeddingModelOptions::default(),
                Some(1),
            ))
            .expect("fixture runtime should be acquired");
        let inputs = [EmbeddingInput::text("fixture".to_owned())];
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

    #[test]
    fn concurrency_policy_prefers_user_and_matches_main_remote_defaults() {
        let mut info = ConcurrentFixtureModel::new().info;
        assert_eq!(resolve_embedding_concurrency(Some(4), &info), 4);
        assert_eq!(resolve_embedding_concurrency(None, &info), 2);
        info.default_concurrency = None;
        assert_eq!(resolve_embedding_concurrency(None, &info), 1);
        assert_eq!(resolve_embedding_concurrency(Some(24), &info), 24);

        info.provider = "qwen".to_owned();
        assert_eq!(resolve_embedding_concurrency(None, &info), 8);
        info.input_kinds.push(EmbeddingInputKind::Image);
        assert_eq!(resolve_embedding_concurrency(None, &info), 4);
    }

    #[test]
    fn rejects_zero_user_concurrency_before_constructing_a_runtime() {
        let creations = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&creations);
        let manager = ModelRuntimeManager::with_factory(move |_reference, _options| {
            captured.fetch_add(1, Ordering::AcqRel);
            Ok(Arc::new(ProgressFixtureModel::new()) as Arc<dyn EmbeddingModel>)
        });

        let error = manager
            .acquire(ModelRuntimeRequest::new(
                "local/fixture",
                CreateEmbeddingModelOptions::default(),
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
            &CreateEmbeddingModelOptions {
                api_key: Some("secret-a".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                model_cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
                compute_runtime: None,
            },
        );
        let same = ModelRuntimeKey::new(
            "local/fixture",
            &CreateEmbeddingModelOptions {
                api_key: Some("secret-a".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                model_cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
                compute_runtime: None,
            },
        );
        let different = ModelRuntimeKey::new(
            "local/fixture",
            &CreateEmbeddingModelOptions {
                api_key: Some("secret-b".to_owned()),
                endpoint: Some("https://example.test/a".to_owned()),
                model_cache_dir: Some(PathBuf::from("/cache/a")),
                device: Some(Device::Cpu),
                compute_runtime: None,
            },
        );

        assert!(base == same);
        assert!(base != different);
        assert_eq!(base.api_key_fingerprint, same.api_key_fingerprint);
        assert_ne!(base.api_key_fingerprint, different.api_key_fingerprint);
    }
}
