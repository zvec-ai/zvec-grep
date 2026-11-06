use super::*;

struct Started {
    lane: usize,
    thread: std::thread::ThreadId,
    force_cpu: bool,
    release: mpsc::Sender<()>,
    threads: usize,
    pool_threads: Vec<usize>,
}

struct FakeRunner {
    lane: usize,
    started: tokio::sync::mpsc::UnboundedSender<Started>,
    fail: Arc<AtomicBool>,
    shared: Arc<Shared>,
    drops: Option<tokio::sync::mpsc::UnboundedSender<std::thread::ThreadId>>,
    threads: usize,
    configured: Arc<StdMutex<Vec<usize>>>,
}

impl Runner for FakeRunner {
    fn configure(&mut self, threads: usize, _force_cpu: bool) -> Result<(), ModelError> {
        self.threads = threads;
        lock_std_mutex(&self.configured)[self.lane] = threads;
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
                threads: self.threads,
                pool_threads: lock_std_mutex(&self.configured).clone(),
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
    let shared = Arc::new(Shared::new(provider, sessions, 1));
    let worker_shared = Arc::clone(&shared);
    let fail = Arc::new(AtomicBool::new(false));
    let worker_fail = Arc::clone(&fail);
    let configured = Arc::new(StdMutex::new(vec![1; sessions]));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::clone(&worker_fail),
            shared: Arc::clone(&worker_shared),
            drops: drops.clone(),
            threads: 1,
            configured: Arc::clone(&configured),
        }))
    });
    let owner = Arc::new(SessionOwner::new(provider, shared, factory));
    owner
        .initialize()
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
        cpu_threads,
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
            cpu_threads,
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
    let first = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 1,
            ..EmbeddingOptions::default()
        },
    );
    let first_gpu = starts.recv().await.expect("first GPU owns lane zero");
    assert_eq!(first_gpu.lane, 0);
    fail.store(true, Ordering::Release);
    let second = start(
        &owner,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
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
async fn cpu_pool_keeps_all_four_sessions_when_budget_is_one() {
    let (owner, mut starts, _) = owner_with_size(TransformersExecutionProvider::Cpu, 4, None);
    assert_eq!(
        owner.stats().session_count,
        4,
        "pool is eager even before the first Run"
    );
    let options = EmbeddingOptions {
        execution_concurrency: 1,
        ..EmbeddingOptions::default()
    };
    let tasks = (0..4)
        .map(|_| start(&owner, options.clone()))
        .collect::<Vec<_>>();
    let mut threads = std::collections::HashSet::new();
    let mut events = Vec::new();
    for _ in 0..4 {
        let event = starts.recv().await.expect("all four CPU sessions run");
        assert_eq!(event.threads, 1);
        assert_eq!(event.pool_threads, vec![1; 4]);
        threads.insert(event.thread);
        events.push(event);
    }
    assert_eq!(threads.len(), 4);
    assert!(owner.try_reserve(&options).is_none());
    for event in events {
        event.release.send(()).expect("finish CPU run");
    }
    for task in tasks {
        assert!(task.await.expect("CPU task").is_ok());
    }
    assert_eq!(owner.stats().session_count, 4);
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
async fn cpu_budget_growth_drains_pool_and_smaller_budgets_reuse_it() {
    let hardware = policy::hardware_threads();
    if hardware < 2 {
        return;
    }
    let sessions = if hardware >= 4 { 2 } else { 1 };
    let (owner, mut starts, _) =
        owner_with_size(TransformersExecutionProvider::Cpu, sessions, None);
    let old = EmbeddingOptions {
        execution_concurrency: 1,
        ..EmbeddingOptions::default()
    };
    let tasks = (0..sessions)
        .map(|_| start(&owner, old.clone()))
        .collect::<Vec<_>>();
    let mut events = Vec::new();
    for _ in 0..sessions {
        events.push(starts.recv().await.expect("old-budget run"));
    }
    let changed = EmbeddingOptions {
        execution_concurrency: hardware,
        ..old.clone()
    };
    assert!(
        owner.try_reserve(&changed).is_none(),
        "new budget must drain old native jobs"
    );
    let last = events.pop().expect("one last old job");
    for event in events {
        event.release.send(()).expect("complete other old jobs");
    }
    assert!(
        owner.try_reserve(&changed).is_none(),
        "one old session still owns native work"
    );
    assert!(
        owner.try_reserve(&old).is_none(),
        "do not refill a draining pool"
    );
    last.release.send(()).expect("complete final old job");
    for task in tasks {
        assert!(task.await.expect("old run").is_ok());
    }
    let next = start(&owner, changed);
    let event = starts.recv().await.expect("new-budget run");
    let threads = policy::threads_per_session(hardware, sessions);
    assert_eq!(
        event.pool_threads,
        vec![threads; sessions],
        "even idle sessions are reconfigured"
    );
    let smaller = owner.try_reserve(&old);
    if sessions > 1 {
        let reservation = smaller.expect("smaller C can use an idle session immediately");
        assert!(reservation.transition.is_none());
        assert_eq!(reservation.cpu_threads, Some(threads));
        drop(reservation);
    } else {
        assert!(smaller.is_none(), "the only session is busy");
    }
    assert!(!lock_std_mutex(&owner.shared.pool).drain);
    event.release.send(()).expect("complete new run");
    assert!(next.await.expect("new run").is_ok());
    for requested in [1, hardware, 1] {
        let task = start(
            &owner,
            EmbeddingOptions {
                execution_concurrency: requested,
                ..old.clone()
            },
        );
        let event = starts.recv().await.expect("reuse larger configuration");
        assert_eq!(event.pool_threads, vec![threads; sessions]);
        event.release.send(()).expect("finish retained-budget run");
        assert!(task.await.expect("retained-budget task").is_ok());
    }
    assert_eq!(owner.stats().pool_reconfigurations, 1);
    assert_eq!(owner.stats().session_count, sessions);
    let query = start(
        &owner,
        EmbeddingOptions {
            purpose: EmbeddingPurpose::Query,
            ..old
        },
    );
    let event = starts.recv().await.expect("query reuses CPU configuration");
    assert_eq!(event.threads, threads);
    assert_eq!(event.pool_threads, vec![threads; sessions]);
    event.release.send(()).expect("complete query");
    assert!(query.await.expect("query").is_ok());
}

#[tokio::test]
async fn cancelled_reconfiguration_keeps_pool_closed_until_native_changes_finish() {
    struct ConfigureRunner(tokio::sync::mpsc::UnboundedSender<mpsc::Sender<()>>);
    impl Runner for ConfigureRunner {
        fn configure(&mut self, _threads: usize, _force_cpu: bool) -> Result<(), ModelError> {
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
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cpu, 2, 1));
    {
        let mut pool = lock_std_mutex(&shared.pool);
        pool.cpu_threads = None;
        // Simulate an interrupted larger configuration independently of the
        // test machine's CPU count; the fake runners create no native threads.
        pool.cpu_threads_high_water = 4;
    }
    let factory: Arc<Factory> =
        Arc::new(move |_| Ok(Box::new(ConfigureRunner(configuring.clone()))));
    let owner = Arc::new(SessionOwner::new(
        TransformersExecutionProvider::Cpu,
        shared,
        factory,
    ));
    owner.initialize().expect("pool loaded");
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
    assert_eq!(retry.cpu_threads, Some(4));
    assert_eq!(lock_std_mutex(&owner.shared.pool).cpu_threads_high_water, 4);
    drop(retry);
}

#[tokio::test]
async fn failed_pool_growth_recovers_at_high_water_for_a_smaller_request() {
    struct FailingConfigureRunner {
        inner: FakeRunner,
        fail_configure: Arc<AtomicBool>,
    }
    impl Runner for FailingConfigureRunner {
        fn configure(&mut self, threads: usize, force_cpu: bool) -> Result<(), ModelError> {
            self.inner.configure(threads, force_cpu)?;
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
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cpu, 2, 1));
    lock_std_mutex(&shared.pool).cpu_threads_high_water = 4;
    let worker_shared = Arc::clone(&shared);
    let configured = Arc::new(StdMutex::new(vec![1; 2]));
    let fail_configure = Arc::new(AtomicBool::new(true));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        Ok(Box::new(FailingConfigureRunner {
            inner: FakeRunner {
                lane,
                started: started.clone(),
                fail: Arc::new(AtomicBool::new(false)),
                shared: Arc::clone(&worker_shared),
                drops: None,
                threads: 1,
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
    owner.initialize().expect("initial pool");
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
    assert_eq!(owner.stats().pool_reconfigurations, 0);
    assert_eq!(lock_std_mutex(&owner.shared.pool).cpu_threads, None);
    let retry = start(&owner, options);
    let event = starts.recv().await.expect("recovered smaller request");
    assert_eq!(event.pool_threads, vec![4; 2]);
    event.release.send(()).expect("finish recovery");
    assert!(retry.await.expect("recovery task").is_ok());
    assert_eq!(owner.stats().pool_reconfigurations, 1);
    assert_eq!(owner.stats().session_count, 2);
}

#[test]
fn partial_pool_initialization_failure_releases_every_created_session() {
    let shared = Arc::new(Shared::new(TransformersExecutionProvider::Cpu, 4, 1));
    let (started, _) = tokio::sync::mpsc::unbounded_channel();
    let worker_shared = Arc::clone(&shared);
    let configured = Arc::new(StdMutex::new(vec![1; 4]));
    let factory: Arc<Factory> = Arc::new(move |lane| {
        if lane == 2 {
            return Err(ModelError::internal("injected initialization failure"));
        }
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::new(AtomicBool::new(false)),
            shared: Arc::clone(&worker_shared),
            drops: None,
            threads: 1,
            configured: Arc::clone(&configured),
        }))
    });
    let owner = SessionOwner::new(
        TransformersExecutionProvider::Cpu,
        Arc::clone(&shared),
        factory,
    );
    assert!(owner.initialize().is_err());
    drop(owner);
    assert_eq!(lock_std_mutex(&shared.stats).session_count, 0);
}
