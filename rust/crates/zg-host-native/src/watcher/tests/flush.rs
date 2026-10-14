use super::*;

struct Fixture {
    _workspace: tempfile::TempDir,
    root: PathBuf,
    raw: mpsc::Sender<notify::Result<Event>>,
    flush: mpsc::Sender<FlushRequest>,
    batches: mpsc::Receiver<WorkspaceChangeBatch>,
    overflowed: Arc<AtomicBool>,
    close: CancellationToken,
    task: JoinHandle<()>,
}

impl Fixture {
    fn new(healthy: bool) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().canonicalize().expect("root");
        fs::write(root.join("existing.txt"), "existing").expect("existing file");
        let spec = RootSpec::new(root.clone(), Arc::new(crate::AllowAllPaths));
        let (raw, raw_receiver) = mpsc::channel(16);
        let (flush, flush_receiver) = mpsc::channel(2);
        let (batch_sender, batches) = mpsc::channel(16);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_notify = Arc::new(Notify::new());
        let watcher = healthy.then(|| {
            // These tests inject raw events explicitly. Manual polling keeps OS
            // notifications (including activity in the shared temp parent) out
            // of the queue while retaining real registration refresh behavior.
            let mut watcher = NativeWatcher {
                backend: WatchBackend::Poll(
                    PollWatcher::new(|_| {}, NotifyConfig::default().with_manual_polling())
                        .expect("watcher"),
                ),
                registrations: BTreeMap::new(),
                aliases: WatchAliases::default(),
            };
            watcher.refresh(&spec, false).expect("registrations");
            watcher
        });
        let close = CancellationToken::new();
        let task = tokio::spawn(watch_loop(WatchLoop {
            flush_receiver,
            root: spec,
            root_is_file: false,
            config: NativeWatcherConfig {
                debounce: Duration::from_secs(30),
                max_wait: Duration::from_secs(30),
                reconcile_interval: None,
                resume_check_interval: None,
                ..NativeWatcherConfig::default()
            },
            watcher,
            registration_error: (!healthy).then(|| "watcher unavailable".to_owned()),
            raw_sender: raw.clone(),
            raw_receiver,
            overflowed: overflowed.clone(),
            overflow_notify,
            batch_sender,
            close: close.clone(),
        }));
        Self {
            _workspace: workspace,
            root,
            raw,
            flush,
            batches,
            overflowed,
            close,
            task,
        }
    }

    async fn flush(&self, reconcile: bool) {
        self.request_flush(reconcile)
            .await
            .expect("successful flush");
    }

    async fn request_flush(&self, reconcile: bool) -> Result<(), HostError> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.flush
            .send(FlushRequest {
                reconcile,
                acknowledge,
            })
            .await
            .expect("flush request");
        tokio::time::timeout(Duration::from_secs(2), acknowledged)
            .await
            .expect("flush completes")
            .expect("acknowledged")
    }

    async fn close(self) {
        self.close.cancel();
        self.task.await.expect("watcher stops");
    }
}

#[tokio::test]
async fn pending_flush_drains_delivered_events_without_promoting_them_to_rescan() {
    let mut fixture = Fixture::new(true);
    fixture.flush(false).await;
    assert!(fixture.batches.try_recv().is_err());
    fixture
        .raw
        .send(Ok(Event::new(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Content,
        )))
        .add_path(fixture.root.join("existing.txt"))))
        .await
        .expect("raw event");
    fixture.flush(false).await;
    assert_eq!(
        fixture
            .batches
            .try_recv()
            .expect("published before acknowledgment")
            .changes,
        [WorkspaceChange::Upsert("existing.txt".into())]
    );
    fixture.flush(false).await;
    assert!(fixture.batches.try_recv().is_err());
    fixture.flush(true).await;
    assert_eq!(
        fixture
            .batches
            .try_recv()
            .expect("forced reconciliation")
            .changes,
        [WorkspaceChange::Rescan]
    );
    fixture.close().await;
}

#[tokio::test(start_paused = true)]
async fn failed_initial_registration_recovers_in_the_background() {
    let mut fixture = Fixture::new(false);

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    fixture.flush(false).await;
    assert_eq!(
        fixture
            .batches
            .try_recv()
            .expect("recovery reconciliation")
            .changes,
        [WorkspaceChange::Rescan]
    );
    fixture.flush(false).await;
    assert!(fixture.batches.try_recv().is_err());
    fixture.close().await;
}

#[tokio::test]
async fn pending_flush_preserves_overflow_and_backend_failure_reconciliation() {
    let mut fixture = Fixture::new(true);
    fixture.overflowed.store(true, Ordering::Release);
    fixture.flush(false).await;
    assert_eq!(
        fixture
            .batches
            .try_recv()
            .expect("overflow reconciliation")
            .changes,
        [WorkspaceChange::Rescan]
    );
    fixture.close().await;
    let mut fixture = Fixture::new(false);
    fixture
        .request_flush(false)
        .await
        .expect_err("missing backend must fail the barrier");
    assert_eq!(
        fixture
            .batches
            .try_recv()
            .expect("missing backend reconciliation")
            .changes,
        [WorkspaceChange::Rescan]
    );
    fixture.close().await;
}
