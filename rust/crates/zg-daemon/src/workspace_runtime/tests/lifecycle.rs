use super::*;

fn idle_fixture() -> (
    WorkspaceRuntimeManager,
    Arc<ManualWatcherFactory>,
    mpsc::Sender<WorkspaceChangeBatch>,
) {
    let (sender, receiver) = mpsc::channel(8);
    let watchers = Arc::new(ManualWatcherFactory {
        receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
        watches: Mutex::new(Vec::new()),
        closes: Arc::new(AtomicUsize::new(0)),
    });
    (
        WorkspaceRuntimeManager::new(
            Arc::new(RecordingExecutor::default()),
            watchers.clone(),
            SchedulerConfig::default(),
        ),
        watchers,
        sender,
    )
}

async fn drain_runtime_callbacks() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_closes_watcher_forgets_history_and_rejects_stale_callbacks() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (manager, watchers, _sender) = idle_fixture();
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    let submitted = manager
        .submit_index(options.clone(), true)
        .await
        .expect("index");
    drain_runtime_callbacks().await;
    let activity = manager.runtime(root.clone(), &options).expect("runtime");
    let old = Arc::clone(&activity);
    drop(activity);
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    drain_runtime_callbacks().await;
    assert_eq!(manager.snapshot().active_runtimes, 0);
    assert_eq!(watchers.closes.load(Ordering::Acquire), 1);
    assert!(manager.job_for_root(&root).is_none());
    assert!(
        manager
            .inner
            .scheduler
            .wait(submitted.job.id)
            .await
            .is_err()
    );
    let replacement = manager
        .runtime(root.clone(), &options)
        .expect("reactivated");
    manager
        .ensure_watching(Arc::clone(&replacement))
        .await
        .expect("watch");
    manager
        .on_index_succeeded(old, submitted.job, 99, options, true)
        .await
        .expect("stale completion");
    assert_eq!(replacement.indexed_revision.load(Ordering::Acquire), 0);
    assert_eq!(watchers.watches.lock().expect("watches").len(), 2);
    manager.shutdown_all().await.expect("shutdown");
    assert_eq!(watchers.closes.load(Ordering::Acquire), 2);
}

#[tokio::test(start_paused = true)]
async fn foreground_activity_protects_subdirectory_queries_and_renews_idle_deadline() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let child = root.join("src");
    std::fs::create_dir(&child).expect("subdirectory");
    let (manager, _, _sender) = idle_fixture();
    drop(
        manager
            .runtime(root.clone(), &IndexOptions::default())
            .expect("runtime"),
    );
    let activity = manager
        .existing_activity(Some(&child))
        .expect("query admission")
        .expect("owning root");
    assert_eq!(activity.canonical_root, root);
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    drop(activity);
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 0);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn background_activity_blocks_retirement_without_renewing_idle_deadline() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (manager, _, _sender) = idle_fixture();
    let foreground = manager
        .runtime(root, &IndexOptions::default())
        .expect("runtime");
    let background = foreground.continuation();
    drop(foreground);
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    drop(background);
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 0);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn cancelled_activity_releases_retirement_protection() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (manager, _, _sender) = idle_fixture();
    let activity = manager
        .runtime(root, &IndexOptions::default())
        .expect("runtime");
    let pending = tokio::spawn(async move {
        let _activity = activity;
        std::future::pending::<()>().await;
    });
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    pending.abort();
    assert!(pending.await.expect_err("aborted").is_cancelled());
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 0);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn running_and_queued_jobs_survive_idle_expiry_then_retire_without_another_ttl() {
    use crate::job_scheduler::{JobReason, JobState};
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (started, mut calls) = mpsc::unbounded_channel();
    let executor = Arc::new(GatedExecutor {
        started,
        release: tokio::sync::Semaphore::new(0),
    });
    let (_, watchers, _sender) = idle_fixture();
    let manager = WorkspaceRuntimeManager::new(
        executor.clone(),
        watchers.clone(),
        SchedulerConfig::default(),
    );
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .submit_index(options.clone(), false)
        .await
        .expect("index");
    calls.recv().await.expect("running");
    let followup = manager
        .inner
        .scheduler
        .submit(root.clone(), options, JobReason::Watch)
        .expect("successor");
    tokio::time::advance(WorkspaceRuntimeManager::DEFAULT_IDLE_TTL).await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    assert_eq!(manager.inner.scheduler.snapshot().queued, 1);
    executor.release.add_permits(1);
    calls.recv().await.expect("successor runs");
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    executor.release.add_permits(1);
    assert_eq!(
        manager
            .inner
            .scheduler
            .wait(followup.job.id)
            .await
            .expect("finished")
            .job
            .state,
        JobState::Succeeded
    );
    drain_runtime_callbacks().await;
    manager.retire_idle(tokio::time::Instant::now()).await;
    assert_eq!(manager.snapshot().active_runtimes, 0);
    assert_eq!(watchers.closes.load(Ordering::Acquire), 1);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn idle_maintenance_does_not_retain_a_dropped_manager() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let (manager, _, _sender) = idle_fixture();
    drop(
        manager
            .runtime(root, &IndexOptions::default())
            .expect("runtime"),
    );
    let weak = Arc::downgrade(&manager.inner);
    drop(manager);
    drain_runtime_callbacks().await;
    assert!(weak.upgrade().is_none());
}
struct SlowCloseWatcher {
    closing: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

struct SlowCloseFactory(Arc<SlowCloseWatcher>);

#[async_trait]
impl WorkspaceWatcherFactoryPort for SlowCloseFactory {
    async fn watch(
        &self,
        _request: &WatchRequest,
        _control: &TaskControl,
    ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
        Ok(self.0.clone())
    }
}

#[async_trait]
impl WorkspaceWatchSessionPort for SlowCloseWatcher {
    async fn next_changes(&self, control: &TaskControl) -> Result<WorkspaceChangeBatch, HostError> {
        control.cancellation.cancelled().await;
        Err(HostError::cancelled("stopped"))
    }
    async fn close(&self) -> Result<(), HostError> {
        self.closing.notify_one();
        self.release.acquire().await.expect("close gate").forget();
        Ok(())
    }
}

#[tokio::test]
async fn slow_retirement_does_not_block_reactivation_or_forget_its_new_jobs() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("root");
    let watcher = Arc::new(SlowCloseWatcher {
        closing: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let manager = WorkspaceRuntimeManager::new(
        Arc::new(RecordingExecutor::default()),
        Arc::new(SlowCloseFactory(watcher.clone())),
        SchedulerConfig::default(),
    );
    let options = IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    };
    manager
        .submit_index(options.clone(), true)
        .await
        .expect("initial job");
    drain_runtime_callbacks().await;
    let retiring = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .retire_idle(
                    tokio::time::Instant::now() + WorkspaceRuntimeManager::DEFAULT_IDLE_TTL,
                )
                .await;
        }
    });
    watcher.closing.notified().await;
    let replacement = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        manager.submit_index(options, true),
    )
    .await
    .expect("map lock is available")
    .expect("replacement job");
    watcher.release.add_permits(1);
    retiring.await.expect("retired");
    assert_eq!(manager.snapshot().active_runtimes, 1);
    assert_eq!(
        manager.job_for_root(&root).expect("new history").id,
        replacement.job.id
    );
    watcher.release.add_permits(1);
    manager.shutdown_all().await.expect("shutdown");
}

#[tokio::test]
async fn retiring_one_workspace_preserves_shared_engine_models_and_persisted_indexes() {
    use zg_engine::api::index::options::EmbeddingModelSpec;

    let first = tempdir().expect("first workspace");
    let second = tempdir().expect("second workspace");
    let first_root = first.path().canonicalize().expect("first root");
    let second_root = second.path().canonicalize().expect("second root");
    let engine = Arc::new(zg_engine::ZvecGrep::new());
    let (_, watchers, _sender) = idle_fixture();
    let manager = WorkspaceRuntimeManager::new(
        Arc::new(super::super::ZvecGrepIndexExecutor {
            engine: engine.clone(),
        }),
        watchers,
        SchedulerConfig::default(),
    );
    for root in [&first_root, &second_root] {
        manager
            .submit_index(
                IndexOptions {
                    root: Some(root.clone()),
                    embedding: Some(EmbeddingModelSpec {
                        reference: "qwen/text-embedding-v4".into(),
                        revision: None,
                        cache_dir: None,
                        endpoint: None,
                        device: zg_engine::api::index::options::Device::Cpu,
                    }),
                    endpoint: Some("http://127.0.0.1:1/unused-empty-workspace".into()),
                    api_key: Some("fixture-key".into()),
                    allow_remote: true,
                    ..IndexOptions::default()
                },
                true,
            )
            .await
            .expect("empty index needs no remote embedding");
    }
    drain_runtime_callbacks().await;
    assert_eq!(engine.runtime_snapshot().loaded_models, 1);
    assert_eq!(engine.runtime_snapshot().active_model_leases, 0);
    let active = manager
        .existing_activity(Some(&second_root))
        .expect("admission")
        .expect("runtime");
    manager
        .retire_idle(tokio::time::Instant::now() + WorkspaceRuntimeManager::DEFAULT_IDLE_TTL)
        .await;
    assert_eq!(manager.snapshot().active_runtimes, 1);
    assert!(manager.job_for_root(&first_root).is_none());
    assert!(manager.job_for_root(&second_root).is_some());
    assert_eq!(engine.runtime_snapshot().loaded_models, 1);
    assert_eq!(engine.runtime_snapshot().active_model_leases, 0);
    assert!(
        engine
            .info(InfoOptions {
                root: Some(first_root),
                include_status: false
            })
            .await
            .expect("persisted index")
            .indexed
    );
    drop(active);
    manager.shutdown_all().await.expect("shutdown");
    engine.close();
}
