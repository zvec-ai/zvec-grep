#[allow(dead_code)]
mod support;

use std::{fs, net::SocketAddr, path::Path, sync::atomic::Ordering};

use serde_json::{Value, json};
use support::{EmbeddingServer, index_options, info_options, native_file_records};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::{
            IndexOptions,
            options::{Device, EmbeddingModelSpec},
        },
        info::{
            InfoOptions,
            result::{IndexCompatibility, IndexStatus},
        },
    },
    authorization::index_authorizations,
};
use zg_host_native::TaskControl;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn explicit_options(root: &Path, address: SocketAddr) -> IndexOptions {
    IndexOptions {
        embedding: Some(EmbeddingModelSpec {
            reference: "qwen/text-embedding-v4".into(),
            revision: None,
            cache_dir: None,
            endpoint: None,
            device: Device::Cpu,
        }),
        api_key: Some("local-test-key".into()),
        endpoint: Some(format!("http://{address}/embeddings")),
        ..index_options(root)
    }
}

fn assert_version_diagnostic(diagnostic: &str, actual_version: Option<u32>) {
    if let Some(version) = actual_version {
        assert!(
            diagnostic.contains(&format!("unsupported index version {version}; expected 2")),
            "{diagnostic}"
        );
        assert!(
            !diagnostic.contains("invalid workspace manifest"),
            "{diagnostic}"
        );
    } else {
        assert!(
            diagnostic.contains("invalid workspace manifest"),
            "{diagnostic}"
        );
        assert!(
            !diagnostic.contains("unsupported index version"),
            "{diagnostic}"
        );
    }
}

fn assert_rebuild_error(error: &EngineError, actual_version: Option<u32>) {
    assert_eq!(error.code(), EngineError::INDEX_INCOMPATIBLE, "{error}");
    assert_eq!(
        error.message(),
        "This index is incompatible with the current version of zg."
    );
    let help = error.help().expect("rebuild guidance");
    assert!(help.contains("zg --index --rebuild"));
    assert_version_diagnostic(help, actual_version);
}

fn assert_manifest_unchanged(root: &Path, expected: &[u8]) -> TestResult {
    assert_eq!(fs::read(root.join(".zvec-grep/manifest.json"))?, expected);
    Ok(())
}

async fn assert_incompatible_operations(
    engine: &ZvecGrep,
    options: &IndexOptions,
    actual_version: Option<u32>,
    manifest: &[u8],
) -> TestResult {
    let root = options.root.as_deref().expect("explicit root");
    for include_status in [false, true] {
        let info = engine
            .info(InfoOptions {
                root: Some(root.to_path_buf()),
                include_status,
                include_counts: false,
            })
            .await?;
        assert_eq!(info.index_status(), IndexStatus::RebuildRequired);
        assert!(info.status.is_none(), "incompatible storage is not scanned");
        let IndexCompatibility::RebuildRequired {
            actual_version: actual,
            expected_version,
            reason,
        } = info.compatibility
        else {
            panic!("expected rebuild-required compatibility");
        };
        assert_eq!(actual, actual_version);
        assert_eq!(expected_version, 2);
        assert_version_diagnostic(&reason, actual_version);
        assert_manifest_unchanged(root, manifest)?;
    }
    for auto_update in [false, true] {
        let error = engine
            .context(ContextOptions {
                root: Some(root.to_path_buf()),
                routes: vec![ContextRoute {
                    mode: ContextRouteMode::Fts,
                    query: "orchard".into(),
                }],
                auto_update,
                allow_remote: true,
                ..ContextOptions::default()
            })
            .await
            .expect_err("context must not use or refresh incompatible data");
        assert_rebuild_error(&error, actual_version);
        assert_manifest_unchanged(root, manifest)?;
    }
    let error = engine
        .index(IndexOptions {
            rebuild: false,
            ..options.clone()
        })
        .await
        .expect_err("ordinary index must require an explicit rebuild");
    assert_rebuild_error(&error, actual_version);
    assert_manifest_unchanged(root, manifest)?;
    let error = engine
        .watch_workspace(root, &TaskControl::default())
        .await
        .err()
        .expect("watch must reject incompatible data before starting");
    assert_rebuild_error(&error, actual_version);
    assert_manifest_unchanged(root, manifest)
}

async fn assert_current_index(engine: &ZvecGrep, root: &Path) -> TestResult<Value> {
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    assert_eq!(
        info.compatibility,
        IndexCompatibility::Compatible { version: 2 }
    );
    assert_eq!(info.index_status(), IndexStatus::Ready);
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join(".zvec-grep/manifest.json"))?)?;
    assert_eq!(manifest["indexVersion"], 2);
    assert!(manifest["sourceRoot"].get("relative").is_none());
    assert_eq!(manifest["index"]["state"], "enabled");
    for field in [
        "format",
        "manifestVersion",
        "indexPolicy",
        "root",
        "path",
        "name",
    ] {
        assert!(manifest.get(field).is_none());
    }
    assert!(manifest["storageGeneration"].as_str().is_some());
    Ok(manifest)
}

#[tokio::test]
async fn v2_without_graph_setting_supports_search_refresh_and_watch() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    let options = explicit_options(root, server.address);
    fs::write(root.join("note.txt"), "Orchard documentation.")?;
    let engine = ZvecGrep::new();
    engine.index(options.clone()).await?;
    let mut manifest = assert_current_index(&engine, root).await?;
    let generation = manifest["storageGeneration"].clone();
    // Exercise the manifest shape emitted by 0.3.0; the storage format is unchanged.
    manifest["index"]["descriptor"]
        .as_object_mut()
        .expect("descriptor")
        .remove("graphIndexing");
    let original = serde_json::to_vec(&manifest)?;
    fs::write(root.join(".zvec-grep/manifest.json"), &original)?;

    let query = |word: &str| ContextOptions {
        root: Some(root.to_path_buf()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: word.into(),
        }],
        auto_update: false,
        ..ContextOptions::default()
    };
    assert!(!engine.context(query("orchard")).await?.items.is_empty());
    assert_manifest_unchanged(root, &original)?;
    let watcher = engine
        .watch_workspace(root, &TaskControl::default())
        .await?;
    watcher.close().await?;

    let requests = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(options.clone()).await?;
    assert_eq!(unchanged.files_unchanged, 1);
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    fs::write(root.join("note.txt"), "Vineyard replacement documentation.")?;
    let updated = engine.index(options).await?;
    assert_eq!(updated.files_modified, 1);
    assert!(!engine.context(query("vineyard")).await?.items.is_empty());
    let manifest = assert_current_index(&engine, root).await?;
    assert_eq!(manifest["storageGeneration"], generation);
    assert_eq!(manifest["index"]["descriptor"]["graphIndexing"], false);
    assert!(!root.join(".zvec-grep/index/graph").exists());
    engine.close();
    Ok(())
}

#[tokio::test]
async fn incompatible_versions_require_explicit_rebuild_across_public_operations() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    let options = explicit_options(root, server.address);
    fs::write(root.join("note.txt"), "Orchard documentation.")?;
    let engine = ZvecGrep::new();
    engine.index(options.clone()).await?;
    let mut current = assert_current_index(&engine, root).await?;
    for version in [
        Some(json!(0)),
        Some(json!(1)),
        Some(json!(3)),
        Some(json!(4)),
        Some(json!(5)),
        None,
        Some(Value::Null),
        Some(json!("2")),
        Some(json!(true)),
        Some(json!(-1)),
        Some(json!(4.5)),
        Some(json!(u64::from(u32::MAX) + 1)),
    ] {
        let generation = current["storageGeneration"]
            .as_str()
            .expect("current generation")
            .to_owned();
        let index_path = root.join(".zvec-grep/index");
        let old_records = native_file_records(&index_path)?;
        let actual_version = version
            .as_ref()
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        match version {
            Some(value) => current["indexVersion"] = value,
            None => {
                current
                    .as_object_mut()
                    .expect("manifest")
                    .remove("indexVersion");
            }
        }
        let incompatible = serde_json::to_vec(&current)?;
        fs::write(root.join(".zvec-grep/manifest.json"), &incompatible)?;
        let requests = server.requests.load(Ordering::Acquire);
        assert_incompatible_operations(&engine, &options, actual_version, &incompatible).await?;
        assert_eq!(server.requests.load(Ordering::Acquire), requests);
        assert_eq!(native_file_records(&index_path)?, old_records);
        assert!(!root.join(".zvec-grep/temp/build.json").exists());
        assert!(!root.join(".zvec-grep/temp/new").exists());
        assert!(!root.join(".zvec-grep/temp/old").exists());

        let rebuilt = engine
            .index(IndexOptions {
                rebuild: true,
                ..options.clone()
            })
            .await?;
        assert_eq!((rebuilt.files_added, rebuilt.files_failed), (1, 0));
        current = assert_current_index(&engine, root).await?;
        assert_ne!(current["storageGeneration"], generation);
        assert!(index_path.is_dir(), "rebuilt index retains the fixed path");
        assert!(!root.join(".zvec-grep/temp/old").exists());
        assert!(!root.join(".zvec-grep/temp/new").exists());
    }
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn unbuilt_workspaces_require_a_valid_current_version_before_initialization() -> TestResult {
    for (state, expected_status) in [
        ("uninitialized", IndexStatus::Uninitialized),
        ("disabled", IndexStatus::Disabled),
    ] {
        let temporary = tempdir()?;
        let root = temporary.path();
        let home = root.join(".zvec-grep");
        fs::create_dir(&home)?;
        fs::write(root.join("note.txt"), "Orchard documentation.")?;
        fs::write(home.join("preserved"), b"unrelated workspace data")?;
        let current = json!({
            "sourceRoot": {
                "recordedPath": fs::canonicalize(root)?.to_string_lossy(),
                "recordedUsername": std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok(),
            },
            "scan": {},
            "indexVersion": 2,
            "createdTime": 0,
            "updatedTime": 0,
            "index": { "state": state },
        });
        fs::write(home.join("manifest.json"), serde_json::to_vec(&current)?)?;
        let server = EmbeddingServer::start()?;
        let options = explicit_options(root, server.address);
        let engine = ZvecGrep::new();
        let info = engine.info(info_options(root)).await?;
        assert_eq!(info.compatibility, IndexCompatibility::Unbuilt);
        assert_eq!(info.index_status(), expected_status);
        assert_eq!(
            info.workspace_index.expect("workspace").index_version,
            Some(2)
        );
        assert_eq!(server.requests.load(Ordering::Acquire), 0);

        for version in [None, Some(Value::Null), Some(json!("2")), Some(json!(3))] {
            let mut incompatible = current.clone();
            let actual_version = version
                .as_ref()
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            match version {
                Some(value) => incompatible["indexVersion"] = value,
                None => {
                    incompatible
                        .as_object_mut()
                        .expect("manifest")
                        .remove("indexVersion");
                }
            }
            let manifest = serde_json::to_vec(&incompatible)?;
            fs::write(home.join("manifest.json"), &manifest)?;
            assert_incompatible_operations(&engine, &options, actual_version, &manifest).await?;
            assert_eq!(server.requests.load(Ordering::Acquire), 0);
            assert_eq!(
                fs::read(home.join("preserved"))?,
                b"unrelated workspace data"
            );
            assert!(!home.join("index").exists());
            assert!(!home.join("temp").exists());
        }

        // A current uninitialized manifest follows ordinary initialization; an
        // incompatible disabled manifest can only be replaced by explicit rebuild.
        let rebuild = state == "disabled";
        if !rebuild {
            fs::write(home.join("manifest.json"), serde_json::to_vec(&current)?)?;
        }
        let indexed = engine.index(IndexOptions { rebuild, ..options }).await?;
        assert_eq!((indexed.files_added, indexed.files_failed), (1, 0));
        assert_current_index(&engine, root).await?;
        assert_eq!(
            fs::read(home.join("preserved"))?,
            b"unrelated workspace data"
        );
        engine.drop_index(info_options(root)).await?;
        engine.close();
    }
    Ok(())
}

fn write_node_index(root: &Path) -> TestResult<Vec<u8>> {
    let home = root.join(".zvec-grep");
    fs::create_dir_all(&home)?;
    let manifest = serde_json::to_vec(&json!({
        "manifestVersion": 1,
        "id": "legacy-index",
        "name": "legacy-workspace",
        "path": home,
        "rootPaths": [{"absolutePath": root, "recursive": true}],
        "indexPolicy": "enabled",
        "indexVersion": 1,
        "embedding": {"provider": "legacy", "model": "retired", "dimension": 3, "metric": "cosine"},
        "embeddingRuntime": {"apiKey": "retired-key", "endpoint": "http://127.0.0.1:9/retired"},
        "createdTime": 1,
        "updatedTime": 1,
    }))?;
    fs::write(home.join("manifest.json"), &manifest)?;
    for directory in ["files.zvec", "index.zvec"] {
        fs::create_dir(home.join(directory))?;
        fs::write(home.join(directory).join("sentinel"), directory)?;
    }
    fs::write(home.join("authorization.json"), b"[]\n")?;
    Ok(manifest)
}

fn assert_node_files_preserved(root: &Path) -> TestResult {
    let home = root.join(".zvec-grep");
    for directory in ["files.zvec", "index.zvec"] {
        assert_eq!(
            fs::read_to_string(home.join(directory).join("sentinel"))?,
            directory
        );
    }
    assert_eq!(fs::read(home.join("authorization.json"))?, b"[]\n");
    Ok(())
}

#[tokio::test]
async fn rebuilding_node_index_preserves_unrelated_files() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let home = root.join(".zvec-grep");
    let manifest = write_node_index(root)?;
    fs::write(root.join("note.txt"), "Orchard replacement index.")?;
    let server = EmbeddingServer::start()?;
    let options = IndexOptions {
        rebuild: true,
        ..explicit_options(root, server.address)
    };
    let engine = ZvecGrep::new();
    assert_incompatible_operations(&engine, &options, Some(1), &manifest).await?;
    let authorizations = index_authorizations(&IndexOptions {
        allow_remote: false,
        ..options.clone()
    })?;
    assert_eq!(authorizations.len(), 1);
    assert_eq!(authorizations[0].root, fs::canonicalize(root)?);
    assert_eq!(authorizations[0].model, "qwen/text-embedding-v4");
    assert_eq!(Some(&authorizations[0].endpoint), options.endpoint.as_ref());
    assert_eq!(server.requests.load(Ordering::Acquire), 0);
    assert_manifest_unchanged(root, &manifest)?;
    assert_node_files_preserved(root)?;

    let signal = CancellationToken::new();
    signal.cancel();
    let cancelled = engine
        .index(IndexOptions {
            signal: Some(signal),
            ..options.clone()
        })
        .await
        .expect_err("cancelled rebuild must not publish");
    assert_eq!(cancelled.code(), EngineError::CANCELLED);
    assert_manifest_unchanged(root, &manifest)?;
    assert_node_files_preserved(root)?;
    assert!(!home.join("temp/build.json").exists());
    assert!(
        !home.join("temp/new").exists(),
        "cancelled admission must not prepare staging storage"
    );

    // Invalid staging paths must not affect the legacy index.
    fs::write(home.join("temp"), "cannot stage an index below a file")?;
    let failed = engine
        .index(options.clone())
        .await
        .expect_err("failed stage creation must not publish");
    assert_ne!(failed.code(), EngineError::CANCELLED);
    assert_manifest_unchanged(root, &manifest)?;
    assert_node_files_preserved(root)?;
    assert_eq!(server.requests.load(Ordering::Acquire), 0);
    fs::remove_file(home.join("temp"))?;

    let rebuilt = engine.index(options.clone()).await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (1, 0));
    assert_eq!(server.inputs.load(Ordering::Acquire), 1);
    let current = assert_current_index(&engine, root).await?;
    assert_eq!(
        current["index"]["descriptor"]["defaultEmbeddingModel"]["model"]["provider"],
        "qwen"
    );
    assert_eq!(
        current["index"]["descriptor"]["defaultEmbeddingModel"]["model"]["name"],
        "text-embedding-v4"
    );
    assert!(current.get("rootPaths").is_none());
    assert!(home.join("index").is_dir());
    assert_node_files_preserved(root)?;
    assert!(!home.join("temp/build.json").exists());
    engine.drop_index(info_options(root)).await?;
    assert_node_files_preserved(root)?;
    engine.close();
    Ok(())
}
