use super::*;

struct Started {
    lane: usize,
    thread: std::thread::ThreadId,
    force_cpu: bool,
    release: mpsc::Sender<()>,
    pool_cpu: Vec<bool>,
}

struct FakeRunner {
    lane: usize,
    started: tokio::sync::mpsc::UnboundedSender<Started>,
    fail: Arc<AtomicBool>,
    shared: Arc<Shared>,
    drops: Option<tokio::sync::mpsc::UnboundedSender<std::thread::ThreadId>>,
    configured: Arc<StdMutex<Vec<bool>>>,
}

impl Runner for FakeRunner {
    fn configure(&mut self, force_cpu: bool) -> Result<(), ModelError> {
        lock_std_mutex(&self.configured)[self.lane] |= force_cpu;
        Ok(())
    }

    fn run(
        &mut self,
        _prepared: &PreparedBatch,
        _entry: TransformersConfig,
        _options: &EmbeddingOptions,
        force_cpu: bool,
    ) -> Result<NativeOutput, ModelError> {
        let (release, released) = mpsc::channel();
        self.started
            .send(Started {
                lane: self.lane,
                thread: std::thread::current().id(),
                force_cpu,
                release,
                pool_cpu: lock_std_mutex(&self.configured).clone(),
            })
            .expect("start event");
        released.recv().expect("release native run");
        if self.fail.swap(false, Ordering::AcqRel) {
            Err(ModelError::internal("injected native error"))
        } else {
            Ok(NativeOutput {
                shape: vec![1, 1],
                data: vec![1.0],
            })
        }
    }
}

impl Drop for FakeRunner {
    fn drop(&mut self) {
        lock_std_mutex(&self.shared.stats).session_count -= 1;
        self.shared.available.notify_waiters();
        if let Some(drops) = &self.drops {
            let _ = drops.send(std::thread::current().id());
        }
    }
}

fn owner(
    provider: TransformersExecutionProvider,
) -> (
    Arc<SessionOwner>,
    tokio::sync::mpsc::UnboundedReceiver<Started>,
    Arc<AtomicBool>,
) {
    owner_with_drops(provider, None)
}

fn owner_with_drops(
    provider: TransformersExecutionProvider,
    drops: Option<tokio::sync::mpsc::UnboundedSender<std::thread::ThreadId>>,
) -> (
    Arc<SessionOwner>,
    tokio::sync::mpsc::UnboundedReceiver<Started>,
    Arc<AtomicBool>,
) {
    let sessions = if provider == TransformersExecutionProvider::Cuda {
        2
    } else {
        1
    };
    owner_with_size(provider, sessions, drops)
}

fn owner_with_size(
    provider: TransformersExecutionProvider,
    sessions: usize,
    drops: Option<tokio::sync::mpsc::UnboundedSender<std::thread::ThreadId>>,
) -> (
    Arc<SessionOwner>,
    tokio::sync::mpsc::UnboundedReceiver<Started>,
    Arc<AtomicBool>,
) {
    let (started, receiver) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared::new(provider, sessions));
    let worker_shared = Arc::clone(&shared);
    let fail = Arc::new(AtomicBool::new(false));
    let worker_fail = Arc::clone(&fail);
    let configured = Arc::new(StdMutex::new(vec![false; sessions]));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        lock_std_mutex(&worker_shared.stats).session_initializations += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::clone(&worker_fail),
            shared: Arc::clone(&worker_shared),
            drops: drops.clone(),
            configured: Arc::clone(&configured),
        }))
    });
    let owner = Arc::new(SessionOwner::new(provider, shared, factory));
    owner
        .initialize(None)
        .expect("all configured sessions load up front");
    (owner, receiver, fail)
}

fn entry() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

fn batch() -> Arc<PreparedBatch> {
    Arc::new(PreparedBatch {
        input_ids: vec![1],
        attention_mask: vec![1],
        token_type_ids: vec![0],
        position_ids: vec![0],
        padding_input_id: 0,
        batch_size: 1,
        sequence_length: 1,
        truncated: vec![],
    })
}

fn start(
    owner: &Arc<SessionOwner>,
    options: EmbeddingOptions,
) -> tokio::task::JoinHandle<Result<NativeOutput, ModelError>> {
    let owner = Arc::clone(owner);
    tokio::spawn(async move {
        owner
            .run(batch(), entry(), options, vec![Instant::now()], None)
            .await
    })
}

fn operation(concurrency: usize) -> EmbeddingOptions {
    EmbeddingOptions {
        execution_concurrency: concurrency,
        execution_scope: Some(Arc::new(())),
        ..EmbeddingOptions::default()
    }
}

#[test]
fn independent_operations_share_capacity_without_sharing_their_execution_limit() {
    for provider in [
        TransformersExecutionProvider::Cpu,
        TransformersExecutionProvider::Cuda,
        TransformersExecutionProvider::DirectMl,
        TransformersExecutionProvider::WebGpu,
    ] {
        let (owner, _starts, _) = owner_with_size(provider, 4, None);
        let first = operation(1);
        let second = operation(1);
        let third = operation(2);
        let a = owner.try_reserve(&first).expect("first operation starts");
        assert!(owner.try_reserve(&first).is_none());
        let b = owner.try_reserve(&second).expect("independent C1 starts");
        assert!(owner.try_reserve(&second).is_none());
        let c = owner.try_reserve(&third).expect("C2 first batch");
        let d = owner.try_reserve(&third).expect("C2 second batch");
        assert!(owner.try_reserve(&operation(4)).is_none(), "pool is full");
        drop(a);
        assert!(
            owner.try_reserve(&third).is_none(),
            "C2 is still at its limit"
        );
        let a = owner
            .try_reserve(&first)
            .expect("C1 reuses the free session");
        drop((a, b, c, d));
        let queries = [operation(1), operation(1)].map(|mut options| {
            options.purpose = EmbeddingPurpose::Query;
            options
        });
        let query = owner.try_reserve(&queries[0]).expect("first query");
        assert!(
            owner.try_reserve(&queries[1]).is_none(),
            "queries retain a pool-wide limit"
        );
        drop(query);
        assert!(owner.try_reserve(&queries[1]).is_some());
    }
}

#[tokio::test]
async fn cancelled_operation_keeps_its_native_slot_without_blocking_other_clients() {
    let (owner, mut starts, _) = owner_with_size(TransformersExecutionProvider::Cpu, 4, None);
    let first = operation(1);
    let second = operation(1);
    let a = start(&owner, first.clone());
    let running_a = starts.recv().await.expect("first operation running");
    let b = start(&owner, second.clone());
    let running_b = starts.recv().await.expect("second C1 overlaps first C1");
    assert_ne!(running_a.lane, running_b.lane);
    let signal = super::super::CancellationToken::new();
    let waiting_options = EmbeddingOptions {
        signal: Some(signal.clone()),
        ..first.clone()
    };
    let mut waiting = Box::pin(owner.reserve(&waiting_options));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    signal.cancel();
    assert_eq!(
        waiting.await.err().expect("cancel waiting batch").code(),
        crate::EngineError::CANCELLED
    );
    a.abort();
    assert!(a.await.expect_err("drop first caller").is_cancelled());
    assert!(
        owner.try_reserve(&first).is_none(),
        "native work still owns operation C"
    );
    running_b
        .release
        .send(())
        .expect("complete second operation");
    assert!(b.await.expect("second result").is_ok());
    let next = owner
        .try_reserve(&second)
        .expect("other operation remains usable");
    drop(next);
    running_a
        .release
        .send(())
        .expect("finish cancelled native work");
    loop {
        let available = owner.availability().notified();
        tokio::pin!(available);
        available.as_mut().enable();
        if owner.try_reserve(&first).is_some() {
            break;
        }
        available.await;
    }
    assert_eq!(owner.stats().session_initializations, 4);
}

#[tokio::test]
async fn cuda_two_lanes_overlap_on_distinct_fixed_threads_without_a_third_session() {
    let (owner, mut starts, _) = owner(TransformersExecutionProvider::Cuda);
    let options = EmbeddingOptions {
        execution_concurrency: 2,
        ..EmbeddingOptions::default()
    };
    let first = start(&owner, options.clone());
    let second = start(&owner, options);
    let a = starts.recv().await.expect("first native start");
    let b = starts
        .recv()
        .await
        .expect("second native starts before first finishes");
    assert_ne!(a.lane, b.lane);
    assert_ne!(a.thread, b.thread);
    assert_eq!(owner.stats().session_count, 2);
    assert_eq!(owner.stats().native_inflight_peak, 2);
    let waiting_options = EmbeddingOptions {
        execution_concurrency: usize::MAX,
        ..EmbeddingOptions::default()
    };
    let mut waiting = Box::pin(owner.reserve(&waiting_options));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    drop(waiting);
    let lane_zero_thread = if a.lane == 0 { a.thread } else { b.thread };
    a.release.send(()).expect("release first");
    b.release.send(()).expect("release second");
    assert!(first.await.expect("first task").is_ok());
    assert!(second.await.expect("second task").is_ok());
    let next = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 1,
            ..EmbeddingOptions::default()
        },
    );
    let c = starts.recv().await.expect("reused native lane");
    assert_eq!(c.lane, 0);
    assert_eq!(c.thread, lane_zero_thread);
    c.release.send(()).expect("release next");
    assert!(next.await.expect("next task").is_ok());
}

#[tokio::test]
async fn dropping_waiter_and_owner_keeps_execution_lease_until_native_return() {
    struct Lease(Option<oneshot::Sender<()>>);
    impl Drop for Lease {
        fn drop(&mut self) {
            let _ = self.0.take().expect("drop once").send(());
        }
    }
    let (owner, mut starts, _) = owner(TransformersExecutionProvider::Cuda);
    let (dropped, mut released) = oneshot::channel();
    let task = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 1,
            execution_lease: Some(Arc::new(Lease(Some(dropped)))),
            ..EmbeddingOptions::default()
        },
    );
    let running = starts.recv().await.expect("native owns request");
    task.abort();
    assert!(task.await.expect_err("aborted waiter").is_cancelled());
    let shared = Arc::clone(&owner.shared);
    let (dropping, started_drop) = oneshot::channel();
    let (dropped_owner, mut finished_drop) = oneshot::channel();
    let shutdown = std::thread::spawn(move || {
        let _ = dropping.send(());
        drop(owner);
        let _ = dropped_owner.send(());
    });
    started_drop.await.expect("owner shutdown started");
    assert!(matches!(
        finished_drop.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        released.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    running.release.send(()).expect("native completes");
    released
        .await
        .expect("lease released after native completion");
    finished_drop.await.expect("owner shutdown joins worker");
    shutdown.join().expect("shutdown thread");
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
}

#[tokio::test]
async fn normal_owner_drop_joins_every_session_destructor() {
    let (drops, mut destroyed) = tokio::sync::mpsc::unbounded_channel();
    let (owner, mut starts, _) = owner_with_drops(TransformersExecutionProvider::Cuda, Some(drops));
    let first = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let second = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let a = starts.recv().await.expect("first native start");
    let b = starts.recv().await.expect("second native start");
    a.release.send(()).expect("complete first");
    b.release.send(()).expect("complete second");
    assert!(first.await.expect("first task").is_ok());
    assert!(second.await.expect("second task").is_ok());
    assert!(
        owner
            .try_reserve(&EmbeddingOptions {
                execution_concurrency: 1,
                ..EmbeddingOptions::default()
            })
            .is_some()
    );
    let shared = Arc::clone(&owner.shared);
    drop(owner);
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
    let x = destroyed
        .try_recv()
        .expect("first destructor finished before owner drop returned");
    let y = destroyed
        .try_recv()
        .expect("second destructor finished before owner drop returned");
    assert!(x == a.thread || x == b.thread);
    assert!(y == a.thread || y == b.thread);
    assert_ne!(x, y);
}

#[tokio::test]
async fn final_owner_released_on_native_thread_destroys_session_before_reply_without_self_join() {
    let (drops, mut destroyed) = tokio::sync::mpsc::unbounded_channel();
    let (owner, mut starts, _) = owner_with_drops(TransformersExecutionProvider::Cuda, Some(drops));
    owner
        .start_worker(0)
        .expect("start worker")
        .recv()
        .expect("worker ready")
        .expect("initialized");
    let options = EmbeddingOptions {
        execution_concurrency: 1,
        execution_lease: Some(owner.clone()),
        ..EmbeddingOptions::default()
    };
    let Reservation {
        lane,
        permit,
        force_cpu,
        ..
    } = owner.try_reserve(&options).expect("reserve native lane");
    let (reply, completed) = oneshot::channel();
    lock_std_mutex(&owner.workers[lane])
        .as_ref()
        .expect("worker")
        .send(Message::Run(Box::new(Job {
            prepared: batch(),
            entry: entry(),
            options,
            enqueued: vec![],
            reply,
            _permit: permit,
            force_cpu,
        })))
        .expect("send native job");
    let running = starts.recv().await.expect("native owns final model lease");
    let shared = Arc::clone(&owner.shared);
    drop(owner);
    running.release.send(()).expect("native finishes");
    assert!(
        completed
            .await
            .expect("reply after self teardown")
            .result
            .is_ok()
    );
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
    let threads = [
        destroyed.try_recv().expect("first session destroyed"),
        destroyed.try_recv().expect("second session destroyed"),
    ];
    assert!(threads.contains(&running.thread));
    assert_ne!(threads[0], threads[1]);
}

#[tokio::test]
async fn cpu_reuses_one_lane_after_errors_and_pre_cancel_does_not_execute() {
    let (owner, mut starts, fail) = owner(TransformersExecutionProvider::Cpu);
    fail.store(true, Ordering::Release);
    let first = start(&owner, EmbeddingOptions::default());
    let event = starts.recv().await.expect("failing run");
    let thread = event.thread;
    event.release.send(()).expect("release error");
    assert!(first.await.expect("failed task").is_err());
    let signal = super::super::CancellationToken::new();
    signal.cancel();
    let result = owner
        .run(
            batch(),
            entry(),
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
            vec![],
            None,
        )
        .await;
    assert_eq!(
        result.expect_err("cancelled").code(),
        crate::EngineError::CANCELLED
    );
    assert_eq!(owner.stats().native_runs, 1);
    let second = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: usize::MAX,
            ..EmbeddingOptions::default()
        },
    );
    let event = starts.recv().await.expect("recovery run");
    assert_eq!(event.thread, thread);
    assert_eq!(event.lane, 0);
    assert_eq!(owner.stats().session_count, 1);
    event.release.send(()).expect("release recovery");
    assert!(second.await.expect("recovered task").is_ok());
}

#[tokio::test]
async fn gpu_failure_retries_on_cpu_without_changing_pool_size() {
    let (owner, mut starts, fail) = owner(TransformersExecutionProvider::Cuda);
    fail.store(true, Ordering::Release);
    let task = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let gpu = starts.recv().await.expect("GPU run");
    assert!(!gpu.force_cpu);
    gpu.release.send(()).expect("fail GPU");
    let cpu = starts.recv().await.expect("fallback run");
    assert!(cpu.force_cpu);
    assert_eq!(cpu.lane, 0);
    assert_eq!(owner.provider(), TransformersExecutionProvider::Cpu);
    cpu.release.send(()).expect("complete fallback");
    assert!(task.await.expect("fallback task").is_ok());
    let next = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let cpu = starts.recv().await.expect("next CPU run");
    assert_eq!(cpu.lane, 0);
    assert!(cpu.force_cpu);
    cpu.release.send(()).expect("complete CPU");
    assert!(next.await.expect("CPU task").is_ok());

    fail.store(true, Ordering::Release);
    let failed_cpu = start(&owner, EmbeddingOptions::default());
    let cpu = starts.recv().await.expect("failing CPU run");
    assert!(cpu.force_cpu);
    cpu.release.send(()).expect("fail CPU");
    assert!(
        failed_cpu
            .await
            .expect("CPU failure delivered without retry")
            .is_err()
    );
    assert!(starts.try_recv().is_err());
}

#[tokio::test]
async fn cuda_limit_counts_active_jobs_instead_of_restricting_slot_numbers() {
    let (owner, _starts, _) = owner(TransformersExecutionProvider::Cuda);
    let one = EmbeddingOptions {
        execution_concurrency: 1,
        ..EmbeddingOptions::default()
    };
    let two = EmbeddingOptions {
        execution_concurrency: 2,
        ..EmbeddingOptions::default()
    };
    let first = owner.try_reserve(&one).expect("lane zero");
    assert_eq!(first.lane, 0);
    assert!(owner.try_reserve(&one).is_none());
    let second = owner.try_reserve(&two).expect("lane one remains available");
    assert_eq!(second.lane, 1);
    assert!(owner.try_reserve(&two).is_none());
    drop(first);
    assert!(
        owner.try_reserve(&one).is_none(),
        "one document is still running"
    );
    let query = EmbeddingOptions {
        purpose: EmbeddingPurpose::Query,
        ..one.clone()
    };
    assert_eq!(
        owner
            .try_reserve(&query)
            .expect("query uses free session")
            .lane,
        0
    );
    drop(second);
    assert!(owner.try_reserve(&one).is_some());
}

#[tokio::test]
async fn secondary_gpu_failure_drains_existing_gpu_work_before_converting_whole_pool() {
    let (owner, mut starts, fail) = owner(TransformersExecutionProvider::Cuda);
    let first = start(&owner, operation(1));
    let first_gpu = starts.recv().await.expect("first GPU owns lane zero");
    assert_eq!(first_gpu.lane, 0);
    fail.store(true, Ordering::Release);
    let second = start(&owner, operation(1));
    let second_gpu = starts.recv().await.expect("second GPU owns lane one");
    assert_eq!(second_gpu.lane, 1);
    second_gpu.release.send(()).expect("fail secondary GPU");
    loop {
        let changed = owner.availability().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if owner.provider() == TransformersExecutionProvider::Cpu {
            break;
        }
        changed.await;
    }
    assert!(
        starts.try_recv().is_err(),
        "CPU must wait for lane zero's existing native call"
    );
    first_gpu.release.send(()).expect("drain primary GPU");
    assert!(first.await.expect("primary GPU completes").is_ok());
    let cpu = starts.recv().await.expect("secondary job retries on CPU");
    assert_eq!(cpu.lane, 0);
    assert!(cpu.force_cpu);
    assert_eq!(owner.stats().session_count, 2);
    cpu.release.send(()).expect("complete fallback");
    assert!(second.await.expect("fallback completes").is_ok());
}

#[tokio::test]
async fn cpu_pool_retains_four_sessions_but_c_one_runs_only_one_at_a_time() {
    let (owner, mut starts, _) = owner_with_size(TransformersExecutionProvider::Cpu, 4, None);
    let options = EmbeddingOptions {
        execution_concurrency: 1,
        ..Default::default()
    };
    let tasks = (0..4)
        .map(|_| start(&owner, options.clone()))
        .collect::<Vec<_>>();
    for _ in 0..4 {
        let event = starts.recv().await.expect("next CPU batch");
        assert!(starts.try_recv().is_err());
        assert!(owner.try_reserve(&options).is_none());
        assert_eq!(owner.stats().session_count, 4);
        event.release.send(()).expect("release native job");
    }
    for task in tasks {
        task.await
            .expect("join caller")
            .expect("inference completes");
    }
    assert_eq!(owner.stats().native_inflight_peak, 1);
}

#[tokio::test]
async fn cuda_pool_keeps_four_sessions_and_clamps_execution_to_pool_size() {
    let (owner, mut starts, _) = owner_with_size(TransformersExecutionProvider::Cuda, 4, None);
    assert_eq!(owner.stats().session_count, 4);
    let one = EmbeddingOptions {
        execution_concurrency: 1,
        ..EmbeddingOptions::default()
    };
    let first = start(&owner, one.clone());
    let event = starts.recv().await.expect("C1 runs one batch");
    assert!(owner.try_reserve(&one).is_none());
    let query = EmbeddingOptions {
        purpose: EmbeddingPurpose::Query,
        ..one.clone()
    };
    let query_slot = owner
        .try_reserve(&query)
        .expect("query can use an idle nonzero slot");
    assert_ne!(query_slot.lane, event.lane);
    drop(query_slot);
    event.release.send(()).expect("finish C1");
    assert!(first.await.expect("C1 task").is_ok());
    let many = EmbeddingOptions {
        execution_concurrency: 99,
        ..EmbeddingOptions::default()
    };
    let tasks = (0..4)
        .map(|_| start(&owner, many.clone()))
        .collect::<Vec<_>>();
    let mut events = Vec::new();
    for _ in 0..4 {
        events.push(starts.recv().await.expect("four overlapping CUDA batches"));
    }
    assert!(owner.try_reserve(&many).is_none());
    assert_eq!(owner.stats().native_inflight_peak, 4);
    for event in events {
        event.release.send(()).expect("finish CUDA run");
    }
    for task in tasks {
        assert!(task.await.expect("CUDA task").is_ok());
    }
    assert_eq!(
        owner.stats().session_count,
        4,
        "lowering C retains the fixed pool"
    );
}

#[tokio::test]
async fn changing_request_concurrency_reuses_every_cpu_session() {
    let (owner, mut starts, _) = owner_with_size(TransformersExecutionProvider::Cpu, 4, None);
    let mut threads = std::collections::HashMap::new();
    for concurrency in [1, 4, 2, 8, 1] {
        let options = EmbeddingOptions {
            execution_concurrency: concurrency,
            ..Default::default()
        };
        let mut tasks = Vec::new();
        let mut events = Vec::new();
        for _ in 0..concurrency.min(4) {
            tasks.push(start(&owner, options.clone()));
            let event = starts.recv().await.expect("native start");
            if let Some(previous) = threads.insert(event.lane, event.thread) {
                assert_eq!(previous, event.thread, "C never recreates native workers");
            }
            events.push(event);
        }
        assert!(owner.try_reserve(&options).is_none());
        for event in events {
            event.release.send(()).expect("release native job");
        }
        for task in tasks {
            task.await
                .expect("join caller")
                .expect("inference completes");
        }
        assert_eq!(owner.stats().session_count, 4);
        assert_eq!(owner.stats().session_initializations, 4);
        assert_eq!(owner.stats().pool_fallbacks, 0);
    }
}

#[tokio::test]
async fn cancelled_fallback_keeps_pool_closed_until_native_changes_finish() {
    struct ConfigureRunner(tokio::sync::mpsc::UnboundedSender<mpsc::Sender<()>>);
    impl Runner for ConfigureRunner {
        fn configure(&mut self, _force_cpu: bool) -> Result<(), ModelError> {
            let (release, wait) = mpsc::channel();
            self.0.send(release).expect("configuration started");
            wait.recv().expect("release configuration");
            Ok(())
        }
        fn run(
            &mut self,
            _: &PreparedBatch,
            _: TransformersConfig,
            _: &EmbeddingOptions,
            _: bool,
        ) -> Result<NativeOutput, ModelError> {
            panic!("cancelled configuration must not start inference")
        }
    }
    let (configuring, mut configurations) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cuda, 2));
    shared.fallback.store(true, Ordering::Release);
    let factory: Arc<Factory> =
        Arc::new(move |_| Ok(Box::new(ConfigureRunner(configuring.clone()))));
    let owner = Arc::new(SessionOwner::new(
        TransformersExecutionProvider::Cpu,
        shared,
        factory,
    ));
    owner.initialize(None).expect("pool loaded");
    let options = EmbeddingOptions {
        execution_concurrency: 1,
        ..EmbeddingOptions::default()
    };
    let task = start(&owner, options.clone());
    let first = configurations
        .recv()
        .await
        .expect("first native configuration");
    let second = configurations
        .recv()
        .await
        .expect("second native configuration");
    task.abort();
    assert!(task.await.expect_err("cancelled waiter").is_cancelled());
    assert!(owner.try_reserve(&options).is_none());
    first.send(()).expect("release first configuration");
    assert!(
        owner.try_reserve(&options).is_none(),
        "second native configuration retains the barrier"
    );
    second.send(()).expect("release second configuration");
    loop {
        let notified = owner.availability().notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !lock_std_mutex(&owner.shared.pool).changing {
            break;
        }
        notified.await;
    }
    assert_eq!(owner.stats().native_runs, 0);
    let retry = owner
        .try_reserve(&options)
        .expect("pool recovers after all workers settle");
    assert!(
        retry.transition.is_some(),
        "cancelled configuration is retried before inference"
    );
    assert!(retry.force_cpu);
    assert!(!lock_std_mutex(&owner.shared.pool).cpu_ready);
    drop(retry);
}

#[tokio::test]
async fn failed_pool_fallback_is_retried_before_inference() {
    struct FailingConfigureRunner {
        inner: FakeRunner,
        fail_configure: Arc<AtomicBool>,
    }
    impl Runner for FailingConfigureRunner {
        fn configure(&mut self, force_cpu: bool) -> Result<(), ModelError> {
            self.inner.configure(force_cpu)?;
            if self.inner.lane == 1 && self.fail_configure.swap(false, Ordering::AcqRel) {
                return Err(ModelError::internal(
                    "injected partial configuration failure",
                ));
            }
            Ok(())
        }

        fn run(
            &mut self,
            prepared: &PreparedBatch,
            entry: TransformersConfig,
            options: &EmbeddingOptions,
            force_cpu: bool,
        ) -> Result<NativeOutput, ModelError> {
            self.inner.run(prepared, entry, options, force_cpu)
        }
    }
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cuda, 2));
    shared.fallback.store(true, Ordering::Release);
    let worker_shared = Arc::clone(&shared);
    let configured = Arc::new(StdMutex::new(vec![false; 2]));
    let fail_configure = Arc::new(AtomicBool::new(true));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        lock_std_mutex(&worker_shared.stats).session_initializations += 1;
        Ok(Box::new(FailingConfigureRunner {
            inner: FakeRunner {
                lane,
                started: started.clone(),
                fail: Arc::new(AtomicBool::new(false)),
                shared: Arc::clone(&worker_shared),
                drops: None,
                configured: Arc::clone(&configured),
            },
            fail_configure: Arc::clone(&fail_configure),
        }))
    });
    let owner = Arc::new(SessionOwner::new(
        TransformersExecutionProvider::Cpu,
        shared,
        factory,
    ));
    owner.initialize(None).expect("initial pool");
    let options = EmbeddingOptions {
        execution_concurrency: 1,
        ..Default::default()
    };
    assert!(
        start(&owner, options.clone())
            .await
            .expect("failed growth")
            .is_err()
    );
    assert_eq!(owner.stats().native_runs, 0);
    assert_eq!(owner.stats().pool_fallbacks, 0);
    assert!(!lock_std_mutex(&owner.shared.pool).cpu_ready);
    let retry = start(&owner, options);
    let event = starts.recv().await.expect("recovered smaller request");
    assert_eq!(event.pool_cpu, vec![true; 2]);
    event.release.send(()).expect("finish recovery");
    assert!(retry.await.expect("recovery task").is_ok());
    assert_eq!(owner.stats().pool_fallbacks, 1);
    assert_eq!(owner.stats().session_count, 2);
}

#[test]
fn partial_pool_initialization_failure_releases_every_created_session() {
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cpu, 4));
    let (started, _) = tokio::sync::mpsc::unbounded_channel();
    let worker_shared = Arc::clone(&shared);
    let configured = Arc::new(StdMutex::new(vec![false; 4]));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        if lane == 2 {
            return Err(ModelError::internal("injected initialization failure"));
        }
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        lock_std_mutex(&worker_shared.stats).session_initializations += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::new(AtomicBool::new(false)),
            shared: Arc::clone(&worker_shared),
            drops: None,
            configured: Arc::clone(&configured),
        }))
    });
    let owner = SessionOwner::new(
        TransformersExecutionProvider::Cpu,
        Arc::clone(&shared),
        factory,
    );
    assert!(owner.initialize(None).is_err());
    drop(owner);
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
}

fn initializing_owner(
    provider: TransformersExecutionProvider,
    on_create: impl Fn(usize, bool) -> Result<(), ModelError> + Send + Sync + 'static,
) -> SessionOwner {
    let shared = Arc::new(Shared::new(provider, 4));
    let worker_shared = Arc::clone(&shared);
    let (started, _) = tokio::sync::mpsc::unbounded_channel();
    let configured = Arc::new(StdMutex::new(vec![false; 4]));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        on_create(lane, worker_shared.fallback.load(Ordering::Acquire))?;
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        lock_std_mutex(&worker_shared.stats).session_initializations += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::new(AtomicBool::new(false)),
            shared: Arc::clone(&worker_shared),
            drops: None,
            configured: Arc::clone(&configured),
        }))
    });
    SessionOwner::new(provider, shared, factory)
}

#[test]
fn cancelled_initialization_stops_at_session_boundaries() {
    for provider in [
        TransformersExecutionProvider::Cpu,
        TransformersExecutionProvider::Cuda,
    ] {
        for cancel_at in [None, Some(0), Some(3)] {
            let signal = CancellationToken::new();
            if cancel_at.is_none() {
                signal.cancel();
            }
            let attempts = Arc::new(AtomicUsize::new(0));
            let worker_attempts = Arc::clone(&attempts);
            let worker_signal = signal.clone();
            let owner = initializing_owner(provider, move |lane, fallback| {
                worker_attempts.fetch_add(1, Ordering::AcqRel);
                assert!(!fallback, "cancellation must not trigger CPU fallback");
                assert!(
                    !worker_signal.is_cancelled(),
                    "no new work after cancellation"
                );
                if Some(lane) == cancel_at {
                    worker_signal.cancel();
                }
                Ok(())
            });
            let shared = Arc::clone(&owner.shared);
            let reporter = ModelDownloadProgressReporter::new("fixture", None, []);
            let error = owner
                .initialize_with_fallback(Some(&signal), &reporter)
                .expect_err("initialization cancelled");
            assert_eq!(error.code(), crate::EngineError::CANCELLED);
            assert_eq!(
                attempts.load(Ordering::Acquire),
                cancel_at.map_or(0, |lane| lane + 1)
            );
            assert_eq!(
                owner.stats().session_count,
                cancel_at.map_or(0, |lane| lane + 1)
            );
            assert!(!shared.fallback.load(Ordering::Acquire));
            drop(owner);
            assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
        }
    }
}

#[test]
fn cancelled_gpu_initialization_does_not_start_cpu_fallback() {
    let signal = CancellationToken::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker_attempts = Arc::clone(&attempts);
    let worker_signal = signal.clone();
    let owner = initializing_owner(
        TransformersExecutionProvider::Cuda,
        move |lane, fallback| {
            worker_attempts.fetch_add(1, Ordering::AcqRel);
            assert!(!fallback, "cancelled GPU failure must not load a CPU pool");
            if lane == 1 {
                worker_signal.cancel();
                return Err(ModelError::internal("injected GPU initialization failure"));
            }
            Ok(())
        },
    );
    let shared = Arc::clone(&owner.shared);
    let reporter = ModelDownloadProgressReporter::new("fixture", None, []);
    let error = owner
        .initialize_with_fallback(Some(&signal), &reporter)
        .expect_err("cancellation wins over GPU failure");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert_eq!(owner.stats().session_initializations, 1);
    assert!(!shared.fallback.load(Ordering::Acquire));
    drop(owner);
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
}

#[test]
fn cpu_fallback_initialization_honors_cancellation() {
    for (cancel_at, fail_on_cancel) in [
        (None, false),
        (Some(0), false),
        (Some(3), false),
        (Some(1), true),
    ] {
        let signal = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let worker_signal = signal.clone();
        let owner = initializing_owner(
            TransformersExecutionProvider::Cuda,
            move |lane, fallback| {
                worker_attempts.fetch_add(1, Ordering::AcqRel);
                assert!(
                    !worker_signal.is_cancelled(),
                    "no new work after cancellation"
                );
                if !fallback && lane == 1 {
                    return Err(ModelError::internal("injected GPU initialization failure"));
                }
                if fallback && Some(lane) == cancel_at {
                    worker_signal.cancel();
                    if fail_on_cancel {
                        return Err(ModelError::internal("injected CPU initialization failure"));
                    }
                }
                Ok(())
            },
        );
        let shared = Arc::clone(&owner.shared);
        let reporter = ModelDownloadProgressReporter::new("fixture", None, []);
        let result = owner.initialize_with_fallback(Some(&signal), &reporter);
        if cancel_at.is_some() {
            assert_eq!(
                result.expect_err("CPU load cancelled").code(),
                crate::EngineError::CANCELLED
            );
        } else {
            result.expect("ordinary GPU failure still falls back successfully");
        }
        let cpu_attempts = cancel_at.map_or(4, |lane| lane + 1);
        let cpu_sessions = cpu_attempts - usize::from(fail_on_cancel);
        assert_eq!(attempts.load(Ordering::Acquire), 2 + cpu_attempts);
        assert_eq!(owner.provider(), TransformersExecutionProvider::Cpu);
        assert_eq!(owner.stats().session_count, cpu_sessions);
        assert_eq!(
            owner.stats().session_initializations,
            1 + cpu_sessions as u64
        );
        drop(owner);
        assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
    }
}
