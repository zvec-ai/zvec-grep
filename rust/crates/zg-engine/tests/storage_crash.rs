//! Terminate a child with interrupted file states and partially replaced records.
//! Keep this separate so native lock descriptors are never inherited from other tests.
mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_file_records,
    set_native_file_status,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::{IndexOptions, options::WorkspaceChange},
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const CRASH_FIXTURE_ROOT_ENV: &str = "ZG_ENGINE_FILE_STATE_CRASH_FIXTURE_ROOT";

#[tokio::test]
async fn interrupted_file_fixture() -> TestResult {
    let Some(root) = std::env::var_os(CRASH_FIXTURE_ROOT_ENV) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    fs::create_dir_all(root.join("src"))?;
    for name in ["healthy", "update", "deleting", "gone"] {
        fs::write(
            root.join(format!("src/{name}.txt")),
            format!("Orchard {name} document.\n"),
        )?;
    }
    let engine = ZvecGrep::new();
    assert_eq!(engine.index(index_options(&root)).await?.files_added, 4);
    let index_path = engine.info(info_options(&root)).await?.index_path;
    engine.close();
    let identities = file_identities(&index_path)?;
    for name in ["update", "gone"] {
        set_native_file_status(
            &index_path,
            identities[&format!("src/{name}.txt")],
            "not_indexed",
        )?;
    }
    set_native_file_status(&index_path, identities["src/deleting.txt"], "deleting")?;
    fs::remove_file(root.join("src/gone.txt"))?;
    fs::remove_file(root.join("src/deleting.txt"))?;

    // Reproduce a mid-replacement boundary: fragments still reference entities
    // which have been removed. No production-only crash hook is needed.
    let path = index_path.join("entities");
    #[cfg(windows)]
    let path = dunce::simplified(&path);
    let entities = zvec_rust::Collection::open(path.to_str().expect("UTF-8 path"), None)?;
    entities.delete_by_filter(&format!("file_id = {}", identities["src/update.txt"]))?;
    entities.flush()?;
    fs::write(
        root.join(".zvec-grep/writer-ready.json"),
        serde_json::to_vec(&index_path)?,
    )?;
    // Leave native handles open; the parent terminates this process without cleanup.
    loop {
        thread::park();
    }
}

#[tokio::test]
async fn scoped_indexing_finishes_interrupted_updates_and_deletions_after_termination() -> TestResult
{
    let directory = tempfile::Builder::new()
        .prefix("zg-storage-crash-")
        .tempdir()?;
    let root = directory.path().join("workspace");
    fs::create_dir(&root)?;
    let root = fs::canonicalize(root)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(&root, server.address)?;
    let log_path = directory.path().join("child.log");
    let mut child = spawn_writer(&root, &log_path)?;
    let index_path = wait_until_ready(&mut child, &root, &log_path)?;
    child.0.kill()?;
    assert!(!child.0.wait()?.success());
    drop(child);

    let before = native_file_records(&index_path)?;
    let identities = file_identities(&index_path)?;
    assert_eq!(before.len(), 4);
    assert!(!index_path.join("pending.json").exists());
    let inputs = server.inputs.load(Ordering::Acquire);
    assert_eq!(inputs, 4);
    let requests = server.requests.load(Ordering::Acquire);

    let engine = ZvecGrep::new();
    engine.info(info_options(&root)).await?;
    assert_eq!(
        native_file_records(&index_path)?,
        before,
        "opening does not recover or re-embed"
    );
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    // Missing canonical records must not make the entire search fail.
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        let paths = search_paths(&engine, &root, mode).await?;
        assert!(paths.contains("src/healthy.txt"));
        assert!(!paths.contains("src/update.txt"));
    }
    let inputs = server.inputs.load(Ordering::Acquire);
    let resumed = engine
        .index(IndexOptions {
            changes: vec![WorkspaceChange::Upsert(root.join("src/healthy.txt"))],
            ..index_options(&root)
        })
        .await?;
    assert_eq!(resumed.files_failed, 0);
    assert_eq!(resumed.files_deleted, 2);
    assert_eq!(
        server.inputs.load(Ordering::Acquire) - inputs,
        1,
        "only the pending existing file is embedded"
    );
    let files = native_file_records(&index_path)?;
    assert_eq!(files.len(), 2);
    for file in &files {
        assert_eq!(file["index_status"]["kind"], "indexed");
        let name = file["relative_path"]["value"]
            .as_str()
            .expect("path")
            .replace(std::path::MAIN_SEPARATOR, "/");
        assert_eq!(file["id"].as_u64(), Some(u64::from(identities[&name])));
    }
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, &root, mode).await?,
            BTreeSet::from(["src/healthy.txt".to_owned(), "src/update.txt".to_owned(),])
        );
    }
    assert!(!index_path.join("pending.json").exists());
    engine.close();
    Ok(())
}

fn file_identities(path: &Path) -> TestResult<BTreeMap<String, u32>> {
    native_file_records(path)?
        .into_iter()
        .map(|file| {
            // Stored relative_path uses native separators; callers key by forward slash.
            let name = file["relative_path"]["value"]
                .as_str()
                .expect("path")
                .replace(std::path::MAIN_SEPARATOR, "/");
            let id = u32::try_from(file["id"].as_u64().expect("file ID"))?;
            Ok((name, id))
        })
        .collect()
}

async fn search_paths(
    engine: &ZvecGrep,
    root: &Path,
    mode: ContextRouteMode,
) -> TestResult<BTreeSet<String>> {
    Ok(engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode,
                query: "orchard".to_owned(),
            }],
            limit: Some(10),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?
        .items
        .into_iter()
        .map(|item| {
            item.relative_path
                .to_str()
                .expect("UTF-8 path")
                .replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect())
}

struct CrashFixtureChild(Child);

impl Drop for CrashFixtureChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_writer(root: &Path, log_path: &Path) -> TestResult<CrashFixtureChild> {
    let output = File::create(log_path)?;
    let errors = output.try_clone()?;
    Ok(CrashFixtureChild(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "interrupted_file_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CRASH_FIXTURE_ROOT_ENV, root)
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                std::env::var_os("ZVEC_GREP_WORKSPACE_REGISTRY").unwrap_or_else(|| {
                    root.parent()
                        .expect("fixture parent")
                        .join("workspaces.json")
                        .into_os_string()
                }),
            )
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            .spawn()?,
    ))
}

fn wait_until_ready(
    child: &mut CrashFixtureChild,
    root: &Path,
    log_path: &Path,
) -> TestResult<PathBuf> {
    let started = Instant::now();
    let ready = root.join(".zvec-grep/writer-ready.json");
    loop {
        if let Some(status) = child.0.try_wait()? {
            panic!(
                "crash fixture exited: {status}\n{}",
                fs::read_to_string(log_path).unwrap_or_default()
            );
        }
        if let Ok(bytes) = fs::read(&ready)
            && let Ok(path) = serde_json::from_slice::<Value>(&bytes)
        {
            return Ok(PathBuf::from(path.as_str().expect("index path")));
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for fixture\n{}",
            fs::read_to_string(log_path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
