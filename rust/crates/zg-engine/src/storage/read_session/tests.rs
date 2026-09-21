use std::{fs, sync::Barrier};

use crate::{
    api::context::{
        ContextOptions,
        options::{ContextRoute, ContextRouteMode},
    },
    domain::{
        Content, Entity, EntityFragment, EntityId, FileIndexStatus, FileRecord, FileSnapshot,
        FragmentId, IndexDescriptor, IndexState, Range, SourcePath, Workspace,
        model::{EmbeddingModelInfo, Metric, ModelInfo},
    },
    models::ModelRuntimeManager,
    pipelines::{indexed_search::service::context, indexing::service::WorkspaceIndexService},
    storage::types::IndexedFragment,
    workspace::{
        CURRENT_INDEX_VERSION,
        lock::acquire_home_lock,
        manifest::{WorkspaceManifest, write_workspace_manifest},
    },
};

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    storage_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path().canonicalize().expect("canonical root");
        let home = root.join(".zvec-grep");
        let storage_home = home
            .join("generations")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&storage_home).expect("generation");
        let fixture = Self {
            _directory: directory,
            home,
            storage_home,
        };
        write_text(&fixture.home, &fixture.storage_home, "orchard");
        fixture
    }

    fn read(&self, cache: &ReadSessionCache) -> (FileLock, ReadSessionLease) {
        let home_lock =
            acquire_home_lock(&self.home, LockMode::Read, "test.query").expect("read lock");
        let lease = cache
            .acquire(&self.home, &self.storage_home)
            .expect("read session");
        (home_lock, lease)
    }

    fn publish(&self) -> (WorkspaceManifest, ContextOptions) {
        let root = self.home.parent().expect("workspace root");
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "cache-fixture".into(),
                root: root.to_path_buf(),
                scan: crate::domain::ScanRules::default(),
                index: IndexState::Enabled(IndexDescriptor::single(schema())),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            self.home.clone(),
            Some(CURRENT_INDEX_VERSION),
            std::collections::BTreeMap::new(),
        )
        .expect("manifest");
        manifest.storage_generation = Some(
            self.storage_home
                .file_name()
                .expect("generation")
                .to_string_lossy()
                .into_owned(),
        );
        write_workspace_manifest(&self.home, &manifest).expect("publish manifest");
        let options = ContextOptions {
            root: Some(root.to_path_buf()),
            auto_update: false,
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            ..ContextOptions::default()
        };
        (manifest, options)
    }
}

fn schema() -> EmbeddingModelInfo {
    EmbeddingModelInfo {
        model: ModelInfo {
            provider: "fixture".into(),
            name: "read-cache".into(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    }
}

fn write_text(home: &Path, storage_home: &Path, text: &str) {
    let _lock =
        acquire_home_lock(home, LockMode::Write, "test.write").expect("writer drains cache");
    let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: storage_home.to_path_buf(),
        embeddings: vec![schema()],
    })
    .expect("writable storage");
    let relative_path = SourcePath::new("source.txt").expect("source path");
    let id = storage
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("file ID")[0];
    fs::write(home.parent().expect("root").join("source.txt"), text).expect("source file");
    let file = FileRecord {
        id,
        relative_path,
        snapshot: FileSnapshot {
            size_bytes: text.len() as u64,
            modified_epoch_ms: None,
            content_hash: Some(crate::utils::sha256_hex(text.as_bytes())),
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    let content = Content::Text(text.into());
    let entity_id = EntityId::new(id, &content, Range::Full).expect("entity ID");
    let fragment_id = FragmentId::new(&entity_id, 0);
    let entity = Entity {
        id: entity_id.clone(),
        file_id: id,
        content,
        source_range: Range::Full,
        metadata: None,
        fragments: vec![EntityFragment {
            id: fragment_id.clone(),
            range: Range::Full,
        }],
    };
    storage
        .replace_file(
            &file,
            &[entity],
            &[IndexedFragment {
                entity_id,
                fragment_id,
                model: schema().model.reference(),
                vector: vec![1.0, 0.0, 0.0],
                fts_text: text.into(),
            }],
        )
        .expect("replace indexed text");
    storage.close().expect("checkpoint");
}

fn cache() -> ReadSessionCache {
    ReadSessionCache::new(Duration::from_secs(60)).expect("cache worker")
}

#[test]
fn sequential_and_concurrent_queries_reuse_the_session() {
    let fixture = Fixture::new();
    let cache = cache();
    let (home_lock, first) = fixture.read(&cache);
    let original = Arc::downgrade(&first.session);
    first.close().expect("return session");
    drop(home_lock);
    let barrier = Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let (_home_lock, lease) = fixture.read(&cache);
                barrier.wait();
                assert!(Weak::ptr_eq(&original, &Arc::downgrade(&lease.session)));
                assert_eq!(
                    lease
                        .storage()
                        .search_fts("orchard", 10, None)
                        .expect("search")
                        .len(),
                    1
                );
            });
        }
    });
    assert_eq!(
        original.strong_count(),
        1,
        "only the idle cache retains the handle"
    );
    cache.close();
    assert_eq!(original.strong_count(), 0);
}

#[test]
fn expiry_starts_at_final_release_and_protects_active_queries() {
    let fixture = Fixture::new();
    let cache = cache();
    let (home_lock, lease) = fixture.read(&cache);
    let session = Arc::downgrade(&lease.session);
    cache.inner.reap(Instant::now() + Duration::from_secs(120));
    assert!(
        lease.storage().list_files().is_ok(),
        "active query remains usable"
    );
    drop(lease);
    drop(home_lock);
    let released = Instant::now();
    cache.inner.reap(released + Duration::from_secs(59));
    assert_eq!(session.strong_count(), 1);
    cache.inner.reap(released + Duration::from_secs(61));
    assert_eq!(session.strong_count(), 0);
}

#[test]
fn concurrent_cache_misses_open_one_session() {
    let fixture = Fixture::new();
    let cache = cache();
    let barrier = Barrier::new(4);
    let sessions = std::thread::scope(|scope| {
        let readers = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    let (_home_lock, lease) = fixture.read(&cache);
                    let session = Arc::downgrade(&lease.session);
                    barrier.wait();
                    assert_eq!(lease.storage().list_files().expect("query files").len(), 1);
                    session
                })
            })
            .collect::<Vec<_>>();
        readers
            .into_iter()
            .map(|reader| reader.join().expect("reader"))
            .collect::<Vec<_>>()
    });
    assert!(
        sessions
            .iter()
            .all(|session| Weak::ptr_eq(session, &sessions[0]))
    );
}

#[test]
fn maintenance_expires_idle_handles_without_another_request() {
    let fixture = Fixture::new();
    let cache = ReadSessionCache::new(Duration::from_millis(20)).expect("cache worker");
    let (home_lock, lease) = fixture.read(&cache);
    let session = Arc::downgrade(&lease.session);
    drop(lease);
    drop(home_lock);
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.strong_count() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        session.strong_count(),
        0,
        "idle worker releases native storage"
    );
}

#[test]
fn maintenance_does_not_recreate_removed_lock_files() {
    let fixture = Fixture::new();
    let cache = cache();
    let (home_lock, lease) = fixture.read(&cache);
    let session = Arc::downgrade(&lease.session);
    drop(lease);
    drop(home_lock);
    let lock_file = fixture.home.join("locks/home");
    fs::remove_file(&lock_file).expect("remove workspace lock");
    cache.inner.reap(Instant::now());
    assert!(
        !lock_file.exists(),
        "maintenance must not recreate deleted workspace paths"
    );
    assert_eq!(session.strong_count(), 0);
}

#[test]
fn generation_replacement_and_shutdown_preserve_inflight_readers() {
    let fixture = Fixture::new();
    let replacement = fixture
        .home
        .join("generations")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&replacement).expect("replacement generation");
    write_text(&fixture.home, &replacement, "vineyard");
    let cache = cache();
    let (home_lock, first) = fixture.read(&cache);
    let previous = Arc::downgrade(&first.session);
    let second = cache
        .acquire(&fixture.home, &replacement)
        .expect("new generation");
    assert!(!Weak::ptr_eq(&previous, &Arc::downgrade(&second.session)));
    assert_eq!(
        first
            .storage()
            .search_fts("orchard", 10, None)
            .expect("old reader")
            .len(),
        1
    );
    assert_eq!(
        second
            .storage()
            .search_fts("vineyard", 10, None)
            .expect("new reader")
            .len(),
        1
    );
    cache.close();
    assert!(cache.acquire(&fixture.home, &replacement).is_err());
    assert!(second.storage().list_files().is_ok());
    assert!(acquire_home_lock(&fixture.home, LockMode::Write, "test.write").is_err());
    drop(first);
    drop(second);
    drop(home_lock);
    assert_eq!(previous.strong_count(), 0);
    let _writer =
        acquire_home_lock(&fixture.home, LockMode::Write, "test.write").expect("readers drained");
    IndexStore::delete(&fixture.storage_home).expect("drop old storage");
    IndexStore::delete(&replacement).expect("drop new storage");
}

#[test]
fn writes_invalidate_every_local_cache_and_next_query_sees_changes() {
    let fixture = Fixture::new();
    let first = cache();
    let second = cache();
    for cache in [&first, &second] {
        let (home_lock, lease) = fixture.read(cache);
        drop(lease);
        drop(home_lock);
    }
    write_text(&fixture.home, &fixture.storage_home, "vineyard");
    for cache in [&first, &second] {
        let (_home_lock, lease) = fixture.read(cache);
        assert!(
            lease
                .storage()
                .search_fts("orchard", 10, None)
                .expect("old text removed")
                .is_empty()
        );
        assert_eq!(
            lease
                .storage()
                .search_fts("vineyard", 10, None)
                .expect("new text visible")
                .len(),
            1
        );
    }
}

#[test]
fn failed_open_can_be_retried_without_leaking_a_residency_lock() {
    let fixture = Fixture::new();
    let cache = cache();
    let missing = fixture
        .home
        .join("generations")
        .join(uuid::Uuid::new_v4().to_string());
    let reader = acquire_home_lock(&fixture.home, LockMode::Read, "test.query").expect("reader");
    assert!(cache.acquire(&fixture.home, &missing).is_err());
    drop(reader);
    fs::create_dir_all(&missing).expect("new generation");
    write_text(&fixture.home, &missing, "vineyard");
    let _reader = acquire_home_lock(&fixture.home, LockMode::Read, "test.query").expect("reader");
    let lease = cache
        .acquire(&fixture.home, &missing)
        .expect("retry opens published storage");
    assert_eq!(
        lease
            .storage()
            .search_fts("vineyard", 10, None)
            .expect("query")
            .len(),
        1
    );
}

#[tokio::test]
async fn indexed_queries_reuse_storage_but_resolve_the_current_manifest() {
    let fixture = Fixture::new();
    let indexing = WorkspaceIndexService::with_test_registry();
    let models = ModelRuntimeManager::new();
    let cache = cache();
    let (mut manifest, options) = fixture.publish();
    let first = context(&indexing, &models, &options, Some(&cache))
        .await
        .expect("first query");
    assert_eq!(first.items.len(), 1);
    let session = Arc::downgrade(
        &lock(&lock(&cache.inner.state).entries[&fixture.home].entry)
            .as_ref()
            .expect("session")
            .session,
    );
    let second = context(&indexing, &models, &options, Some(&cache))
        .await
        .expect("warm query");
    assert_eq!(first.items, second.items);
    assert!(Weak::ptr_eq(
        &session,
        &Arc::downgrade(
            &lock(&lock(&cache.inner.state).entries[&fixture.home].entry)
                .as_ref()
                .expect("session")
                .session
        )
    ));
    assert_eq!(
        models.snapshot().cached_runtimes,
        0,
        "FTS does not acquire a model"
    );
    manifest.workspace.index = IndexState::Disabled;
    manifest.index_version = None;
    manifest.storage_generation = None;
    {
        let _writer =
            acquire_home_lock(&fixture.home, LockMode::Write, "test.disable").expect("writer");
        write_workspace_manifest(&fixture.home, &manifest).expect("disable index");
    }
    assert!(
        context(&indexing, &models, &options, Some(&cache))
            .await
            .is_err()
    );
    assert_eq!(session.strong_count(), 0);
}

#[tokio::test]
#[ignore = "manual comparison of uncached and resident query latency"]
async fn benchmark_warm_queries() {
    let fixture = Fixture::new();
    let (_, options) = fixture.publish();
    let indexing = WorkspaceIndexService::with_test_registry();
    let models = ModelRuntimeManager::new();
    let cache = cache();
    for (name, selected_cache) in [("uncached", None), ("resident", Some(&cache))] {
        for _ in 0..3 {
            context(&indexing, &models, &options, selected_cache)
                .await
                .expect("warmup");
        }
        let mut durations = Vec::new();
        for _ in 0..50 {
            let started = Instant::now();
            let result = context(&indexing, &models, &options, selected_cache)
                .await
                .expect("query");
            durations.push(started.elapsed());
            assert_eq!(result.items.len(), 1);
        }
        durations.sort_unstable();
        eprintln!(
            "{name}: 50 FTS queries, median={:?}, p95={:?}",
            durations[25], durations[47]
        );
    }
}

#[test]
fn cold_open_does_not_block_another_workspaces_cache_hit() {
    let cold = Fixture::new();
    let warm = Fixture::new();
    let cache = cache();
    let (home_lock, lease) = warm.read(&cache);
    drop(lease);
    drop(home_lock);
    let (started, opening) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let cache = &cache;
        scope.spawn(move || {
            let _home = acquire_home_lock(&cold.home, LockMode::Read, "cold").expect("reader");
            cache
                .acquire_with(&cold.home, &cold.storage_home, || {
                    started.send(()).expect("opening");
                    released.recv().expect("allow native open");
                    IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
                        storage_path: cold.storage_home.clone(),
                    })
                })
                .expect("cold query");
        });
        opening.recv().expect("cold open in progress");
        let (completed, completion) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let (_home, lease) = warm.read(cache);
            completed
                .send(
                    lease
                        .storage()
                        .search_fts("orchard", 10, None)
                        .expect("warm search")
                        .len(),
                )
                .expect("result");
        });
        let result = completion.recv_timeout(Duration::from_secs(2));
        release.send(()).expect("release cold open even on failure");
        assert_eq!(
            result.expect("warm cache hit must not wait for another workspace"),
            1
        );
    });
}
