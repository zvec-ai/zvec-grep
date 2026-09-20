use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tempfile::tempdir_in;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    AllowAllPaths, HostError, NativeWatcherConfig, NativeWatcherFactory, PathPolicy, RootSpec,
    TaskControl, WatchRequest, WorkspaceChange, WorkspaceWatcherFactoryPort,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_debounces_changes_and_obeys_injected_selection_policy() -> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let root = temporary.path();
    fs::create_dir_all(root.join("node_modules/pkg"))?;
    fs::write(root.join("tracked.ts"), "export const value = 1;\n")?;
    fs::write(
        root.join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )?;

    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(30),
        max_wait: Duration::from_millis(200),
        reconcile_interval: None,
        resume_check_interval: None,
        poll_interval: Some(Duration::from_millis(30)),
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::new(CancellationToken::new());
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(
                    root.to_path_buf(),
                    Arc::new(BlockSubtree(root.join("node_modules"))),
                ),
            },
            &control,
        )
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(
        root.join("tracked.ts"),
        "export const value = 200; // changed\n",
    )?;
    fs::write(
        root.join("node_modules/pkg/index.js"),
        "module.exports = 2;\n",
    )?;

    let batch =
        tokio::time::timeout(Duration::from_secs(5), session.next_changes(&control)).await??;
    assert!(
        batch
            .changes
            .contains(&WorkspaceChange::Upsert(PathBuf::from("tracked.ts")))
    );
    assert!(!batch.changes.iter().any(|change| match change {
        WorkspaceChange::Upsert(path)
        | WorkspaceChange::Delete(path)
        | WorkspaceChange::RescanDirectory(path)
        | WorkspaceChange::DeletePrefix(path) => path.starts_with("node_modules"),
        WorkspaceChange::Rescan => false,
    }));
    session.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_close_does_not_wait_for_a_full_batch_queue() -> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let root = temporary.path();
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(20),
        max_wait: Duration::from_millis(80),
        reconcile_interval: None,
        resume_check_interval: None,
        poll_interval: Some(Duration::from_millis(20)),
        batch_capacity: 1,
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::new(CancellationToken::new());
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(
                    root.to_path_buf(),
                    Arc::new(BlockSubtree(root.join("node_modules"))),
                ),
            },
            &control,
        )
        .await?;

    tokio::time::sleep(Duration::from_millis(60)).await;
    fs::write(root.join("first.rs"), "fn first() {}\n")?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    fs::write(root.join("second.rs"), "fn second() {}\n")?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    tokio::time::timeout(Duration::from_secs(1), session.close()).await??;
    Ok(())
}

#[tokio::test]
async fn watcher_flush_publishes_reconciliation_without_waiting_for_debounce() -> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_secs(60),
        max_wait: Duration::from_secs(60),
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::new(CancellationToken::new());
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(temporary.path().to_path_buf(), Arc::new(AllowAllPaths)),
            },
            &control,
        )
        .await?;
    fs::write(temporary.path().join("changed.rs"), "fn changed() {}")?;
    tokio::time::timeout(Duration::from_secs(2), session.flush()).await??;
    let batch =
        tokio::time::timeout(Duration::from_secs(2), session.next_changes(&control)).await??;
    assert!(batch.changes.contains(&WorkspaceChange::Rescan));
    session.close().await?;
    Ok(())
}

#[derive(Debug)]
struct BlockSubtree(PathBuf);

impl PathPolicy for BlockSubtree {
    fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
        Ok(!path.starts_with(&self.0))
    }

    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        Ok(!path.starts_with(&self.0))
    }
}

#[derive(Debug)]
struct MutablePolicy {
    blocked: PathBuf,
    control: PathBuf,
    enabled: std::sync::atomic::AtomicBool,
}

impl PathPolicy for MutablePolicy {
    fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
        self.can_descend(path)
    }

    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        Ok(!path.starts_with(&self.blocked)
            || self.enabled.load(std::sync::atomic::Ordering::Acquire))
    }

    fn control_file_changed(&self, path: &Path) -> Result<Option<PathBuf>, HostError> {
        if path != self.control {
            return Ok(None);
        }
        self.enabled.store(
            fs::read_to_string(path).is_ok_and(|value| value == "enabled"),
            std::sync::atomic::Ordering::Release,
        );
        Ok(Some(PathBuf::new()))
    }

    fn control_paths(&self) -> Vec<PathBuf> {
        vec![self.control.clone()]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_updates_registrations_after_external_rule_changes_and_new_directories()
-> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let root = temporary.path().join("workspace");
    fs::create_dir_all(root.join("blocked/deep"))?;
    fs::write(root.join("blocked/deep/source.rs"), "initial")?;
    let control_path = temporary.path().join("external.rules");
    fs::write(&control_path, "disabled")?;
    let policy = Arc::new(MutablePolicy {
        blocked: root.join("blocked"),
        control: control_path.clone(),
        enabled: std::sync::atomic::AtomicBool::new(false),
    });
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(20),
        max_wait: Duration::from_millis(80),
        reconcile_interval: None,
        resume_check_interval: None,
        poll_interval: Some(Duration::from_millis(20)),
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::default();
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(root.clone(), policy.clone()),
            },
            &control,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(60)).await;
    fs::write(&control_path, "enabled")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::RescanDirectory(PathBuf::new()),
    )
    .await?;
    assert!(policy.enabled.load(std::sync::atomic::Ordering::Acquire));
    fs::write(
        root.join("blocked/deep/source.rs"),
        "updated after rule change",
    )?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::Upsert(PathBuf::from("blocked/deep/source.rs")),
    )
    .await?;
    fs::create_dir(root.join("blocked/deep/new"))?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::RescanDirectory(PathBuf::from("blocked/deep/new")),
    )
    .await?;
    fs::write(root.join("blocked/deep/new/added.rs"), "newly watched")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::Upsert(PathBuf::from("blocked/deep/new/added.rs")),
    )
    .await?;
    fs::write(&control_path, "disabled")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::RescanDirectory(PathBuf::new()),
    )
    .await?;
    assert!(!policy.enabled.load(std::sync::atomic::Ordering::Acquire));
    session.close().await?;
    Ok(())
}

async fn wait_for_change(
    session: &Arc<dyn zg_host_native::WorkspaceWatchSessionPort>,
    control: &TaskControl,
    expected: WorkspaceChange,
) -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let batch = session.next_changes(control).await?;
            if batch
                .changes
                .iter()
                .any(|actual| covers_change(actual, &expected))
            {
                return Ok::<_, HostError>(());
            }
        }
    })
    .await
    .map_err(|error| format!("waiting for {expected:?}: {error}"))??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_watcher_tracks_external_file_symlinks_at_the_depth_limit() -> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let root = temporary.path().join("workspace");
    let outside = temporary.path().join("external");
    fs::create_dir(&root)?;
    fs::create_dir(&outside)?;
    let target = outside.join("target.rs");
    fs::write(&target, "before")?;
    std::os::unix::fs::symlink(&target, root.join("linked.rs"))?;
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(20),
        max_wait: Duration::from_millis(80),
        reconcile_interval: None,
        resume_check_interval: None,
        ..NativeWatcherConfig::default()
    });
    let mut spec = RootSpec::new(root, Arc::new(AllowAllPaths));
    spec.follow = true;
    spec.max_depth = Some(1);
    let control = TaskControl::default();
    let session = factory
        .watch(&WatchRequest { root: spec }, &control)
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(&target, "after native watch registration")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::Upsert(PathBuf::from("linked.rs")),
    )
    .await?;
    fs::remove_file(&target)?;
    wait_for_change_matching(&session, &control, |change| matches!(change,
        WorkspaceChange::Delete(path) | WorkspaceChange::DeletePrefix(path) if path == Path::new("linked.rs"))).await?;
    fs::write(&target, "recreated target")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::Upsert(PathBuf::from("linked.rs")),
    )
    .await?;
    session.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_recovers_when_a_missing_root_parent_is_recreated_without_control_files()
-> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let parent = temporary.path().join("parent");
    let root = parent.join("workspace");
    fs::create_dir_all(&root)?;
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(20),
        max_wait: Duration::from_millis(80),
        reconcile_interval: None,
        resume_check_interval: None,
        poll_interval: Some(Duration::from_millis(20)),
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::default();
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(root.clone(), Arc::new(AllowAllPaths)),
            },
            &control,
        )
        .await?;
    fs::remove_dir_all(&parent)?;
    wait_for_change(&session, &control, WorkspaceChange::Rescan).await?;
    fs::create_dir(&parent)?;
    wait_for_change(&session, &control, WorkspaceChange::Rescan).await?;
    fs::create_dir(&root)?;
    wait_for_change(&session, &control, WorkspaceChange::Rescan).await?;
    fs::write(root.join("restored.rs"), "initial")?;
    // Creating the file can also report root metadata; after registration settles,
    // changing its contents must produce the actual narrow file event.
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(root.join("restored.rs"), "changed after recreation")?;
    wait_for_change(
        &session,
        &control,
        WorkspaceChange::Upsert(PathBuf::from("restored.rs")),
    )
    .await?;
    session.close().await?;
    Ok(())
}

async fn wait_for_change_matching(
    session: &Arc<dyn zg_host_native::WorkspaceWatchSessionPort>,
    control: &TaskControl,
    matches: impl Fn(&WorkspaceChange) -> bool,
) -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let batch = session.next_changes(control).await?;
            if batch.changes.iter().any(&matches) {
                return Ok::<_, HostError>(());
            }
        }
    })
    .await??;
    Ok(())
}

fn covers_change(actual: &WorkspaceChange, expected: &WorkspaceChange) -> bool {
    actual == expected
        || match (actual, expected) {
            (WorkspaceChange::Rescan, _) => true,
            (
                WorkspaceChange::RescanDirectory(parent),
                WorkspaceChange::Upsert(path) | WorkspaceChange::RescanDirectory(path),
            ) => path.starts_with(parent),
            _ => false,
        }
}

#[derive(Debug)]
struct MarkerPolicy {
    directory: PathBuf,
    marker: PathBuf,
    controls: std::sync::Mutex<Vec<PathBuf>>,
}

impl PathPolicy for MarkerPolicy {
    fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
        self.can_descend(path)
    }

    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        if path.starts_with(&self.directory) && self.marker.exists() {
            *self.controls.lock().expect("controls") = vec![self.marker.clone()];
            return Ok(false);
        }
        Ok(!path.starts_with(&self.marker))
    }

    fn control_file_changed(&self, path: &Path) -> Result<Option<PathBuf>, HostError> {
        Ok((path == self.marker).then(|| PathBuf::from("nested")))
    }

    fn control_paths(&self) -> Vec<PathBuf> {
        self.controls.lock().expect("controls").clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_observes_markers_in_rejected_subtrees_and_updates_the_watch_scope() -> TestResult {
    let temporary = tempdir_in(std::env::current_dir()?)?;
    let root = temporary.path().join("workspace");
    let directory = root.join("nested");
    let marker = directory.join(".git");
    fs::create_dir_all(&marker)?;
    fs::create_dir_all(directory.join("src/deep"))?;
    fs::write(directory.join("src/deep/source.rs"), "before")?;
    let policy = Arc::new(MarkerPolicy {
        directory: directory.clone(),
        marker: marker.clone(),
        controls: std::sync::Mutex::new(Vec::new()),
    });
    let factory = NativeWatcherFactory::default().with_config(NativeWatcherConfig {
        debounce: Duration::from_millis(20),
        max_wait: Duration::from_millis(80),
        reconcile_interval: None,
        resume_check_interval: None,
        poll_interval: Some(Duration::from_millis(20)),
        ..NativeWatcherConfig::default()
    });
    let control = TaskControl::default();
    let session = factory
        .watch(
            &WatchRequest {
                root: RootSpec::new(root, policy),
            },
            &control,
        )
        .await?;
    for directory_marker in [true, false] {
        if directory_marker {
            fs::remove_dir(&marker)?;
        } else {
            fs::remove_file(&marker)?;
        }
        wait_for_change(
            &session,
            &control,
            WorkspaceChange::RescanDirectory("nested".into()),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        fs::write(
            directory.join("src/deep/source.rs"),
            format!("reopened after directory marker: {directory_marker}"),
        )?;
        wait_for_change_matching(&session, &control, |change|
            matches!(change, WorkspaceChange::Upsert(path) if path == Path::new("nested/src/deep/source.rs"))).await?;
        fs::write(&marker, "marker file")?;
        wait_for_change(
            &session,
            &control,
            WorkspaceChange::RescanDirectory("nested".into()),
        )
        .await?;
    }
    session.close().await?;
    Ok(())
}
