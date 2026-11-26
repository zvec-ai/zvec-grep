use std::process::{Child, Command, Stdio};

use super::*;

const CHILD_ROOT: &str = "ZG_TEST_INCREMENTAL_WAL_ROOT";
const CHILD_TEST: &str =
    "storage::read_session::tests::wal_recovery::first_search_recovers_interrupted_incremental_wal";

struct CrashWriter(Child);

impl Drop for CrashWriter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn interrupted_incremental_writer(root: &Path) -> ! {
    let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
    let (_lock, storage) = write_unclosed_text(&layout, layout.index_dir(), "vineyard");
    let file = storage.list_files().expect("written file").remove(0);
    let mut snapshot = file.snapshot().clone();
    snapshot.modified_epoch_ms = fs::metadata(root.join("source.txt"))
        .expect("source metadata")
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    let file = FileRecord::new(
        file.id(),
        file.relative_path().clone(),
        snapshot,
        file.index_status().clone(),
    )
    .expect("current source snapshot");
    storage
        .update_file_snapshot(&file)
        .expect("save source timestamp");
    fs::write(layout.temp_dir().join("wal-ready"), b"ready").expect("signal WAL written");
    // The parent kills us without running native destructors, including on Windows.
    loop {
        std::thread::park();
    }
}

fn interrupt_incremental_write(fixture: &Fixture) {
    fs::create_dir_all(fixture.layout.temp_dir()).expect("temporary data");
    let log_path = fixture.layout.temp_dir().join("wal-child.log");
    let ready = fixture.layout.temp_dir().join("wal-ready");
    let log = fs::File::create(&log_path).expect("child output");
    let mut child = CrashWriter(
        Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
            .env(
                CHILD_ROOT,
                fixture.layout.storage_dir().parent().expect("source root"),
            )
            .stdin(Stdio::null())
            .stderr(log.try_clone().expect("child error output"))
            .stdout(log)
            .spawn()
            .expect("incremental writer"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            child.0.try_wait().expect("child status").is_none() && Instant::now() < deadline,
            "writer did not reach WAL checkpoint: {}",
            fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().expect("interrupt incremental writer");
    assert!(!child.0.wait().expect("reap writer").success());
    assert_wal_pending(fixture);
}

fn assert_wal_pending(fixture: &Fixture) {
    let path = fixture.storage_home.join("files");
    #[cfg(windows)]
    let path = dunce::simplified(&path);
    let mut options = zvec_rust::CollectionOptions::new().expect("collection options");
    options.set_read_only(true).expect("read-only options");
    let error = zvec_rust::Collection::open(path.to_str().expect("UTF-8 path"), Some(&options))
        .err()
        .expect("native read-only open must require WAL recovery");
    assert_eq!(error.code, zvec_rust::ErrorCode::FailedPrecondition);
    assert!(
        error
            .message
            .starts_with("Collection requires WAL recovery;")
    );
}

#[tokio::test]
async fn first_search_recovers_interrupted_incremental_wal() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        interrupted_incremental_writer(Path::new(&root));
    }
    for (cached, auto_update) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        let (_manifest, mut options) = fixture.publish();
        interrupt_incremental_write(&fixture);
        options.routes[0].query = "vineyard".into();
        options.auto_update = auto_update;
        let cache = cache();
        let indexing = WorkspaceIndexService::new();
        let models = ModelRuntimeManager::new();
        let result = context(
            &indexing,
            &models,
            &options,
            cached.then_some(&cache),
            false,
        )
        .await
        .expect("first search or automatic refresh check recovers WAL");
        assert_eq!(result.items.len(), 1, "recovered incremental content");
        assert_eq!(models.snapshot().cached_runtimes, 0, "no reindex is needed");
    }
}

#[tokio::test]
async fn first_status_recovers_interrupted_incremental_wal() {
    let fixture = Fixture::new();
    fixture.publish();
    interrupt_incremental_write(&fixture);
    assert!(
        IndexStore::inspect_published(&fixture.layout, |store| store.populated_kinds(&[]))
            .expect("preflight defers WAL recovery")
            .is_none()
    );
    assert_wal_pending(&fixture);
    let result = WorkspaceIndexService::new()
        .info(crate::api::info::InfoOptions {
            root: fixture.layout.storage_dir().parent().map(Path::to_path_buf),
            include_status: true,
            include_counts: true,
        })
        .await
        .expect("first status recovers WAL");
    let status = result.status.expect("index status");
    assert_eq!(status.files_indexed, 1);
    assert_eq!(status.files_modified, 0);
    assert_eq!(status.fragments_indexed, Some(1));
}

#[tokio::test]
async fn wal_recovery_requires_exclusive_storage_access_and_restores_shared_readers() {
    let fixture = Fixture::new();
    let (_manifest, mut options) = fixture.publish();
    interrupt_incremental_write(&fixture);
    let storage_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.storage_home.join("storage.lock"))
        .expect("storage lock");
    storage_lock.try_lock_shared().expect("competing reader");
    let indexing = WorkspaceIndexService::new();
    let models = ModelRuntimeManager::new();
    let cache = cache();
    options.routes[0].query = "vineyard".into();
    let error = context(&indexing, &models, &options, Some(&cache), false)
        .await
        .expect_err("recovery must not write under a shared storage lock");
    assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    assert_wal_pending(&fixture);
    drop(storage_lock);

    let result = context(&indexing, &models, &options, Some(&cache), false)
        .await
        .expect("recovery succeeds after the competing reader leaves");
    assert_eq!(result.items.len(), 1);
    let shared_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.storage_home.join("storage.lock"))
        .expect("storage lock");
    shared_lock
        .try_lock_shared()
        .expect("recovered cache permits other readers");
    let (_lock, lease) = fixture.read(&cache);
    let error = lease
        .storage()
        .resolve_file_ids(&[])
        .expect_err("read-only lease");
    assert_eq!(error.code(), EngineError::PERMISSION_DENIED);
}
