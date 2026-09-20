mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_documents,
    native_file_records,
};

use serde_json::{Value, json};
use tempfile::tempdir;
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode, QueryFilter, SymbolType},
            result::{ContextItemStatus, EntityMetadata},
        },
        index::{
            IndexOptions,
            options::{ContentKind, Device, EmbeddingModelSpec, WorkspaceChange},
        },
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn content_routes_partition_canonical_fragments_into_one_index_per_model() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("note.txt"), "Orchard textual documentation.")?;
    let engine = ZvecGrep::new();
    engine
        .index(IndexOptions {
            scan: zg_engine::api::index::options::ScanRulesUpdate {
                globs: Some(vec!["*.txt".into(), "*.png".into()]),
                ..Default::default()
            },
            ..index_options(root)
        })
        .await?;
    let original = engine.info(info_options(root)).await?;
    assert_eq!(model_collections(&original.index_path)?.len(), 1);
    for name in ["directories", "files", "entities"] {
        assert!(original.index_path.join(name).is_dir());
    }
    // A complete PNG resource; the fixture provider embeds bytes without decoding them.
    fs::write(
        root.join("diagram.png"),
        [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 4, 0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15,
            0, 1, 5, 1, 1, 39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ],
    )?;
    let unsupported = engine.index(index_options(root)).await?;
    assert_eq!(unsupported.files_failed, 1);
    assert_eq!(unsupported.failed_files[0].path, Path::new("diagram.png"));
    assert_eq!(server.multimodal_inputs.load(Ordering::Acquire), 0);

    let spec = |reference: &str| EmbeddingModelSpec {
        reference: reference.to_owned(),
        revision: None,
        cache_dir: None,
        endpoint: Some(format!("http://{}/embeddings", server.address)),
        device: Device::Auto,
    };
    let routes = BTreeMap::from([
        (ContentKind::Text, spec("qwen/text-embedding-v4")),
        (ContentKind::Image, spec("qwen/qwen3-vl-embedding")),
    ]);
    let inputs = server.inputs.load(Ordering::Acquire);
    let error = engine
        .index(IndexOptions {
            embedding_routes: Some(routes.clone()),
            api_key: Some("local-test-key".into()),
            ..index_options(root)
        })
        .await
        .expect_err("model additions require explicit rebuild");
    assert!(error.message().contains("rebuild"), "{error}");
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs);
    assert_eq!(
        engine.info(info_options(root)).await?.index_path,
        original.index_path
    );

    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            embedding_routes: Some(routes),
            api_key: Some("local-test-key".into()),
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (2, 0));
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs + 2);
    assert_eq!(server.multimodal_inputs.load(Ordering::Acquire), 1);
    let published = engine.info(info_options(root)).await?;
    assert_ne!(published.index_path, original.index_path);
    let collections = model_collections(&published.index_path)?;
    assert_eq!(collections.len(), 2);
    assert!(!published.index_path.join("fragments").exists());
    assert!(!published.index_path.join("directories.json").exists());
    let mut indexed_ids = BTreeSet::new();
    for collection in collections {
        let documents = native_documents(&collection)?;
        assert_eq!(
            documents.len(),
            1,
            "each content belongs to exactly one model"
        );
        assert!(documents[0].get_string("text")?.is_some());
        assert!(
            indexed_ids.insert(
                documents[0]
                    .get_string("document_id")?
                    .expect("fragment ID")
            )
        );
    }
    let entities = native_documents(&published.index_path.join("entities"))?;
    assert_eq!(entities.len(), 2);
    let mut canonical_ids = BTreeSet::new();
    for entity in entities {
        let payload: Value =
            serde_json::from_str(&entity.get_string("payload")?.expect("entity payload"))?;
        for fragment in payload["value"]
            .as_array()
            .expect("canonical fragment bundle")
        {
            let id = fragment["value"]["id"].as_str().expect("fragment ID");
            assert!(canonical_ids.insert(hex::encode(id)));
        }
    }
    assert_eq!(canonical_ids, indexed_ids);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "image".into(),
            }],
            filter: QueryFilter {
                formats: vec![zg_engine::api::context::options::FileFormat::Png],
                ..QueryFilter::default()
            },
            auto_update: false,
            allow_remote: true,
            api_key: Some("local-test-key".into()),
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path, Path::new("diagram.png"));
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn public_symbol_filters_distinguish_all_categories_and_implementation_blocks() -> TestResult
{
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("symbols.rs"),
        r"/// orchard module
pub mod catalog {
    /// orchard class
    pub struct Model { pub count: u32 }
    /// orchard enum
    pub enum Status { Ready, Waiting }
    /// orchard interface
    pub trait Contract { fn execute(&self); }
    /// orchard alias
    pub type Handle = Model;
    /// orchard value
    pub const LIMIT: u32 = 10;
    /// orchard implementation
    impl Model {
        /// orchard function
        pub fn execute(&self) {}
    }
}
",
    )?;
    let engine = ZvecGrep::new();
    assert_eq!(engine.index(index_options(root)).await?.files_failed, 0);
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        for symbol_type in [
            SymbolType::Alias,
            SymbolType::Class,
            SymbolType::Enum,
            SymbolType::Function,
            SymbolType::Interface,
            SymbolType::Module,
            SymbolType::Value,
        ] {
            let result = engine
                .context(ContextOptions {
                    root: Some(root.to_path_buf()),
                    routes: vec![ContextRoute {
                        mode,
                        query: "orchard".into(),
                    }],
                    filter: QueryFilter {
                        symbol_types: vec![symbol_type],
                        ..QueryFilter::default()
                    },
                    limit: Some(30),
                    auto_update: false,
                    allow_remote: true,
                    ..ContextOptions::default()
                })
                .await?;
            assert!(!result.items.is_empty(), "{mode:?}: {symbol_type:?}");
            assert!(result.items.iter().all(|item| matches!(
                &item.metadata,
                Some(EntityMetadata::Code(metadata)) if metadata.symbol_type == Some(symbol_type)
            )));
            if symbol_type == SymbolType::Class {
                for signature in ["pub struct Model", "impl Model"] {
                    assert!(
                        result.items.iter().any(|item| matches!(
                            &item.metadata,
                            Some(EntityMetadata::Code(metadata))
                                if metadata.signature.as_deref() == Some(signature)
                        )),
                        "{mode:?}: {signature}"
                    );
                }
            }
        }
    }
    let unfiltered = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            limit: Some(30),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(unfiltered.items.iter().any(|item| matches!(
        &item.metadata,
        Some(EntityMetadata::Code(metadata))
            if metadata.signature.as_deref() == Some("impl Model")
                && metadata.symbol_type == Some(SymbolType::Class)
    )));
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_engine_persists_searches_updates_and_drops_real_storage() -> TestResult {
    let temporary = tempfile::Builder::new()
        .prefix("engine storage ")
        .tempdir()?;
    // Windows canonical paths have a verbatim prefix; the native boundary must handle it.
    let canonical_root = fs::canonicalize(temporary.path())?;
    let root = canonical_root.as_path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("auth.rs"),
        "/// Orchard authentication policy.\npub fn authenticate() -> bool { true }\n",
    )?;
    fs::write(
        root.join("billing.md"),
        "# Billing\n\nInvoices and monthly payments.\n",
    )?;
    fs::write(
        root.join("tmp"),
        "A readable extensionless office memorandum.\n",
    )?;
    fs::write(root.join("opaque"), [0_u8, 255, 0, 128])?;
    fs::write(root.join("state.sqlite"), b"SQLite format 3\0")?;
    fs::create_dir(root.join("nested"))?;

    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!(initial.files_added, 3, "{initial:?}");
    assert_eq!(initial.files_failed, 0);
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    assert!(info.index_path.exists());
    let fts = info
        .workspace_index
        .as_ref()
        .expect("workspace info")
        .fts
        .as_ref()
        .expect("FTS info");
    assert_eq!(fts.tokenizer, "jieba");
    assert_eq!(fts.filters, ["lowercase"]);
    assert_eq!(info.status.as_ref().expect("status").files_indexed, 3);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, &root.join("nested"), "invoices").await?,
        [PathBuf::from("billing.md")]
    );

    let hybrid = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            query: Some("orchard authentication".to_owned()),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        hybrid
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );
    let calls = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!(unchanged.files_unchanged, 3);
    assert_eq!(server.requests.load(Ordering::Acquire), calls);
    engine.close();
    assert_eq!(
        engine
            .info(info_options(root))
            .await
            .expect_err("closed engine")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    drop(engine);

    let engine = ZvecGrep::new();
    assert!(engine.info(info_options(root)).await?.indexed);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    let vector = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard authentication".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        vector
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    fs::write(
        root.join("auth.rs"),
        "/// Vineyard session renewal policy.\npub fn renew_session() -> bool { false }\n",
    )?;
    fs::remove_file(root.join("billing.md"))?;
    fs::write(
        root.join("support.txt"),
        "Customer support handles delivery inquiries.\n",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!(
        (
            updated.files_added,
            updated.files_modified,
            updated.files_deleted
        ),
        (1, 1, 1)
    );
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert!(fts_paths(&engine, root, "invoices").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, root, "delivery").await?,
        [PathBuf::from("support.txt")]
    );

    let before_rebuild = engine.info(info_options(root)).await?;
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!(rebuilt.files_added, 3);
    let after_rebuild = engine.info(info_options(root)).await?;
    assert_eq!(
        after_rebuild
            .workspace_index
            .as_ref()
            .expect("rebuilt workspace")
            .name,
        before_rebuild
            .workspace_index
            .as_ref()
            .expect("original workspace")
            .name,
    );
    assert_ne!(after_rebuild.index_path, before_rebuild.index_path);
    assert!(after_rebuild.index_path.is_dir());
    assert!(!before_rebuild.index_path.exists());
    let rebuilt_query = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "vineyard session renewal".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        rebuilt_query
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    assert!(engine.drop_index(info_options(root)).await?);
    assert!(!engine.drop_index(info_options(root)).await?);
    assert!(!info.index_path.exists());
    assert!(!after_rebuild.index_path.exists());
    assert!(!engine.info(info_options(root)).await?.indexed);
    assert!(root.join("auth.rs").is_file());

    configure_remote_model(root, server.address)?;
    engine.index(index_options(root)).await?;
    let orphaned_index_path = engine.info(info_options(root)).await?.index_path;
    assert!(orphaned_index_path.is_dir());
    fs::remove_file(root.join(".zvec-grep/manifest.json"))?;
    assert!(
        engine.drop_index(info_options(root)).await?,
        "orphaned backend can be removed"
    );
    assert!(!orphaned_index_path.exists());
    assert!(!info.index_path.exists());
    engine.close();
    Ok(())
}

#[tokio::test]
async fn initial_build_publishes_successes_and_incrementally_retries_failed_files() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let home = root.join(".zvec-grep");
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("broken.txt"), [255_u8, 254, 255])?;
    fs::write(root.join("stable.txt"), "Stable orchard baseline.\n")?;
    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!(initial.files_failed, 1);
    assert_eq!(initial.failed_files.len(), 1);
    assert_eq!(initial.failed_files[0].path, Path::new("broken.txt"));
    assert!(!initial.failed_files[0].reason.is_empty());
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    let status = info.status.expect("published status");
    assert_eq!((status.files_failed, status.files_indexed), (1, 1));
    assert_eq!(status.failed_files[0].path, Path::new("broken.txt"));
    assert!(!home.join("build.json").exists());
    assert_eq!(fs::read_dir(home.join("generations"))?.count(), 1);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("stable.txt")]
    );
    let inputs = server.inputs.load(Ordering::Acquire);
    fs::write(
        root.join("broken.txt"),
        "Recovered readable nebula documentation.\n",
    )?;
    engine.close();
    drop(engine);

    let engine = ZvecGrep::new();
    let resumed = engine.index(index_options(root)).await?;
    assert_eq!((resumed.files_unchanged, resumed.files_failed), (1, 0));
    assert!(resumed.failed_files.is_empty());
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs + 1);
    let after = engine.info(info_options(root)).await?;
    assert_eq!(after.index_path, info.index_path);
    let status = after.status.expect("published status");
    assert_eq!((status.files_failed, status.files_indexed), (0, 2));
    assert!(status.failed_files.is_empty());
    let records = native_file_records(&after.index_path)?;
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record["value"]["index_status"]["kind"] == "indexed")
    );
    assert_eq!(
        fts_paths(&engine, root, "nebula").await?,
        [PathBuf::from("broken.txt")]
    );
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn rebuild_publishes_successful_files_and_records_failures_for_incremental_retry()
-> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let home = root.join(".zvec-grep");
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("note.txt"),
        "Orchard documentation remains available.\n",
    )?;
    fs::write(
        root.join("stable.txt"),
        "Stable baseline that must be embedded again.\n",
    )?;
    let engine = ZvecGrep::new();
    engine.index(index_options(root)).await?;
    let original_path = engine.info(info_options(root)).await?.index_path;
    let original_inputs = server.inputs.load(Ordering::Acquire);
    fs::write(root.join("note.txt"), [255_u8, 254, 255])?;
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!(rebuilt.files_failed, 1);
    assert_eq!(rebuilt.failed_files[0].path, Path::new("note.txt"));
    assert_eq!(server.inputs.load(Ordering::Acquire), original_inputs + 1);
    let published = engine.info(info_options(root)).await?;
    assert_ne!(published.index_path, original_path);
    assert!(!original_path.exists());
    let status = published.status.expect("published status");
    assert_eq!((status.files_indexed, status.files_failed), (1, 1));
    assert_eq!(status.failed_files[0].path, Path::new("note.txt"));
    assert!(!home.join("build.json").exists());
    assert_eq!(fs::read_dir(home.join("generations"))?.count(), 1);
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "baseline").await?,
        [PathBuf::from("stable.txt")]
    );
    fs::write(
        root.join("note.txt"),
        "Vineyard replacement documentation.\n",
    )?;
    engine.close();
    drop(engine);

    let engine = ZvecGrep::new();
    let inputs = server.inputs.load(Ordering::Acquire);
    let resumed = engine.index(index_options(root)).await?;
    assert_eq!((resumed.files_unchanged, resumed.files_failed), (1, 0));
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs + 1);
    assert_eq!(
        engine.info(info_options(root)).await?.index_path,
        published.index_path
    );
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("note.txt")]
    );
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn incremental_failure_removes_all_old_searchable_content_for_the_file() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("note.txt"), "Orchard original documentation.\n")?;
    fs::write(root.join("stable.txt"), "Unchanged vineyard baseline.\n")?;
    let engine = ZvecGrep::new();
    engine.index(index_options(root)).await?;
    let index_path = engine.info(info_options(root)).await?.index_path;
    let inputs = server.inputs.load(Ordering::Acquire);
    fs::write(root.join("note.txt"), [255_u8, 254, 255])?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!((updated.files_unchanged, updated.files_failed), (1, 1));
    assert_eq!(updated.failed_files[0].path, Path::new("note.txt"));
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs);
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("stable.txt")]
    );
    assert_eq!(native_documents(&index_path.join("entities"))?.len(), 1);
    for collection in model_collections(&index_path)? {
        assert_eq!(native_documents(&collection)?.len(), 1);
    }
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!((status.files_indexed, status.files_failed), (1, 1));
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_engine_recovers_pending_files_without_skipping_unchanged_sources() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let source_path = root.join("note.txt");
    let contents = "Orchard documentation survives interrupted indexing.\n";
    fs::write(&source_path, contents)?;
    let modified = fs::metadata(&source_path)?.modified()?;

    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!((initial.files_added, initial.files_failed), (1, 0));
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
    let index_path = engine.info(info_options(root)).await?.index_path;
    let requests = server.requests.load(Ordering::Acquire);
    assert!(requests > 0);
    assert_eq!(server.inputs.load(Ordering::Acquire), 1);
    engine.close();
    drop(engine);

    let files_path = index_path.join("files");
    let source = {
        let files = native_documents(&files_path)?;
        assert_eq!(files.len(), 1);
        files[0].get_string("payload")?.expect("source payload")
    };
    // A completed file can still have a pending marker after a crash before marker removal.
    // The actual journal stores reindex intent, never a claim that the batch is complete.
    let mut pending_source: Value = serde_json::from_str(&source)?;
    pending_source["value"]["index_status"] = json!({"kind": "not_indexed"});
    // Preserve its full snapshot so recovery must override the ordinary unchanged-file fast path.
    let pending_path = index_path.join("pending.json");
    fs::write(
        &pending_path,
        serde_json::to_vec(&json!({
            "version": 2,
            "files": [{ "kind": "reindex", "source": serde_json::to_string(&pending_source)? }],
        }))?,
    )?;

    let engine = ZvecGrep::new();
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!(
        (
            status.files_pending,
            status.files_indexed,
            status.files_failed
        ),
        (1, 0, 0)
    );
    assert!(
        !pending_path.exists(),
        "recovery flushes and clears the marker"
    );
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    engine.close();
    drop(engine);

    let files = native_documents(&files_path)?;
    assert_eq!(
        files.len(),
        1,
        "recovery preserves the source for reindexing"
    );
    let original: Value = serde_json::from_str(&source)?;
    assert_eq!(original["value"]["index_status"]["kind"], "indexed");
    let recovered: Value = serde_json::from_str(
        &files[0]
            .get_string("payload")?
            .expect("recovered file payload"),
    )?;
    assert_eq!(recovered["version"], original["version"]);
    assert!(recovered["value"].get("formats").is_none());
    for field in ["id", "relative_path", "snapshot"] {
        assert_eq!(
            recovered["value"][field], original["value"][field],
            "recovery preserves {field}"
        );
    }
    assert_eq!(
        recovered["value"]["index_status"],
        json!({"kind": "not_indexed"})
    );
    let collections = fs::read_dir(&index_path)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name == "entities" || name.starts_with("fragments_"))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(collections.len(), 2);
    for collection in collections {
        assert!(
            native_documents(&collection)?.is_empty(),
            "recovery clears searchable records in {}",
            collection.display()
        );
    }
    assert_eq!(fs::read_to_string(&source_path)?, contents);
    assert_eq!(fs::metadata(&source_path)?.modified()?, modified);
    assert_eq!(server.requests.load(Ordering::Acquire), requests);

    let engine = ZvecGrep::new();
    let recovered = engine.index(index_options(root)).await?;
    assert_eq!((recovered.files_unchanged, recovered.files_failed), (0, 0));
    let after_reindex = server.requests.load(Ordering::Acquire);
    assert!(
        after_reindex > requests,
        "pending sources must be embedded again"
    );
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!((status.files_pending, status.files_indexed), (0, 1));
    let vector = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard documentation".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        vector
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("note.txt"))
    );
    let after_search = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!(unchanged.files_unchanged, 1);
    assert_eq!(server.requests.load(Ordering::Acquire), after_search);
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn public_engine_reuses_relative_files_after_moving_workspace() -> TestResult {
    let temporary = tempdir()?;
    let original_root = temporary.path().join("original");
    fs::create_dir_all(original_root.join("src"))?;
    let original_root = fs::canonicalize(original_root)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(&original_root, server.address)?;
    fs::write(
        original_root.join("src/note.txt"),
        "Orchard relocation preserves workspace identity.\n",
    )?;
    let query = |root: &Path, term: &str| ContextOptions {
        root: Some(root.to_path_buf()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: term.to_owned(),
        }],
        auto_update: false,
        allow_remote: true,
        ..ContextOptions::default()
    };
    let engine = ZvecGrep::new();
    engine.index(index_options(&original_root)).await?;
    let before = engine.context(query(&original_root, "orchard")).await?;
    assert_eq!(before.items.len(), 1);
    let entity_id = before.items[0].entity_id.clone();
    assert!(entity_id.is_some());
    engine.close();
    drop(engine);

    let relocated_root = temporary.path().join("relocated");
    fs::rename(&original_root, &relocated_root)?;
    let relocated_root = fs::canonicalize(relocated_root)?;
    let engine = ZvecGrep::new();
    let after = engine
        .context(query(&relocated_root.join("src"), "orchard"))
        .await?;
    assert_eq!(after.items.len(), 1);
    assert_eq!(after.items[0].entity_id, entity_id);
    assert_eq!(after.items[0].relative_path, Path::new("src/note.txt"));
    assert_eq!(
        after.items[0].absolute_path,
        relocated_root.join("src/note.txt")
    );
    assert_eq!(after.items[0].status, ContextItemStatus::Fresh);

    let calls = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(&relocated_root)).await?;
    assert_eq!(unchanged.files_unchanged, 1);
    assert_eq!((unchanged.files_added, unchanged.files_deleted), (0, 0));
    assert_eq!(server.requests.load(Ordering::Acquire), calls);

    fs::write(
        relocated_root.join("src/note.txt"),
        "Vineyard updated content.\n",
    )?;
    let updated = engine
        .index(IndexOptions {
            changes: vec![WorkspaceChange::Upsert(PathBuf::from("src/note.txt"))],
            ..index_options(&relocated_root)
        })
        .await?;
    assert_eq!(updated.files_modified, 1);
    let result = engine.context(query(&relocated_root, "vineyard")).await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].entity_id, entity_id);
    engine.drop_index(info_options(&relocated_root)).await?;
    engine.close();
    Ok(())
}

async fn fts_paths(
    engine: &ZvecGrep,
    root: &Path,
    query: &str,
) -> Result<Vec<PathBuf>, EngineError> {
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: query.to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    let mut paths = result
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[tokio::test]
async fn version_four_requires_explicit_rebuild_to_version_five() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("one.txt"), "Orchard first source.")?;
    fs::write(root.join("two.txt"), "Orchard second source.")?;
    let engine = ZvecGrep::new();
    engine.index(index_options(root)).await?;
    let before = engine.info(info_options(root)).await?;
    assert_eq!(
        before
            .workspace_index
            .as_ref()
            .expect("workspace")
            .index_version,
        Some(5)
    );
    let manifest_path = before.home.join("manifest.json");
    let mut old: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    old["indexVersion"] = json!(4);
    let old_manifest = serde_json::to_vec(&old)?;
    fs::write(&manifest_path, &old_manifest)?;
    // The index version must be checked before attempting to decode old storage.
    fs::write(
        before.index_path.join("schema.json"),
        b"obsolete storage format",
    )?;
    let inputs = server.inputs.load(Ordering::Acquire);
    let error = engine
        .index(index_options(root))
        .await
        .expect_err("no automatic upgrade");
    assert!(error.message().contains("zg index --rebuild"));
    let error = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            auto_update: true,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await
        .expect_err("query refresh must not upgrade the format");
    assert!(error.message().contains("zg index --rebuild"));
    assert_eq!(fs::read(&manifest_path)?, old_manifest);
    assert!(before.index_path.exists());
    assert!(!before.home.join("build.json").exists());
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs);
    let result = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!(result.files_added, 2);
    let after = engine.info(info_options(root)).await?;
    assert_eq!(
        after
            .workspace_index
            .as_ref()
            .expect("workspace")
            .index_version,
        Some(5)
    );
    assert_eq!(after.status.as_ref().expect("status").files_indexed, 2);
    assert_ne!(after.index_path, before.index_path);
    assert!(!before.index_path.exists());
    let current: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    for field in ["name", "root", "scan", "embeddingRuntimes", "createdTime"] {
        assert_eq!(current[field], old[field], "{field}");
    }
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 2);
    assert_eq!(
        engine.info(info_options(root)).await?.index_path,
        after.index_path
    );
    engine.close();
    Ok(())
}

fn model_collections(index_path: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    Ok(fs::read_dir(index_path)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("fragments_"))
        })
        .map(|entry| entry.path())
        .collect())
}
