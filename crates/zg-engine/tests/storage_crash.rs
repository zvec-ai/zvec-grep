//! Keep process termination tests in a separate executable: zvec's native lock
//! descriptors can otherwise be inherited from concurrently running unit tests.
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

use serde_json::{Value, json};
use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_documents,
    native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode, QueryFilter},
        },
        index::{
            IndexOptions,
            progress::{IndexProgressPhase, IndexProgressReporter},
        },
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const CRASH_FIXTURE_ROOT_ENV: &str = "ZG_ENGINE_STORAGE_CHECKPOINT_CRASH_FIXTURE_ROOT";
const CHECKPOINT_FILES: usize = 64;
const PENDING_FILES: usize = 6;
const TOTAL_FILES: usize = CHECKPOINT_FILES + PENDING_FILES;

#[tokio::test]
async fn checkpoint_crash_fixture() -> TestResult {
    let Some(root) = std::env::var_os(CRASH_FIXTURE_ROOT_ENV) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let engine = ZvecGrep::new();
    // Publish an empty initial generation so the interrupted write updates the
    // active index, exercising storage recovery rather than build publication.
    assert_eq!(engine.index(index_options(&root)).await?.files_added, 0);
    let index_path = engine.info(info_options(&root)).await?.index_path;
    fs::create_dir_all(root.join("src").join("deep"))?;
    for index in 0..TOTAL_FILES {
        fs::write(
            root.join(fixture_source_path(index)),
            format!("Orchard document {index}.\n"),
        )?;
    }
    let ready = root.join(".zvec-grep/writer-ready.json");
    engine
        .index(IndexOptions {
            on_progress: Some(IndexProgressReporter::new(move |progress| {
                if progress.phase == IndexProgressPhase::Indexing
                    && progress.files_indexed == Some(TOTAL_FILES)
                {
                    assert_eq!(progress.files_failed, Some(0));
                    fs::write(&ready, serde_json::to_vec(&index_path).expect("index path"))
                        .expect("publish writer readiness");
                    // This callback runs immediately after the write, before final
                    // checkpointing. The parent must kill the still-live writer.
                    loop {
                        thread::park();
                    }
                }
            })),
            ..index_options(&root)
        })
        .await?;
    panic!("fixture completed without stopping at the uncheckpointed batch");
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "keep crash recovery and selective reindex assertions together"
)]
async fn recovers_only_the_uncheckpointed_batch_after_process_termination() -> TestResult {
    let directory = tempfile::Builder::new()
        .prefix("zg-storage-crash-")
        .tempdir()?;
    let root = directory.path().join("workspace");
    fs::create_dir(&root)?;
    let root = fs::canonicalize(root)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(&root, server.address)?;

    // No zvec collection is opened in this process before spawn. The only other
    // test in this executable is the env-guarded child fixture above.
    let log_path = directory.path().join("child.log");
    let mut child = spawn_writer(&root, &log_path)?;
    let index_path = wait_until_ready(&mut child, &root, &log_path)?;
    let pending_path = index_path.join("pending.json");
    let pending: Value = serde_json::from_slice(&fs::read(&pending_path)?)?;
    let pending_sources = pending["files"]
        .as_array()
        .expect("pending source records")
        .iter()
        .map(|change| {
            assert_eq!(change["kind"], "reindex");
            serde_json::from_str::<Value>(change["source"].as_str().expect("source payload"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(pending_sources.len(), PENDING_FILES);
    // Completion order may vary with embedding concurrency; the persisted intent
    // identifies the actual unfinished files rather than assuming filename order.
    let pending_names = pending_sources
        .iter()
        .map(source_name)
        .collect::<BTreeSet<_>>();
    let indexed_names = (0..TOTAL_FILES)
        .map(|index| {
            fixture_source_path(index)
                .to_str()
                .expect("UTF-8 fixture path")
                .to_owned()
        })
        .filter(|name| !pending_names.contains(name))
        .collect::<BTreeSet<_>>();
    child.0.kill()?;
    assert!(
        !child.0.wait()?.success(),
        "writer must end through forced termination"
    );
    drop(child);
    let child_log = fs::read_to_string(&log_path)?;
    assert_eq!(
        server.inputs.load(Ordering::Acquire),
        TOTAL_FILES,
        "{child_log}"
    );

    // The killed writer has released every native handle. Read durable logical
    // identities before engine recovery; never open a parent handle before spawn.
    let mut identities_before_recovery = index_identities(&index_path)?;
    // New records may exist only in durable write intent until recovery.
    for source in &pending_sources {
        let path = PathBuf::from(source_name(source));
        let id = u32::try_from(source["value"]["id"].as_u64().expect("pending file ID"))?;
        if let Some(existing) = identities_before_recovery.files.insert(path, id) {
            assert_eq!(existing, id, "intent preserves an existing source identity");
        }
    }
    assert!(!root.join(".zvec-grep/catalog").exists());
    let directory_ids = &identities_before_recovery.directories;
    assert_eq!(identities_before_recovery.files.len(), TOTAL_FILES);
    assert_eq!(directory_ids.len(), 2);

    let recovery_requests = server.requests.load(Ordering::Acquire);
    let engine = ZvecGrep::new();
    let info = engine
        .info(info_options(&root))
        .await
        .unwrap_or_else(|error| panic!("open after abrupt termination: {error}\n{child_log}"));
    let status = info.status.expect("recovered status");
    assert_eq!(
        (
            status.files_stored,
            status.files_indexed,
            status.files_pending,
            status.files_failed
        ),
        (TOTAL_FILES, CHECKPOINT_FILES, PENDING_FILES, 0),
        "{child_log}"
    );
    assert_eq!(status.entities_indexed, u64::try_from(CHECKPOINT_FILES)?);
    assert!(!pending_path.exists(), "successful recovery clears intent");
    assert_eq!(
        server.inputs.load(Ordering::Acquire),
        TOTAL_FILES,
        "recovery does not embed"
    );

    assert_eq!(server.requests.load(Ordering::Acquire), recovery_requests);
    let files = native_file_records(&index_path)?;
    assert_eq!(files.len(), TOTAL_FILES);
    for file in &files {
        if let Some(source) = pending_sources
            .iter()
            .find(|source| source_name(source) == source_name(file))
        {
            assert_eq!(
                file, source,
                "recovery preserves the complete source snapshot"
            );
            assert_eq!(
                file["value"]["index_status"],
                json!({"kind": "not_indexed"})
            );
        } else {
            assert_eq!(file["value"]["index_status"]["kind"], "indexed");
            assert_eq!(file["value"]["index_status"]["entity_count"], 1);
        }
    }
    let file_paths = files
        .iter()
        .map(|file| {
            (
                u32::try_from(file["value"]["id"].as_u64().expect("numeric file ID"))
                    .expect("u32 file ID"),
                PathBuf::from(source_name(file)),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        file_paths.len(),
        TOTAL_FILES,
        "every source has a unique numeric identity"
    );
    assert_eq!(
        file_paths
            .iter()
            .map(|(id, path)| (path.clone(), *id))
            .collect::<BTreeMap<_, _>>(),
        identities_before_recovery.files,
        "recovered file records retain source and journal identities"
    );
    let file_documents = native_documents(&index_path.join("files"))?;
    // Recovery restores NotIndexed rows as well as their directory projection.
    // This also checks root-file empty arrays in the same active collection.
    assert_document_file_metadata(&file_documents, &file_paths, directory_ids)?;
    assert!(pending_names.iter().any(|name| {
        Path::new(name)
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    }));
    assert_eq!(
        index_identities(&index_path)?,
        identities_before_recovery,
        "recovery preserves every allocated file and directory ID"
    );
    let indexed_file_ids = file_documents
        .iter()
        .filter_map(|doc| {
            let file: Value = serde_json::from_str(
                &doc.get_string("payload")
                    .expect("payload")
                    .expect("file payload"),
            )
            .expect("file JSON");
            indexed_names.contains(&source_name(&file)).then(|| {
                doc.get_u32("file_id")
                    .expect("numeric file ID")
                    .expect("file identity")
            })
        })
        .collect::<BTreeSet<_>>();
    let mut searchable_collections = 0;
    for entry in fs::read_dir(&index_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "entities" || name.starts_with("fragments_") {
            searchable_collections += 1;
            let documents = native_documents(&entry.path())?;
            assert_eq!(documents.len(), CHECKPOINT_FILES, "{name}");
            let file_ids = documents
                .iter()
                .map(|doc| {
                    doc.get_u32("file_id")
                        .expect("numeric file ID")
                        .expect("document file identity")
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(
                file_ids, indexed_file_ids,
                "{name}: unfinished records must be removed"
            );
            if name != "entities" {
                assert_document_file_metadata(&documents, &file_paths, directory_ids)?;
            }
        }
    }
    assert_eq!(searchable_collections, 2);
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, &root, mode, Vec::new()).await?,
            indexed_names
        );
        assert!(
            search_paths(
                &engine,
                &root,
                mode,
                pending_names.iter().cloned().collect()
            )
            .await?
            .is_empty()
        );
    }
    let before_reindex = server.inputs.load(Ordering::Acquire);
    let resumed = engine.index(index_options(&root)).await?;
    assert_eq!(
        (
            resumed.files_unchanged,
            resumed.files_pending,
            resumed.files_failed
        ),
        (CHECKPOINT_FILES, PENDING_FILES, 0)
    );
    assert_eq!(
        server.inputs.load(Ordering::Acquire) - before_reindex,
        PENDING_FILES,
        "only unfinished files require embedding again"
    );
    let status = engine
        .info(info_options(&root))
        .await?
        .status
        .expect("resumed status");
    assert_eq!(
        (status.files_indexed, status.files_pending),
        (TOTAL_FILES, 0)
    );
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, &root, mode, Vec::new()).await?,
            indexed_names.union(&pending_names).cloned().collect()
        );
    }
    for entry in fs::read_dir(&index_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("fragments_") {
            let documents = native_documents(&entry.path())?;
            assert_eq!(documents.len(), TOTAL_FILES);
            assert_document_file_metadata(&documents, &file_paths, directory_ids)?;
        }
    }
    assert_document_file_metadata(
        &native_documents(&index_path.join("files"))?,
        &file_paths,
        directory_ids,
    )?;
    assert_eq!(
        index_identities(&index_path)?,
        identities_before_recovery,
        "recovery and reindex preserve every allocated file and directory ID"
    );
    engine.close();
    Ok(())
}

fn fixture_source_path(index: usize) -> PathBuf {
    let name = format!("crash-{index:03}.txt");
    // Fewer root files than pending slots guarantees that the interrupted batch
    // contains a nested file regardless of extraction completion order.
    if index < PENDING_FILES - 1 {
        PathBuf::from(name)
    } else {
        Path::new("src").join("deep").join(name)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct IdentitySnapshot {
    files: BTreeMap<PathBuf, u32>,
    directories: BTreeMap<PathBuf, u32>,
}

fn index_identities(home: &Path) -> TestResult<IdentitySnapshot> {
    let mut files = BTreeMap::new();
    let mut directories = BTreeMap::new();
    for doc in native_documents(&home.join("directories"))? {
        let id = doc.get_u32("directory_id")?.expect("directory ID");
        let path: Value =
            serde_json::from_str(&doc.get_string("path")?.expect("native directory path"))?;
        assert_eq!(path["encoding"], "utf8");
        let path = PathBuf::from(path["value"].as_str().expect("UTF-8 directory path"));
        assert!(
            directories.insert(path, id).is_none(),
            "unique directory path"
        );
    }
    let mut ids = BTreeSet::new();
    for doc in native_documents(&home.join("files"))? {
        let id = doc.get_u32("file_id")?.expect("numeric source identity");
        let path: Value = serde_json::from_str(&doc.get_string("path")?.expect("native path"))?;
        assert_eq!(path["encoding"], "utf8");
        let path = PathBuf::from(path["value"].as_str().expect("UTF-8 fixture path"));
        let mut ancestors = path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        ancestors.reverse();
        let membership = doc
            .get_array_u32("ancestor_directory_ids")?
            .unwrap_or_default();
        assert_eq!(ancestors.len(), membership.len());
        for (path, directory_id) in ancestors.into_iter().zip(membership) {
            assert_eq!(
                directories.get(path),
                Some(&directory_id),
                "file ancestry references canonical directories"
            );
        }
        assert!(ids.insert(id), "source IDs must be unique");
        assert!(
            files.insert(path, id).is_none(),
            "source paths must be unique"
        );
    }
    assert_eq!(
        directories.values().collect::<BTreeSet<_>>().len(),
        directories.len(),
        "directory IDs must be unique"
    );
    Ok(IdentitySnapshot { files, directories })
}

fn assert_document_file_metadata(
    documents: &[zvec_rust::Doc],
    file_paths: &BTreeMap<u32, PathBuf>,
    directory_ids: &BTreeMap<PathBuf, u32>,
) -> TestResult {
    for document in documents {
        let id = document.get_u32("file_id")?.expect("numeric file ID");
        let path = file_paths.get(&id).expect("registered file owner");
        assert_eq!(
            document.get_string("file_name")?.as_deref(),
            path.file_name().and_then(|name| name.to_str())
        );
        let mut ancestors = path
            .ancestors()
            .skip(1)
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| directory_ids[parent])
            .collect::<Vec<_>>();
        ancestors.reverse();
        assert!(document.has_field("ancestor_directory_ids"));
        assert!(!document.is_field_null("ancestor_directory_ids"));
        // zvec-rust's pointer getter represents an empty array as None.
        assert_eq!(
            document
                .get_array_u32("ancestor_directory_ids")?
                .unwrap_or_default(),
            ancestors,
            "{}: root files have no ancestors; nested files retain the complete ancestry",
            path.display()
        );
    }
    Ok(())
}

fn source_name(source: &Value) -> String {
    source["value"]["relative_path"]["value"]
        .as_str()
        .expect("UTF-8 source path")
        .to_owned()
}

async fn search_paths(
    engine: &ZvecGrep,
    root: &Path,
    mode: ContextRouteMode,
    selected_paths: Vec<String>,
) -> TestResult<BTreeSet<String>> {
    Ok(engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode,
                query: "orchard".to_owned(),
            }],
            limit: Some(TOTAL_FILES),
            filter: QueryFilter {
                // These are concrete fixture file names; anchor them to the root.
                globs: selected_paths
                    .into_iter()
                    .map(|path| format!("/{}", path.replace(std::path::MAIN_SEPARATOR, "/")).into())
                    .collect(),
                ..QueryFilter::default()
            },
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
                .expect("UTF-8 fixture path")
                .to_owned()
        })
        .collect())
}

struct CrashFixtureChild(Child);

impl Drop for CrashFixtureChild {
    fn drop(&mut self) {
        // Reap the child even if readiness or an assertion fails.
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
                "checkpoint_crash_fixture",
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
                "crash fixture exited before termination: {status}\n{}",
                fs::read_to_string(log_path).unwrap_or_default()
            );
        }
        // The small readiness file may still be in the middle of being written.
        if let Ok(bytes) = fs::read(&ready)
            && let Ok(path) = serde_json::from_slice(&bytes)
        {
            return Ok(path);
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for crash fixture\n{}",
            fs::read_to_string(log_path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
