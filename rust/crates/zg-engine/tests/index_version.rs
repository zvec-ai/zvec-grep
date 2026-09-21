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
        name: Some(format!("index-version-{}", uuid::Uuid::new_v4())),
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

fn assert_rebuild_error(error: &EngineError) {
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE, "{error}");
    assert!(error.message().contains("rebuild"), "{error}");
    assert!(error.message().contains("zg index --rebuild"), "{error}");
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
        assert!(!reason.is_empty());
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
        assert_rebuild_error(&error);
        assert_manifest_unchanged(root, manifest)?;
    }
    let error = engine
        .index(IndexOptions {
            rebuild: false,
            ..options.clone()
        })
        .await
        .expect_err("ordinary index must require an explicit rebuild");
    assert_rebuild_error(&error);
    assert_manifest_unchanged(root, manifest)?;
    let error = engine
        .watch_workspace(root, &TaskControl::default())
        .await
        .err()
        .expect("watch must reject incompatible data before starting");
    assert_rebuild_error(&error);
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
    assert!(manifest.get("manifestVersion").is_none());
    assert!(manifest["storageGeneration"].as_str().is_some());
    Ok(manifest)
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
        Some(json!(5)),
        None,
        Some(Value::Null),
    ] {
        let generation = current["storageGeneration"]
            .as_str()
            .expect("current generation")
            .to_owned();
        let old_storage = root.join(".zvec-grep/generations").join(&generation);
        let old_records = native_file_records(&old_storage.join("storage"))?;
        let actual_version = version
            .as_ref()
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).expect("fixture version"));
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
        assert_eq!(
            native_file_records(&old_storage.join("storage"))?,
            old_records
        );

        let rebuilt = engine
            .index(IndexOptions {
                rebuild: true,
                ..options.clone()
            })
            .await?;
        assert_eq!((rebuilt.files_added, rebuilt.files_failed), (1, 0));
        current = assert_current_index(&engine, root).await?;
        assert_ne!(current["storageGeneration"], generation);
        assert!(!old_storage.exists(), "superseded generation is removed");
    }
    engine.drop_index(info_options(root)).await?;
    engine.close();
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

fn assert_node_index_preserved(root: &Path, manifest: &[u8]) -> TestResult {
    assert_manifest_unchanged(root, manifest)?;
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
async fn rebuilding_node_index_preserves_old_data_until_successful_publication() -> TestResult {
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
    assert_node_index_preserved(root, &manifest)?;

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
    assert_node_index_preserved(root, &manifest)?;
    assert!(!home.join("build.json").exists());

    // Fail stage creation after the ownership record is written, without touching
    // either legacy collection or its active manifest.
    fs::remove_dir(home.join("generations"))?;
    fs::write(
        home.join("generations"),
        "cannot create a generation below a file",
    )?;
    let failed = engine
        .index(options.clone())
        .await
        .expect_err("failed stage creation must not publish");
    assert_ne!(failed.code(), EngineError::CANCELLED);
    assert_node_index_preserved(root, &manifest)?;
    assert_eq!(server.requests.load(Ordering::Acquire), 0);
    fs::remove_file(home.join("generations"))?;

    let rebuilt = engine.index(options.clone()).await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (1, 0));
    assert_eq!(server.inputs.load(Ordering::Acquire), 1);
    let current = assert_current_index(&engine, root).await?;
    assert_eq!(current["embeddings"][0]["model"]["provider"], "qwen");
    assert_eq!(
        current["embeddings"][0]["model"]["name"],
        "text-embedding-v4"
    );
    assert!(current.get("rootPaths").is_none());
    assert!(home.join("generations").is_dir());
    for directory in ["files.zvec", "index.zvec"] {
        assert!(!home.join(directory).exists());
    }
    assert_eq!(fs::read(home.join("authorization.json"))?, b"[]\n");
    assert!(!home.join("build.json").exists());
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}
