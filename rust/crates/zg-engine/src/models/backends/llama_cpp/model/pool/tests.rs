use super::super::tests::test_resources;
use super::super::{LlamaToken, LlamaWorkerMessage, should_fallback_to_cpu};
use super::*;
use std::sync::{atomic::AtomicBool, mpsc};

#[test]
fn overlapping_operations_share_the_fixed_capacity_instead_of_adding_budgets() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let first = admission.register(4, None);
    let second = admission.register(4, None);
    for id in [first, second, first, second] {
        assert!(admission.try_acquire(id));
    }
    assert_eq!(admission.in_flight, 4);
    assert!(!admission.try_acquire(first));
    assert!(!admission.try_acquire(second));
    admission.release(first);
    assert!(admission.try_acquire(second));
    assert_eq!(admission.in_flight, 4);
}

#[test]
fn an_operation_cannot_borrow_another_operations_larger_limit() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let small = admission.register(1, None);
    let large = admission.register(4, None);
    assert!(admission.try_acquire(small));
    assert!(!admission.try_acquire(small));
    for _ in 0..3 {
        assert!(admission.try_acquire(large));
    }
    assert!(!admission.try_acquire(large));
    admission.release(small);
    admission.unregister(small);
    assert!(admission.try_acquire(large));
}

#[test]
fn releasing_an_operation_preserves_server_capacity() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let large = admission.register(4, None);
    let small = admission.register(1, None);
    assert!(admission.try_acquire(small));
    assert!(admission.try_acquire(large));
    admission.release(large);
    admission.unregister(large);
    assert_eq!(admission.limit(), 4);
    assert!(!admission.try_acquire(small));
    admission.release(small);
    assert!(admission.try_acquire(small));
    admission.release(small);
    admission.unregister(small);
    assert_eq!(admission.in_flight, 0);
    assert_eq!(admission.limit(), 4);
}

#[test]
fn cancellation_and_failed_jobs_release_their_ownership_before_retry() {
    let mut admission = ContextAdmission {
        capacity: 2,
        ..ContextAdmission::default()
    };
    let cancelled = admission.register(2, None);
    assert!(admission.try_acquire(cancelled));
    assert!(admission.try_acquire(cancelled));
    let waiting = admission.register(2, None);
    assert!(!admission.try_acquire(waiting));
    // Cancellation keeps ownership until each already submitted job drains.
    admission.release(cancelled);
    assert!(admission.try_acquire(waiting));
    assert!(!admission.try_acquire(waiting));
    admission.release(cancelled);
    admission.unregister(cancelled);
    assert!(admission.try_acquire(waiting));
    admission.release(waiting);
    admission.release(waiting);
    admission.unregister(waiting);
    let retry = admission.register(2, None);
    assert!(admission.try_acquire(retry));
    assert!(admission.try_acquire(retry));
}

fn idle_worker() -> Arc<LlamaContextWorker> {
    let (sender, _) = mpsc::channel();
    Arc::new(LlamaContextWorker {
        busy: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        retiring: StdMutex::new(false),
        sender,
    })
}

#[test]
fn small_requests_keep_fixed_capacity_and_retire_only_failed_workers() {
    let mut state = LlamaContextPoolState {
        workers: (0..4).map(|_| idle_worker()).collect(),
        admission: ContextAdmission {
            capacity: 4,
            ..ContextAdmission::default()
        },
    };
    state.admission.register(1, None);
    assert!(retire_idle_workers(&mut state).is_empty());
    state.workers[3].failed.store(true, Ordering::Release);
    assert_eq!(retire_idle_workers(&mut state).len(), 1);
    assert_eq!(state.workers.len(), 3);
}

#[test]
fn batches_of_one_request_share_its_context_allowance() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let scope = Arc::new(());
    let first_batch = admission.register(1, Some(Arc::clone(&scope)));
    let second_batch = admission.register(1, Some(scope));
    assert_eq!(first_batch, second_batch);
    assert!(admission.try_acquire(first_batch));
    assert!(!admission.try_acquire(second_batch));
    let other_request = admission.register(4, Some(Arc::new(())));
    assert!(admission.try_acquire(other_request));
    admission.unregister(second_batch);
    assert!(!admission.try_acquire(first_batch));
    admission.release(first_batch);
    assert!(admission.try_acquire(first_batch));
    admission.release(first_batch);
    admission.unregister(first_batch);
    admission.release(other_request);
    admission.unregister(other_request);
    assert!(admission.operations.is_empty());
}

#[tokio::test]
async fn a_dropped_response_disconnects_and_wakes_its_owner_without_native_inference() {
    let completion = Arc::new(CompletionSignal::default());
    let (sender, mut receiver) = oneshot::channel();
    let generation = completion.generation();
    drop(LlamaResponseSender {
        sender: Some(sender),
        lease: None,
        execution_lease: None,
        completion: Arc::clone(&completion),
    });
    assert!(matches!(
        receiver.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert_ne!(completion.generation(), generation);
    completion.wait(generation).await;
}

#[tokio::test]
async fn cancellation_response_keeps_its_structured_error_and_notifies_waiters() {
    let completion = Arc::new(CompletionSignal::default());
    let (sender, mut receiver) = oneshot::channel();
    let generation = completion.generation();
    LlamaResponseSender {
        sender: Some(sender),
        lease: None,
        execution_lease: None,
        completion: Arc::clone(&completion),
    }
    .send(Err(ModelError::cancelled(
        "cancelled after native completion",
    )
    .with_cause("fixture")));
    let error = receiver
        .try_recv()
        .expect("response")
        .expect_err("cancelled");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert_eq!(error.cause(), Some("fixture"));
    assert!(!should_fallback_to_cpu(true, &error));
    assert_ne!(completion.generation(), generation);
}

#[test]
fn each_input_sizes_its_own_context_on_every_backend() {
    for backend in [
        LlamaExecutionBackend::Cpu,
        LlamaExecutionBackend::Cuda,
        LlamaExecutionBackend::OtherGpu,
    ] {
        for architecture in ["qwen3", "gemma-embedding"] {
            let context_size = if architecture == "qwen3" {
                8_192
            } else {
                2_048
            };
            let budget = LlamaExecutionBudget::resolve(backend, 4, &test_resources(4));
            assert_eq!(budget.contexts, 4);
            let execution = LlamaBatchExecution {
                backend,
                architecture: architecture.to_owned(),
                context_size,
                budget,
                concurrency: 4,
            };
            let profiles = [32, context_size as usize, 64, 64]
                .into_iter()
                .enumerate()
                .map(|(index, length)| {
                    execution
                        .context_profile(&PreparedLlamaInput {
                            index,
                            tokens: vec![LlamaToken::new(7); length],
                        })
                        .expect("single input profile")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                profiles
                    .iter()
                    .map(|profile| profile.context_size)
                    .collect::<Vec<_>>(),
                if architecture == "qwen3" {
                    vec![256, 8_192, 256, 256]
                } else {
                    vec![2_048; 4]
                }
            );
            assert_eq!(
                profiles
                    .iter()
                    .map(|profile| profile.micro_batch_capacity)
                    .collect::<Vec<_>>(),
                if architecture == "qwen3" {
                    vec![32, 256, 64, 64]
                } else {
                    vec![32, 2_048, 64, 64]
                }
            );
        }
    }
}

#[test]
fn invalid_input_lengths_are_rejected_before_dispatch() {
    let execution = LlamaBatchExecution {
        backend: LlamaExecutionBackend::Cpu,
        architecture: "qwen3".to_owned(),
        context_size: 256,
        budget: LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cpu, 4, &test_resources(4)),
        concurrency: 4,
    };
    for length in [0, 257] {
        let error = execution
            .context_profile(&PreparedLlamaInput {
                index: 0,
                tokens: vec![LlamaToken::new(7); length],
            })
            .expect_err("invalid input length");
        assert!(!should_fallback_to_cpu(true, &error));
    }
}

#[tokio::test]
async fn another_operations_completion_wakes_waiters_with_a_pending_long_job() {
    let completion = Arc::new(CompletionSignal::default());
    let long_operation = Arc::clone(&completion);
    let short_operation = Arc::clone(&completion);
    let observed = long_operation.generation();
    // A global capacity release must be observable even if this operation's
    // own pending native job has not completed.
    short_operation.notify();
    assert_ne!(long_operation.generation(), observed);
    long_operation.wait(observed).await;
}

struct TestPool {
    state: Arc<StdMutex<LlamaContextPoolState>>,
    completion: Arc<CompletionSignal>,
    resources: Arc<LlamaResources>,
}

impl TestPool {
    fn new(capacity: usize) -> Self {
        Self {
            state: Arc::new(StdMutex::new(LlamaContextPoolState {
                admission: ContextAdmission {
                    capacity,
                    ..ContextAdmission::default()
                },
                ..LlamaContextPoolState::default()
            })),
            completion: Arc::new(CompletionSignal::default()),
            resources: test_resources(capacity),
        }
    }

    async fn run(
        &self,
        count: usize,
        concurrency: usize,
        signal: Option<&CancellationToken>,
        keepalive: Option<Arc<dyn Send + Sync>>,
        create_worker: impl FnMut() -> Result<Arc<LlamaContextWorker>, ModelError> + Send,
    ) -> Result<EmbeddingResult, ModelError> {
        let backend = LlamaExecutionBackend::Cpu;
        let budget = LlamaExecutionBudget::resolve(backend, concurrency, &self.resources);
        let operation =
            LlamaOperationLease::register(&self.state, &self.completion, budget.contexts, None);
        let execution = LlamaBatchExecution {
            backend,
            architecture: "qwen3".to_owned(),
            context_size: 256,
            budget,
            concurrency,
        };
        run_prepared(
            PreparedLlamaBatch {
                inputs: (0..count)
                    .map(|index| PreparedLlamaInput {
                        index,
                        tokens: vec![LlamaToken::new(7); 32],
                    })
                    .collect(),
                truncated: Vec::new(),
            },
            execution,
            operation,
            signal,
            keepalive,
            create_worker,
        )
        .await
    }

    fn in_flight(&self) -> usize {
        lock_llama_mutex(&self.state).admission.in_flight
    }

    fn assert_released(&self) {
        let state = lock_llama_mutex(&self.state);
        assert_eq!(state.admission.in_flight, 0);
        assert!(state.admission.operations.is_empty());
        assert!(
            state
                .workers
                .iter()
                .all(|worker| !worker.busy.load(Ordering::Acquire))
        );
    }
}

fn worker_channel() -> (Arc<LlamaContextWorker>, mpsc::Receiver<LlamaWorkerMessage>) {
    let (sender, receiver) = mpsc::channel();
    (
        Arc::new(LlamaContextWorker {
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            retiring: StdMutex::new(false),
            sender,
        }),
        receiver,
    )
}

fn received_job(receiver: &mpsc::Receiver<LlamaWorkerMessage>) -> LlamaWorkerJob {
    match receiver.try_recv().expect("native job submitted") {
        LlamaWorkerMessage::Embed(job) => job,
        LlamaWorkerMessage::Shutdown => panic!("unexpected shutdown"),
    }
}

fn finish(job: LlamaWorkerJob) {
    let index = job.input.index;
    job.response.send(Ok((index, vec![1.0])));
}

fn execution_fixture(pool: &TestPool) -> LlamaBatchExecution {
    LlamaBatchExecution {
        backend: LlamaExecutionBackend::Cpu,
        architecture: "qwen3".to_owned(),
        context_size: 256,
        budget: LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cpu, 1, &pool.resources),
        concurrency: 1,
    }
}

#[test]
fn another_runtime_can_retire_a_leased_worker_before_submission() {
    let owner = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("owner runtime");
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("caller runtime");
    let worker = owner.block_on(async {
        super::super::worker::spawn(|receiver| {
            assert!(matches!(receiver.recv(), Ok(LlamaWorkerMessage::Shutdown)));
        })
        .expect("worker")
    });
    let pool = TestPool::new(1);
    let operation = LlamaOperationLease::register(&pool.state, &pool.completion, 1, None);
    let lease = caller.block_on(async {
        operation
            .try_lease_worker(|| Ok(Arc::clone(&worker)))
            .expect("lease")
            .expect("capacity")
    });
    assert_eq!(pool.in_flight(), 1);
    drop(owner);
    caller.block_on(async {
        let execution = execution_fixture(&pool);
        let input = PreparedLlamaInput {
            index: 0,
            tokens: vec![LlamaToken::new(7); 32],
        };
        let profile = execution.context_profile(&input).expect("profile");
        let Dispatch::Retired(input) =
            dispatch_input(lease, input, profile, &execution, None, None)
                .expect("unsubmitted input")
        else {
            panic!("retired worker must return the input without starting it");
        };
        assert_eq!(pool.in_flight(), 0);
        let result = run_prepared(
            PreparedLlamaBatch {
                inputs: vec![input],
                truncated: vec![],
            },
            execution,
            operation,
            None,
            None,
            || {
                super::super::worker::spawn(|receiver| {
                    while let Ok(LlamaWorkerMessage::Embed(job)) = receiver.recv() {
                        finish(job);
                    }
                })
            },
        )
        .await
        .expect("another runtime completes the same input");
        assert_eq!(result.vectors, vec![vec![1.0]]);
    });
    pool.assert_released();
    drop(caller);
}

#[tokio::test]
async fn retirement_before_enqueue_retries_only_unsubmitted_work_once() {
    for retire_replacement in [false, true] {
        let pool = TestPool::new(1);
        let (worker, receiver) = worker_channel();
        let mut creates = 0;
        let mut request = Box::pin(pool.run(1, 1, None, None, || {
            creates += 1;
            if creates == 1 || retire_replacement {
                let retired = idle_worker();
                retired.retire();
                Ok(retired)
            } else {
                Ok(Arc::clone(&worker))
            }
        }));
        if retire_replacement {
            let error = request.await.expect_err("bounded retirement retry");
            assert!(!should_fallback_to_cpu(true, &error));
            assert!(receiver.try_recv().is_err());
        } else {
            assert!(futures_util::poll!(&mut request).is_pending());
            assert_eq!(pool.in_flight(), 1);
            finish(received_job(&receiver));
            assert_eq!(
                request.await.expect("retry success").vectors,
                vec![vec![1.0]]
            );
            assert!(
                receiver.try_recv().is_err(),
                "accepted work must not be replayed"
            );
        }
        assert_eq!(creates, 2);
        pool.assert_released();
    }
}

#[test]
fn another_runtime_shutdown_drains_an_already_submitted_job() {
    let owner = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("owner runtime");
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("caller runtime");
    let (release, held) = mpsc::channel();
    let worker = owner.block_on(async {
        super::super::worker::spawn(move |receiver| {
            let job = match receiver.recv().expect("first message") {
                LlamaWorkerMessage::Embed(job) => job,
                LlamaWorkerMessage::Shutdown => panic!("submission must precede shutdown"),
            };
            // Observing Shutdown proves retirement has run. Accepted work still
            // owns its lease and must return a result before this thread exits.
            assert!(matches!(receiver.recv(), Ok(LlamaWorkerMessage::Shutdown)));
            release.send(()).expect("retirement observed");
            finish(job);
        })
        .expect("worker")
    });
    let pool = TestPool::new(1);
    let mut request = Box::pin(pool.run(1, 1, None, None, || Ok(Arc::clone(&worker))));
    caller.block_on(async {
        assert!(futures_util::poll!(&mut request).is_pending());
    });
    drop(owner);
    held.recv_timeout(std::time::Duration::from_secs(10))
        .expect("shutdown delivered after job");
    assert_eq!(
        caller.block_on(request).expect("accepted result").vectors,
        vec![vec![1.0]]
    );
    pool.assert_released();
    drop(caller);
}

#[test]
fn a_dropped_request_can_release_its_last_keepalive_on_the_native_worker() {
    struct RetireWorker(Arc<LlamaContextWorker>);
    impl Drop for RetireWorker {
        fn drop(&mut self) {
            shutdown_workers(vec![Arc::clone(&self.0)]);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let cleaned = Arc::new(AtomicBool::new(false));
    let native_cleaned = Arc::clone(&cleaned);
    let pool = TestPool::new(1);
    runtime.block_on(async {
        let (started, dispatched) = oneshot::channel();
        let (release, held) = mpsc::channel();
        let worker = super::super::worker::spawn(move |receiver| {
            let LlamaWorkerMessage::Embed(job) = receiver.recv().expect("job") else {
                panic!("expected native job");
            };
            let _ = started.send(());
            held.recv().expect("release native work");
            // The caller has dropped its future. Sending this result drops the
            // final execution keepalive on this thread and requests our shutdown.
            finish(job);
            assert!(matches!(receiver.recv(), Ok(LlamaWorkerMessage::Shutdown)));
            native_cleaned.store(true, Ordering::Release);
        })
        .expect("worker");
        let keepalive = Arc::new(RetireWorker(Arc::clone(&worker)));
        let mut request =
            Box::pin(pool.run(1, 1, None, Some(keepalive), || Ok(Arc::clone(&worker))));
        assert!(futures_util::poll!(&mut request).is_pending());
        dispatched.await.expect("native job started");
        drop(request);
        assert_eq!(pool.in_flight(), 1);
        release.send(()).expect("finish native work");
    });
    drop(runtime);
    assert!(cleaned.load(Ordering::Acquire));
    pool.assert_released();
}

#[tokio::test]
async fn queued_context_and_native_completion_leave_shared_compute_available() {
    let compute = crate::models::runtime::ModelComputeRuntime::for_test(2);
    let pool = TestPool::new(1);
    let (worker, receiver) = worker_channel();
    // Preparation has used the same compute pool as Potion/model2vec.
    compute.run(|| ()).await.expect("first preparation");
    let first = pool.run(1, 1, None, None, || Ok(Arc::clone(&worker)));
    tokio::pin!(first);
    assert!(futures_util::poll!(&mut first).is_pending());
    let held = received_job(&receiver);
    compute.run(|| ()).await.expect("second preparation");
    let second = pool.run(1, 1, None, None, || panic!("must reuse the worker"));
    tokio::pin!(second);
    assert!(futures_util::poll!(&mut second).is_pending());
    assert_eq!(pool.in_flight(), 1);
    assert!(receiver.try_recv().is_err());
    // Timeout is only a deadlock guard; the assertion is that other model work
    // executes while native work remains held and the second request is queued.
    let other = tokio::time::timeout(std::time::Duration::from_secs(5), compute.run(|| 42))
        .await
        .expect("other model must start without releasing llama work")
        .expect("shared compute");
    assert_eq!(other, 42);
    assert_eq!(pool.in_flight(), 1);
    finish(held);
    first.await.expect("first result");
    assert!(futures_util::poll!(&mut second).is_pending());
    finish(received_job(&receiver));
    second.await.expect("second result");
    pool.assert_released();
}

#[tokio::test]
async fn cancelling_queued_work_does_not_dispatch_or_release_another_jobs_slot() {
    let pool = TestPool::new(1);
    let (worker, receiver) = worker_channel();
    let first = pool.run(1, 1, None, None, || Ok(Arc::clone(&worker)));
    tokio::pin!(first);
    assert!(futures_util::poll!(&mut first).is_pending());
    let held = received_job(&receiver);
    let signal = CancellationToken::new();
    let waiting = pool.run(1, 1, Some(&signal), None, || panic!("capacity full"));
    tokio::pin!(waiting);
    assert!(futures_util::poll!(&mut waiting).is_pending());
    signal.cancel();
    assert_eq!(
        waiting.await.expect_err("cancelled queue").code(),
        crate::EngineError::CANCELLED
    );
    assert_eq!(pool.in_flight(), 1);
    assert!(receiver.try_recv().is_err());
    finish(held);
    first.await.expect("first result");
    pool.assert_released();
}

#[tokio::test]
async fn dropping_inflight_future_retains_native_ownership_and_runtime_keepalive() {
    let pool = TestPool::new(1);
    let (worker, receiver) = worker_channel();
    let keepalive: Arc<dyn Send + Sync> = Arc::new(());
    let weak = Arc::downgrade(&keepalive);
    let mut first = Box::pin(pool.run(2, 1, None, Some(keepalive), || Ok(Arc::clone(&worker))));
    assert!(futures_util::poll!(&mut first).is_pending());
    let held = received_job(&receiver);
    drop(first); // Must not block, drain synchronously, or return native capacity.
    assert!(weak.upgrade().is_some());
    assert_eq!(pool.in_flight(), 1);
    let next = pool.run(1, 1, None, None, || panic!("must reuse worker"));
    tokio::pin!(next);
    assert!(futures_util::poll!(&mut next).is_pending());
    assert!(receiver.try_recv().is_err());
    finish(held);
    assert!(weak.upgrade().is_none());
    assert!(futures_util::poll!(&mut next).is_pending());
    finish(received_job(&receiver));
    next.await.expect("capacity recovered");
    assert!(receiver.try_recv().is_err()); // Dropped request's second input never ran.
    pool.assert_released();
}

#[tokio::test]
async fn inflight_cancellation_drains_asynchronously_and_stops_further_dispatch() {
    let pool = TestPool::new(1);
    let (worker, receiver) = worker_channel();
    let signal = CancellationToken::new();
    let request = pool.run(2, 1, Some(&signal), None, || Ok(Arc::clone(&worker)));
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    let held = received_job(&receiver);
    signal.cancel();
    assert!(futures_util::poll!(&mut request).is_pending());
    assert_eq!(pool.in_flight(), 1);
    assert!(receiver.try_recv().is_err());
    finish(held);
    let error = request
        .await
        .expect_err("cancelled after native completion");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert!(!should_fallback_to_cpu(true, &error));
    assert!(receiver.try_recv().is_err());
    pool.assert_released();
}

#[tokio::test]
async fn worker_disconnect_releases_capacity_without_cpu_fallback_and_allows_recovery() {
    let pool = TestPool::new(1);
    let (worker, receiver) = worker_channel();
    let request = pool.run(1, 1, None, None, || Ok(Arc::clone(&worker)));
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    drop(received_job(&receiver)); // Simulate worker unwinding before a response.
    let error = request.await.expect_err("worker disconnected");
    assert!(!should_fallback_to_cpu(true, &error));
    assert!(worker.failed.load(Ordering::Acquire));
    pool.assert_released();
    assert!(lock_llama_mutex(&pool.state).workers.is_empty());
    let (replacement, receiver) = worker_channel();
    let retry = pool.run(1, 1, None, None, || Ok(Arc::clone(&replacement)));
    tokio::pin!(retry);
    assert!(futures_util::poll!(&mut retry).is_pending());
    finish(received_job(&receiver));
    retry.await.expect("replacement worker result");
    pool.assert_released();
}

#[tokio::test]
async fn failure_to_create_a_worker_restores_admission_without_cpu_fallback() {
    let pool = TestPool::new(1);
    let error = pool
        .run(1, 1, None, None, || {
            Err(ModelError::internal("cannot spawn worker"))
        })
        .await
        .expect_err("worker creation failed");
    assert!(!should_fallback_to_cpu(true, &error));
    pool.assert_released();
}

#[tokio::test]
async fn native_completions_preserve_input_order_and_explicit_fallback_metadata() {
    let pool = TestPool::new(2);
    let (sender, receiver) = mpsc::channel();
    let request = pool.run(2, 2, None, None, || {
        let (worker, inbox) = worker_channel();
        sender.send(inbox).expect("worker inbox");
        Ok(worker)
    });
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    let first = received_job(&receiver.try_recv().expect("first inbox"));
    let second = received_job(&receiver.try_recv().expect("second inbox"));
    assert_eq!(pool.in_flight(), 2);
    second.response.send(Ok((1, vec![2.0])));
    assert!(futures_util::poll!(&mut request).is_pending());
    finish(first);
    assert_eq!(
        request.await.expect("ordered results").vectors,
        [vec![1.0], vec![2.0]]
    );
    pool.assert_released();

    // A native error keeps its explicit fallback permission across async delivery.
    let (worker, inbox) = worker_channel();
    let pool = TestPool::new(1);
    let request = pool.run(1, 1, None, None, || Ok(Arc::clone(&worker)));
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    received_job(&inbox)
        .response
        .send(Err(ModelError::internal("decode failed").native_backend()));
    let error = request.await.expect_err("native failure");
    assert!(should_fallback_to_cpu(true, &error));
    assert!(!should_fallback_to_cpu(false, &error));
    pool.assert_released();
}
