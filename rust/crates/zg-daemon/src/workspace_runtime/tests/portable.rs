use super::*;

use zg_engine::api::{
    context::{
        ContextOptions,
        options::{ContextRoute, ContextRouteMode, RefreshPolicy},
    },
    index::options::{Device, EmbeddingModelSpec},
};
use zg_transport_mcp::IndexOperationProvider;

async fn empty_index(engine: &zg_engine::ZvecGrep, root: &std::path::Path, home: &std::path::Path) {
    engine
        .index(IndexOptions {
            root: Some(root.to_path_buf()),
            index_dir: Some(home.to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/text-embedding-v4".into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Cpu,
            }),
            endpoint: Some("http://127.0.0.1:1/empty-index".into()),
            api_key: Some("fixture-key".into()),
            allow_remote: true,
            ..IndexOptions::default()
        })
        .await
        .expect("empty index without model inference");
}

fn query(cwd: &std::path::Path, home: &std::path::Path) -> ContextOptions {
    ContextOptions {
        root: Some(cwd.to_path_buf()),
        index_dir: Some(home.to_path_buf()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "needle".into(),
        }],
        refresh: Some(RefreshPolicy::Off),
        ..ContextOptions::default()
    }
}

#[tokio::test]
async fn relocated_package_uses_saved_source_for_queries_and_background_work() {
    let directory = tempdir().expect("package parent");
    let parent = directory.path().canonicalize().expect("parent");
    let original = parent.join("original");
    std::fs::create_dir_all(original.join("materials")).expect("sources");
    let builder = zg_engine::ZvecGrep::new();
    empty_index(
        &builder,
        &original.join("materials"),
        &original.join("index"),
    )
    .await;
    builder.close();
    let moved = parent.join("moved");
    std::fs::rename(original, &moved).expect("move complete package while closed");
    let root = moved.join("materials");
    let home = moved.join("index");
    let unrelated = parent.join("unrelated");
    std::fs::create_dir(&unrelated).expect("caller directory");
    std::fs::write(unrelated.join("probe.txt"), "needle").expect("direct search fixture");

    let engine = zg_engine::ZvecGrep::new();
    let executor = Arc::new(RecordingExecutor::default());
    let (sender, receiver) = mpsc::channel(8);
    let watchers = Arc::new(ManualWatcherFactory {
        receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
        watches: Mutex::new(Vec::new()),
        closes: Arc::new(AtomicUsize::new(0)),
    });
    let manager = WorkspaceRuntimeManager::new(
        executor.clone(),
        watchers.clone(),
        SchedulerConfig::default(),
    );
    let mut request = query(&unrelated, &home);
    let reply = manager
        .search(&engine, request.clone())
        .await
        .expect("query moved index");
    assert_eq!(reply.root, root);
    assert!(reply.items.is_empty());

    request.refresh = Some(RefreshPolicy::Wait);
    let reply = manager
        .search(&engine, request.clone())
        .await
        .expect("refresh moved index");
    assert_eq!(reply.root, root);
    assert_eq!(
        executor.calls.lock().expect("calls")[0].root,
        Some(root.clone())
    );
    assert_eq!(watchers.watches.lock().expect("watches")[0].root.path, root);

    manager
        .submit_index(
            IndexOptions {
                index_dir: Some(home.clone()),
                ..IndexOptions::default()
            },
            true,
        )
        .await
        .expect("manual update with saved source");
    let calls = executor.calls.lock().expect("calls").clone();
    assert!(calls.iter().all(|call| call.root.as_ref() == Some(&root)));
    assert_eq!(
        calls.last().expect("manual update").changes,
        [IndexChange::Rescan]
    );

    std::fs::rename(&root, moved.join("renamed-materials"))
        .expect("break relative source relation");
    for refresh in [
        RefreshPolicy::Off,
        RefreshPolicy::Background,
        RefreshPolicy::Wait,
    ] {
        request.refresh = Some(refresh);
        manager
            .search(&engine, request.clone())
            .await
            .expect_err("indexed query requires source");
    }
    let reply = manager
        .search(
            &engine,
            ContextOptions {
                root: Some(unrelated),
                index_dir: Some(home),
                query: Some("needle".into()),
                rg: true,
                ..ContextOptions::default()
            },
        )
        .await
        .expect("direct search ignores unavailable indexed source");
    assert_eq!(reply.items.len(), 1);
    assert_eq!(
        reply.items[0].relative_path,
        std::path::Path::new("probe.txt")
    );
    drop(sender);
    manager.shutdown_all().await.expect("shutdown");
    engine.close();
}

#[tokio::test]
async fn changed_source_relationship_cannot_bypass_active_runtime_with_refresh_off() {
    let directory = tempdir().expect("package");
    let parent = directory.path().canonicalize().expect("parent");
    let root = parent.join("materials");
    let replacement = parent.join("replacement");
    let home = parent.join("index");
    std::fs::create_dir(&root).expect("source");
    std::fs::create_dir(&replacement).expect("replacement source");
    let engine = zg_engine::ZvecGrep::new();
    empty_index(&engine, &root, &home).await;
    let (_sender, receiver) = mpsc::channel(1);
    let manager = WorkspaceRuntimeManager::new(
        Arc::new(RecordingExecutor::default()),
        Arc::new(ManualWatcherFactory {
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
            watches: Mutex::new(Vec::new()),
            closes: Arc::new(AtomicUsize::new(0)),
        }),
        SchedulerConfig::default(),
    );
    drop(
        manager
            .runtime(
                &root,
                &IndexOptions {
                    index_dir: Some(home.clone()),
                    ..IndexOptions::default()
                },
            )
            .expect("active original runtime"),
    );
    let manifest = home.join("manifest.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).expect("manifest")).expect("json");
    data["sourceRoot"] = "../replacement".into();
    std::fs::write(manifest, serde_json::to_vec(&data).expect("json"))
        .expect("simulate external source change");
    let error = manager
        .search(&engine, query(&parent, &home))
        .await
        .expect_err("old runtime blocks rebind");
    assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    assert!(error.to_string().contains("close that workspace"));
    manager.shutdown_all().await.expect("shutdown");
    engine.close();
}
