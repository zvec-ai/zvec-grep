use std::{error::Error, fs, path::PathBuf, time::Duration};

use tempfile::tempdir_in;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    DiscoveryOptions, NativeWatcherConfig, NativeWatcherFactory, RootSpec, TaskControl,
    WatchRequest, WorkspaceChange, WorkspaceWatcherFactoryPort,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_debounces_changes_and_filters_default_ignored_paths() -> TestResult {
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
                root: RootSpec {
                    path: root.to_path_buf(),
                    recursive: true,
                    discovery: DiscoveryOptions::default(),
                },
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
                root: RootSpec {
                    path: root.to_path_buf(),
                    recursive: true,
                    discovery: DiscoveryOptions::default(),
                },
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
                root: RootSpec {
                    path: temporary.path().to_path_buf(),
                    recursive: true,
                    discovery: DiscoveryOptions::default(),
                },
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
