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

    async fn prepare(&self, options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
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
        Ok(())
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        _options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
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
        let request = || ModelRuntimeRequest::new("local/fixture", ModelConfig::default(), None);
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
async fn forwards_preparation_progress_to_both_callbacks_with_effective_concurrency() {
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
    lease
        .prepare(
            EmbeddingPrepareOptions {
                on_progress: Some(Arc::new(move |progress| {
                    captured_model_events
                        .lock()
                        .expect("model event lock should not be poisoned")
                        .push(progress);
                })),
                ..EmbeddingPrepareOptions::default()
            },
            Some(ModelProgressReporter::new(move |progress, concurrency| {
                captured_reported_events
                    .lock()
                    .expect("reported event lock should not be poisoned")
                    .push((progress, concurrency));
            })),
        )
        .await
        .expect("fixture preparation should complete");

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
