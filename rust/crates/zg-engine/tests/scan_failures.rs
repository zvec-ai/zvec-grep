#![cfg(unix)]

mod support;

use std::{
    collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::atomic::Ordering,
};

use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::{IndexOptions, options::ScanRulesUpdate},
    },
};

#[tokio::test]
async fn incomplete_source_scan_preserves_existing_index_and_search_results()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let restricted = root.join("restricted");
    fs::create_dir(&restricted)?;
    fs::write(
        root.join("visible.txt"),
        "Orchard apples in the public garden.",
    )?;
    fs::write(
        restricted.join("hidden.txt"),
        "Orchard pears in the private garden.",
    )?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(root)).await?;
    assert_eq!(indexed.files_added, 2);
    assert_eq!(server.inputs.load(Ordering::Acquire), 2);
    let requests = server.requests.load(Ordering::Acquire);
    let info = engine.info(info_options(root)).await?;
    let before = native_file_records(&info.index_path)?;
    assert_eq!(before.len(), 2);

    let permissions = fs::metadata(&restricted)?.permissions();
    // Preserve traversal/stat access while denying directory enumeration.
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o111))?;
    if fs::read_dir(&restricted).is_ok() {
        fs::set_permissions(&restricted, permissions)?;
        eprintln!(
            "Skipping permission failure injection: this user can enumerate restricted directories."
        );
        engine.close();
        return Ok(());
    }
    let update = engine.index(index_options(root)).await;
    fs::set_permissions(&restricted, permissions)?;
    let error = update.expect_err("an incomplete source scan must fail the index update");
    assert!(error.to_string().contains("read directory"), "{error}");
    assert!(error.to_string().contains("restricted"), "{error}");
    assert_eq!(server.requests.load(Ordering::Acquire), requests);

    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            auto_update: false,
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            PathBuf::from("visible.txt"),
            PathBuf::from("restricted/hidden.txt")
        ])
    );
    engine.close();
    assert_eq!(native_file_records(&info.index_path)?, before);
    Ok(())
}

#[tokio::test]
async fn inaccessible_sources_allow_stored_queries_and_exclusion_updates()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let restricted = root.join("restricted");
    fs::create_dir(&restricted)?;
    fs::write(
        root.join("visible.txt"),
        "Orchard apples in the public garden.",
    )?;
    fs::write(
        restricted.join("hidden.txt"),
        "Orchard pears in the private garden.",
    )?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let engine = ZvecGrep::new();
    assert_eq!(engine.index(index_options(root)).await?.files_added, 2);
    let requests = server.requests.load(Ordering::Acquire);

    let permissions = fs::metadata(&restricted)?.permissions();
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o000))?;
    if fs::symlink_metadata(restricted.join("hidden.txt")).is_ok() {
        fs::set_permissions(&restricted, permissions)?;
        eprintln!(
            "Skipping permission failure injection: this user can inspect restricted sources."
        );
        engine.close();
        return Ok(());
    }
    let query = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            auto_update: false,
            ..ContextOptions::default()
        })
        .await;
    let update = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["!restricted".into()]),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await;
    fs::set_permissions(&restricted, permissions)?;
    engine.close();

    assert_eq!(
        query?
            .items
            .into_iter()
            .map(|item| item.relative_path)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            PathBuf::from("visible.txt"),
            PathBuf::from("restricted/hidden.txt"),
        ])
    );
    let update = update?;
    assert_eq!(
        (
            update.files_added,
            update.files_modified,
            update.files_deleted,
            update.files_failed
        ),
        (0, 0, 1, 0)
    );
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    let records = native_file_records(&root.join(".zvec-grep/index"))?;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["relative_path"]["components"],
        serde_json::json!(["visible.txt"])
    );
    Ok(())
}

#[tokio::test]
async fn excluded_symlink_loops_allow_indexing_status_and_refresh()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(root.join("visible.txt"), "Orchard apples in the garden.")?;
    std::os::unix::fs::symlink("private", root.join("private"))?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["!private".into()]),
                follow_symlinks: Some(true),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (1, 0));
    assert!(engine.info(info_options(root)).await?.status.is_some());

    fs::write(
        root.join("visible.txt"),
        "Orchard pears in the updated garden.",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!((updated.files_modified, updated.files_failed), (1, 0));
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "pears".into(),
            }],
            auto_update: true,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(
        result
            .items
            .into_iter()
            .map(|item| item.relative_path)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([PathBuf::from("visible.txt")])
    );

    let error = engine
        .index(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(Vec::new()),
                ..ScanRulesUpdate::default()
            },
            ..index_options(root)
        })
        .await
        .expect_err("selected symlink loops must still fail the scan");
    assert!(error.to_string().contains("inspect link target"), "{error}");
    assert!(error.to_string().contains("private"), "{error}");
    let info = engine.info(info_options(root)).await?;
    assert_eq!(
        info.workspace_index.expect("workspace").scan.globs(),
        &["!private".into()]
    );
    engine.close();
    let records = native_file_records(&info.index_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["relative_path"]["components"],
        serde_json::json!(["visible.txt"])
    );
    Ok(())
}
