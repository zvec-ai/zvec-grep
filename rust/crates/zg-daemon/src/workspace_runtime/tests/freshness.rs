use super::*;
use crate::job_scheduler::JobReason;

fn fixture(
    executor: Arc<dyn IndexExecutor>,
) -> (WorkspaceRuntimeManager, mpsc::Sender<WorkspaceChangeBatch>) {
    let (sender, receiver) = mpsc::channel(8);
    (
        WorkspaceRuntimeManager::new(
            executor,
            Arc::new(ManualWatcherFactory {
                receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                watches: Mutex::new(Vec::new()),
                closes: Arc::new(AtomicUsize::new(0)),
            }),
            SchedulerConfig::default(),
        ),
        sender,
    )
}

async fn deliver(
    manager: &WorkspaceRuntimeManager,
    sender: &mpsc::Sender<WorkspaceChangeBatch>,
    root: &std::path::Path,
    change: WorkspaceChange,
) {
    sender
        .send(WorkspaceChangeBatch {
            changes: vec![change],
        })
        .await
        .expect("event");
    let runtime = manager
        .runtime(root.to_path_buf(), &IndexOptions::default())
        .expect("runtime");
    manager.flush_watcher(&runtime).await.expect("flush");
    runtime.settle_jobs(&manager.inner.scheduler, None).await;
}

#[tokio::test]
async fn repeated_clean_refreshes_and_narrow_changes_do_not_rescan() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let executor = Arc::new(RecordingExecutor::default());
    let (manager, sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    assert!(
        manager
            .refresh_index(options.clone(), true)
            .await
            .expect("first reconcile")
    );
    for wait in [true, false, true, false] {
        assert!(
            !manager
                .refresh_index(options.clone(), wait)
                .await
                .expect("clean")
        );
    }
    assert_eq!(executor.calls.lock().expect("calls").len(), 1);
    deliver(
        &manager,
        &sender,
        &root,
        WorkspaceChange::Upsert("changed.rs".into()),
    )
    .await;
    assert!(
        !manager
            .refresh_index(options.clone(), true)
            .await
            .expect("narrow change indexed")
    );
    assert_eq!(
        executor.calls.lock().expect("calls")[1].changes,
        [IndexChange::Upsert("changed.rs".into())]
    );
    deliver(&manager, &sender, &root, WorkspaceChange::Rescan).await;
    assert!(
        !manager
            .refresh_index(options, true)
            .await
            .expect("watcher rescan is sufficient")
    );
    assert_eq!(executor.calls.lock().expect("calls").len(), 3);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn concurrent_waits_share_a_refresh_and_background_does_not_wait_for_it() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(0),
    });
    let (manager, _sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root),
        ..IndexOptions::default()
    };
    let first = tokio::spawn({
        let manager = manager.clone();
        let options = options.clone();
        async move { manager.refresh_index(options, true).await }
    });
    jobs.recv().await.expect("first refresh");
    let second = tokio::spawn({
        let manager = manager.clone();
        let options = options.clone();
        async move { manager.refresh_index(options, true).await }
    });
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            manager.refresh_index(options, false)
        )
        .await
        .expect("background returns")
        .expect("scheduled")
    );
    executor.release.add_permits(1);
    assert!(first.await.expect("task").expect("refreshed"));
    assert!(!second.await.expect("task").expect("reused proof"));
    assert!(jobs.try_recv().is_err());
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn a_new_recovery_epoch_cannot_be_cleared_by_an_older_full_refresh() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(0),
    });
    let (manager, _sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    let waiting = tokio::spawn({
        let manager = manager.clone();
        async move { manager.refresh_index(options, true).await }
    });
    jobs.recv().await.expect("first refresh");
    manager
        .runtime(root, &IndexOptions::default())
        .expect("runtime")
        .require_reconciliation();
    executor.release.add_permits(1);
    let second = jobs
        .recv()
        .await
        .expect("new recovery needs another refresh");
    assert_eq!(second.changes, [IndexChange::Rescan]);
    assert!(!waiting.is_finished());
    executor.release.add_permits(1);
    waiting.await.expect("task").expect("fresh");
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn a_successful_narrow_job_does_not_cover_an_earlier_failed_batch() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let executor = Arc::new(RecordingExecutor::default());
    let (manager, sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial refresh");
    executor.fail.store(true, Ordering::Release);
    deliver(
        &manager,
        &sender,
        &root,
        WorkspaceChange::Upsert("failed.rs".into()),
    )
    .await;
    executor.fail.store(false, Ordering::Release);
    deliver(
        &manager,
        &sender,
        &root,
        WorkspaceChange::Upsert("later.rs".into()),
    )
    .await;
    assert!(
        !manager
            .runtime(root, &options)
            .expect("runtime")
            .is_reconciled()
    );
    assert!(
        manager
            .refresh_index(options, true)
            .await
            .expect("repair lost batch")
    );
    assert_eq!(executor.calls.lock().expect("calls").len(), 4);
    assert_eq!(
        executor.calls.lock().expect("calls")[3].changes,
        [IndexChange::Rescan]
    );
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn restarting_a_watcher_requires_a_new_reconciliation() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let executor = Arc::new(RecordingExecutor::default());
    let (manager, _sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial refresh");
    manager.stop_watching(&root).await.expect("watcher stopped");
    assert!(
        manager
            .refresh_index(options, true)
            .await
            .expect("repair watcher gap")
    );
    assert_eq!(executor.calls.lock().expect("calls").len(), 2);
    manager.shutdown_all().await.expect("shutdown");
}

struct PartiallyFailingExecutor;
#[async_trait]
impl IndexExecutor for PartiallyFailingExecutor {
    async fn index(&self, _options: IndexOptions) -> Result<IndexResult, EngineError> {
        Ok(IndexResult {
            files_failed: 1,
            ..IndexResult::default()
        })
    }
}

#[tokio::test]
async fn partial_index_failure_is_not_cached_as_fresh_or_retried_forever() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (manager, _sender) = fixture(Arc::new(PartiallyFailingExecutor));
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        manager.refresh_index(options.clone(), true),
    )
    .await
    .expect("bounded failure")
    .expect_err("not fresh");
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
    assert!(
        !manager
            .runtime(root, &options)
            .expect("runtime")
            .is_reconciled()
    );
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn cancelled_wait_releases_refresh_coordination_without_losing_the_job_proof() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(0),
    });
    let (manager, _sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root),
        ..IndexOptions::default()
    };
    let first = tokio::spawn({
        let manager = manager.clone();
        let options = options.clone();
        async move { manager.refresh_index(options, true).await }
    });
    jobs.recv().await.expect("first refresh");
    first.abort();
    assert!(first.await.expect_err("cancelled").is_cancelled());
    executor.release.add_permits(1);
    assert!(
        !manager
            .refresh_index(options, true)
            .await
            .expect("finish existing work")
    );
    assert!(jobs.try_recv().is_err());
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn native_watcher_clean_refreshes_reuse_the_initial_reconciliation() {
    use zg_engine::api::index::options::EmbeddingModelSpec;
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let engine = Arc::new(zg_engine::ZvecGrep::new());
    let manager = WorkspaceRuntimeManager::native(engine.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        embedding: Some(EmbeddingModelSpec {
            reference: "qwen/text-embedding-v4".into(),
            revision: None,
            cache_dir: None,
            endpoint: None,
            device: zg_engine::api::index::options::Device::Cpu,
        }),
        endpoint: Some("http://127.0.0.1:1/empty-index".into()),
        api_key: Some("fixture-key".into()),
        allow_remote: true,
        ..IndexOptions::default()
    };
    manager
        .submit_index(options.clone(), true)
        .await
        .expect("empty index");
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial watcher reconciliation");
    let id = manager.job_for_root(&root).expect("initial job").id;
    for wait in [true, false, true] {
        assert!(
            !manager
                .refresh_index(options.clone(), wait)
                .await
                .expect("unchanged workspace")
        );
        assert_eq!(manager.job_for_root(&root).expect("same job").id, id);
    }
    manager.shutdown_all().await.expect("shutdown");
    engine.close();
}

#[tokio::test]
async fn rejected_watcher_batches_require_full_repair_after_queue_pressure_ends() {
    let workspace = tempdir().expect("workspace");
    let other = tempdir().expect("other workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(1),
    });
    let (sender, receiver) = mpsc::channel(8);
    let manager = WorkspaceRuntimeManager::new(
        executor.clone(),
        Arc::new(ManualWatcherFactory {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        }),
        SchedulerConfig {
            concurrency: 1,
            queue_capacity: 0,
        },
    );
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial refresh");
    jobs.recv().await.expect("initial scan");
    let blocker = manager
        .inner
        .scheduler
        .submit(
            other.path().to_path_buf(),
            IndexOptions::default(),
            JobReason::Manual,
        )
        .expect("other writer");
    jobs.recv().await.expect("other writer running");
    deliver(
        &manager,
        &sender,
        &root,
        WorkspaceChange::Upsert("lost.rs".into()),
    )
    .await;
    assert!(
        !manager
            .runtime(root, &options)
            .expect("runtime")
            .is_reconciled()
    );
    executor.release.add_permits(1);
    manager
        .inner
        .scheduler
        .wait(blocker.job.id)
        .await
        .expect("other writer finishes");
    executor.release.add_permits(1);
    assert!(
        manager
            .refresh_index(options, true)
            .await
            .expect("repair rejected change")
    );
    assert_eq!(
        jobs.recv().await.expect("repair job").changes,
        [IndexChange::Rescan]
    );
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn background_refresh_accounts_for_late_watcher_notifications() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(1),
    });
    let (manager, sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial reconciliation");
    jobs.recv().await.expect("initial job");
    assert!(
        !manager
            .refresh_index(options.clone(), false)
            .await
            .expect("idle without events")
    );

    // Native backends can deliver another notification after a fresh query returns.
    sender
        .send(WorkspaceChangeBatch {
            changes: vec![WorkspaceChange::Upsert("fresh.txt".into())],
        })
        .await
        .expect("late notification");
    let runtime = manager.runtime(root, &options).expect("runtime");
    manager
        .flush_watcher(&runtime)
        .await
        .expect("notification delivered");
    let incremental = jobs.recv().await.expect("watcher job started");
    assert_eq!(
        incremental.changes,
        [IndexChange::Upsert("fresh.txt".into())]
    );
    let scheduled = tokio::time::timeout(
        Duration::from_secs(1),
        manager.refresh_index(options.clone(), false),
    )
    .await
    .expect("background does not wait for watcher job")
    .expect("background status");
    assert!(scheduled, "the late watcher job is still running");

    executor.release.add_permits(1);
    assert!(
        !manager
            .refresh_index(options.clone(), true)
            .await
            .expect("drain existing update")
    );
    assert!(
        !manager
            .refresh_index(options, false)
            .await
            .expect("idle again")
    );
    assert!(
        jobs.try_recv().is_err(),
        "no unnecessary full reconciliation"
    );
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn concurrent_waiters_receive_progress_before_shared_refresh_finishes() {
    use zg_engine::api::index::progress::{
        IndexProgress, IndexProgressPhase, IndexProgressReporter,
    };
    let workspace = tempdir().expect("workspace");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(0),
    });
    let (manager, _sender) = fixture(executor.clone());
    let options = IndexOptions {
        root: Some(workspace.path().to_path_buf()),
        ..IndexOptions::default()
    };
    let first = tokio::spawn({
        let manager = manager.clone();
        let options = options.clone();
        async move { manager.refresh_index(options, true).await }
    });
    let running = jobs.recv().await.expect("shared job");
    let (progress, mut received) = mpsc::unbounded_channel();
    let second = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .refresh_index(
                    IndexOptions {
                        on_progress: Some(IndexProgressReporter::new(move |event| {
                            let _ = progress.send(event);
                        })),
                        ..options
                    },
                    true,
                )
                .await
        }
    });
    running
        .on_progress
        .expect("scheduler reporter")
        .report(IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(7),
            files_indexed: Some(2),
            files_failed: Some(0),
            detail: None,
            embedding: None,
        });
    let event = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .expect("second waiter receives live progress")
        .expect("event");
    assert_eq!(event.files_total, Some(7));
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    executor.release.add_permits(1);
    first.await.expect("first task").expect("first refresh");
    second.await.expect("second task").expect("second refresh");
    assert!(jobs.try_recv().is_err());
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn public_wait_reuses_reconciliation_without_engine_rescan() {
    use zg_engine::api::{
        context::options::{ContextRoute, ContextRouteMode, RefreshPolicy},
        index::options::EmbeddingModelSpec,
    };
    use zg_transport_mcp::IndexOperationProvider;
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let engine = zg_engine::ZvecGrep::new();
    engine
        .index(IndexOptions {
            root: Some(root.clone()),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/text-embedding-v4".into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: zg_engine::api::index::options::Device::Cpu,
            }),
            endpoint: Some("http://127.0.0.1:1/empty-index".into()),
            api_key: Some("fixture-key".into()),
            allow_remote: true,
            ..IndexOptions::default()
        })
        .await
        .expect("empty index");
    let executor = Arc::new(RecordingExecutor::default());
    let (manager, _sender) = fixture(executor.clone());
    let request = zg_engine::api::context::ContextOptions {
        root: Some(root.clone()),
        refresh: Some(RefreshPolicy::Wait),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "probe".into(),
        }],
        ..Default::default()
    };
    manager
        .search(&engine, request.clone())
        .await
        .expect("initial reconciliation");
    // Keep the controlled watcher quiet. Resident Wait covers delivered events,
    // so an unreported file must not trigger another engine disk scan/index job.
    std::fs::write(root.join("probe.txt"), "probe").expect("scan probe");
    for _ in 0..2 {
        let reply = manager
            .search(&engine, request.clone())
            .await
            .expect("reuse daemon proof");
        assert_eq!(reply.freshness.as_deref(), Some("fresh"));
    }
    assert_eq!(executor.calls.lock().expect("calls").len(), 1);
    assert_eq!(
        engine
            .context(request)
            .await
            .expect_err("direct Wait checks disk")
            .code(),
        EngineError::PERMISSION_DENIED
    );
    manager.shutdown_all().await.expect("shutdown");
    engine.close();
}

struct BufferedWatcher {
    session: ManualWatchSession,
    sender: mpsc::Sender<WorkspaceChangeBatch>,
    pending: AtomicBool,
    flushed: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl WorkspaceWatchSessionPort for BufferedWatcher {
    async fn next_changes(&self, control: &TaskControl) -> Result<WorkspaceChangeBatch, HostError> {
        self.session.next_changes(control).await
    }

    async fn flush_pending(&self) -> Result<(), HostError> {
        if self.pending.swap(false, Ordering::AcqRel) {
            self.sender
                .send(WorkspaceChangeBatch {
                    changes: vec![WorkspaceChange::Upsert("during-index.txt".into())],
                })
                .await
                .expect("buffered event");
        }
        let _ = self.flushed.send(());
        Ok(())
    }

    async fn close(&self) -> Result<(), HostError> {
        Ok(())
    }
}

struct BufferedWatcherFactory(Arc<BufferedWatcher>);

#[async_trait]
impl WorkspaceWatcherFactoryPort for BufferedWatcherFactory {
    async fn watch(
        &self,
        _request: &WatchRequest,
        _control: &TaskControl,
    ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn waiting_for_existing_job_flushes_events_buffered_during_that_job() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut jobs) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(1),
    });
    let (sender, receiver) = mpsc::channel(8);
    let (flushed, mut barriers) = mpsc::unbounded_channel();
    let watcher = Arc::new(BufferedWatcher {
        session: ManualWatchSession {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            closes: Arc::new(AtomicUsize::new(0)),
        },
        sender,
        pending: AtomicBool::new(false),
        flushed,
    });
    let manager = WorkspaceRuntimeManager::new(
        executor.clone(),
        Arc::new(BufferedWatcherFactory(watcher.clone())),
        SchedulerConfig::default(),
    );
    let options = IndexOptions {
        root: Some(root.clone()),
        ..Default::default()
    };
    manager
        .refresh_index(options.clone(), true)
        .await
        .expect("initial reconcile");
    jobs.recv().await.expect("initial job");
    watcher
        .sender
        .send(WorkspaceChangeBatch {
            changes: vec![WorkspaceChange::Upsert("before-index.txt".into())],
        })
        .await
        .expect("first event");
    jobs.recv().await.expect("existing job");
    while barriers.try_recv().is_ok() {}
    let waiting = tokio::spawn({
        let manager = manager.clone();
        async move { manager.refresh_index(options, true).await }
    });
    barriers.recv().await.expect("pre-wait flush");
    watcher.pending.store(true, Ordering::Release);
    executor.release.add_permits(1);
    let followup = tokio::time::timeout(Duration::from_secs(2), jobs.recv())
        .await
        .expect("post-wait flush schedules buffered event")
        .expect("followup");
    assert_eq!(
        followup.changes,
        [IndexChange::Upsert("during-index.txt".into())]
    );
    assert!(!waiting.is_finished());
    executor.release.add_permits(1);
    waiting.await.expect("wait task").expect("fresh");
    assert!(!watcher.pending.load(Ordering::Acquire));
    manager.shutdown_all().await.expect("shutdown");
}
