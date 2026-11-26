use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::Duration,
};

use futures_util::poll;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    HostError, NativeScanner, PathPolicy, ReadBatchRequest, ScanRequest, ScanSnapshot, SourceFile,
    TaskControl, WorkspaceScannerPort,
};

use super::{
    ModelRuntimeManager, WorkspaceIndexService, context, empty_index_options, inspected_fts,
};
use crate::api::index::options::WorkspaceChange;

const CHANGED_FILE: &str = "changed.txt";
const UPDATED_BYTES: &[u8] = b"  \n";

#[derive(Debug)]
struct PausedRoot {
    policy: Arc<dyn PathPolicy>,
    entered: Arc<Notify>,
    resume: SyncSender<()>,
}

impl PathPolicy for PausedRoot {
    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        // Called on the native walk worker with the scanner permit already held.
        self.entered.notify_one();
        self.resume
            .send(())
            .map_err(|_| HostError::cancelled("test ended"))?;
        self.policy.can_descend(path)
    }

    fn can_descend_child(&self, path: &Path) -> Result<bool, HostError> {
        self.policy.can_descend_child(path)
    }

    fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
        self.policy.includes_file(path)
    }

    fn includes_child_file(&self, path: &Path) -> Result<bool, HostError> {
        self.policy.includes_child_file(path)
    }
}

struct ObservedScanner {
    native: NativeScanner,
    walk_entered: Arc<Notify>,
    resume_walk: SyncSender<()>,
    ready_to_read: Notify,
    allow_read: CancellationToken,
    read_finished: AtomicBool,
}

#[async_trait::async_trait]
impl WorkspaceScannerPort for ObservedScanner {
    async fn discover(
        &self,
        request: &ScanRequest,
        control: &TaskControl,
    ) -> Result<ScanSnapshot, HostError> {
        if !request.scope_paths.is_empty() {
            return self.native.discover(request, control).await;
        }
        // Only the query requests a full walk. The update scans a single changed path.
        let mut full_walk = request.clone();
        let root = &mut full_walk.roots[0];
        root.policy = Arc::new(PausedRoot {
            policy: Arc::clone(&root.policy),
            entered: Arc::clone(&self.walk_entered),
            resume: self.resume_walk.clone(),
        });
        self.native.discover(&full_walk, control).await
    }

    async fn read_batch(
        &self,
        request: &ReadBatchRequest,
        control: &TaskControl,
    ) -> Result<Vec<SourceFile>, HostError> {
        self.ready_to_read.notify_one();
        self.allow_read.cancelled().await;
        let sources = self.native.read_batch(request, control).await?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].relative_path, Path::new(CHANGED_FILE));
        assert_eq!(sources[0].bytes, UPDATED_BYTES);
        self.read_finished.store(true, Ordering::SeqCst);
        Ok(sources)
    }
}

#[test]
fn incremental_read_waits_for_full_query_walk_on_the_shared_scanner() {
    // One blocking worker holds the walk. The other services reads in submission order.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), Box::pin(exercise_shared_scanner()))
            .await
            .expect("query and update finish");
    });
}

async fn exercise_shared_scanner() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    let path = root.join(CHANGED_FILE);
    // Whitespace produces indexed file snapshots without requiring an embedding model download.
    std::fs::write(&path, b"\n").expect("original file");
    let models = ModelRuntimeManager::new();
    let mut service = WorkspaceIndexService::new();
    let initial = service
        .index(&models, empty_index_options(root))
        .await
        .expect("initial index");
    assert_eq!((initial.files_added, initial.files_failed), (1, 0));
    std::fs::write(&path, UPDATED_BYTES).expect("changed file");

    let (resume_walk, paused_walk) = sync_channel(0);
    let scanner = Arc::new(ObservedScanner {
        native: NativeScanner::new(),
        walk_entered: Arc::default(),
        resume_walk,
        ready_to_read: Notify::new(),
        allow_read: CancellationToken::new(),
        read_finished: AtomicBool::new(false),
    });
    service.scanner = scanner.clone();
    let mut options = empty_index_options(root);
    options.changes = vec![WorkspaceChange::Upsert(CHANGED_FILE.into())];
    let update = service.index(&models, options);
    tokio::pin!(update);
    tokio::select! {
        () = scanner.ready_to_read.notified() => {}
        result = &mut update => panic!("update ended before reading the changed file: {result:?}"),
    }
    let options = inspected_fts(root);
    let query = context(&service, &models, &options, None, false);
    tokio::pin!(query);
    tokio::select! {
        () = scanner.walk_entered.notified() => {}
        result = &mut query => panic!("query ended before entering discovery: {result:?}"),
    }

    scanner.allow_read.cancel();
    assert!(
        poll!(&mut update).is_pending(),
        "update finished during discovery"
    );
    // Drain any read already dispatched to the free worker, then poll the update to
    // observe its result. A read bypassing the shared permit would have finished.
    tokio::task::spawn_blocking(|| ())
        .await
        .expect("blocking worker");
    let update_state = poll!(&mut update);
    assert!(
        !scanner.read_finished.load(Ordering::SeqCst),
        "read overtook query discovery"
    );
    assert!(
        update_state.is_pending(),
        "update finished during discovery"
    );

    paused_walk.recv().expect("release full query walk");
    let (updated, result) = tokio::try_join!(update, query).expect("update and query");
    assert_eq!((updated.files_modified, updated.files_failed), (1, 0));
    assert!(scanner.read_finished.load(Ordering::SeqCst));
    assert_eq!(result.indexing.expect("borrowed writer").state, "running");
}
