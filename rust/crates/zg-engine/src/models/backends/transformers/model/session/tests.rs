use super::*;

struct Started {
    lane: usize,
    thread: std::thread::ThreadId,
    force_cpu: bool,
    release: mpsc::Sender<()>,
}

struct FakeRunner {
    lane: usize,
    started: tokio::sync::mpsc::UnboundedSender<Started>,
    fail: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl Runner for FakeRunner {
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
    }
}

fn owner(
    provider: TransformersExecutionProvider,
) -> (
    Arc<SessionOwner>,
    tokio::sync::mpsc::UnboundedReceiver<Started>,
    Arc<AtomicBool>,
) {
    let (started, receiver) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared::new(provider));
    let worker_shared = Arc::clone(&shared);
    let fail = Arc::new(AtomicBool::new(false));
    let worker_fail = Arc::clone(&fail);
    let factory: Arc<Factory> = Arc::new(move |lane| {
        lock_std_mutex(&worker_shared.stats).session_count += 1;
        Ok(Box::new(FakeRunner {
            lane,
            started: started.clone(),
            fail: Arc::clone(&worker_fail),
            shared: Arc::clone(&worker_shared),
        }))
    });
    (
        Arc::new(SessionOwner::new(provider, shared, factory)),
        receiver,
        fail,
    )
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
    drop(owner);
    assert!(matches!(
        released.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    running.release.send(()).expect("native completes");
    released
        .await
        .expect("lease released after native completion");
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
async fn gpu_failure_retries_on_single_cpu_lane_and_retires_second_lane() {
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
async fn one_lane_reservation_leaves_the_second_lane_eligible_for_c2_only() {
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
    assert_eq!(owner.try_reserve(&one).expect("lane zero released").lane, 0);
    drop(second);
}

#[tokio::test]
async fn secondary_gpu_failure_drains_existing_gpu_work_and_uses_only_cpu_lane_zero() {
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
        if owner.provider() == TransformersExecutionProvider::Cpu
            && owner.stats().session_count == 1
        {
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
    assert_eq!(owner.stats().session_count, 1);
    cpu.release.send(()).expect("complete fallback");
    assert!(second.await.expect("fallback completes").is_ok());
}
