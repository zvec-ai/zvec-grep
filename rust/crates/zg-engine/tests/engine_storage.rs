mod support;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_documents,
    native_file_records, set_native_file_status,
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
        index::{IndexOptions, options::WorkspaceChange},
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn one_text_model_indexes_text_and_skips_images_without_embedding_them() -> TestResult {
    use zg_engine::api::index::result::SkippedFileReason;

    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("note.txt"), "Orchard textual documentation.")?;
    fs::write(root.join("diagram.png"), [137, 80, 78, 71, 13, 10, 26, 10])?;
    fs::write(
        root.join("diagram.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )?;
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            // Exercise unsupported-content handling even for files normally ignored.
            scan: zg_engine::api::index::options::ScanRulesUpdate {
                globs: Some(vec!["*.txt".into(), "*.png".into(), "*.svg".into()]),
                ..Default::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (1, 0));
    assert_eq!(indexed.skipped.len(), 2);
    assert!(
        indexed
            .skipped
            .iter()
            .all(|file| file.reason == SkippedFileReason::Unsupported)
    );
    assert_eq!(server.inputs.load(Ordering::Acquire), 1);
    assert_eq!(server.multimodal_inputs.load(Ordering::Acquire), 0);

    let info = engine.info(info_options(root)).await?;
    assert_eq!(
        info.workspace_index
            .as_ref()
            .expect("workspace")
            .index_version,
        Some(2)
    );
    let collections = model_collections(&info.index_path)?;
    assert_eq!(collections.len(), 1);
    assert_eq!(native_documents(&collections[0])?.len(), 1);
    assert_eq!(
        native_documents(&info.index_path.join("entities"))?.len(),
        1
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join(".zvec-grep/manifest.json"))?)?;
    assert!(manifest.get("manifestVersion").is_none());
    assert_eq!(manifest["indexVersion"], 2);
    assert_eq!(manifest["embeddings"].as_array().expect("models").len(), 1);
    assert_eq!(
        manifest["embeddingRoutes"],
        json!({"text":"qwen/text-embedding-v4"})
    );
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );

    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard".into(),
            }],
            auto_update: false,
            allow_remote: true,
            api_key: Some("local-test-key".into()),
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path, Path::new("note.txt"));
    assert_eq!(server.inputs.load(Ordering::Acquire), 2);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!((unchanged.files_unchanged, unchanged.files_failed), (1, 0));
    assert_eq!(server.inputs.load(Ordering::Acquire), 2);
    assert_eq!(server.multimodal_inputs.load(Ordering::Acquire), 0);
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn whitespace_only_ranges_do_not_fail_complete_file_indexing() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let preamble = format!(
        "intro 中文\r\n{}tail punctuation!\r",
        " \t\r\n".repeat(5_000)
    );
    fs::write(
        root.join("README.md"),
        format!("{preamble}\n# Heading\r\nbody"),
    )?;
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(root)).await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (1, 0));
    let info = engine.info(info_options(root)).await?;
    let entities = native_documents(&info.index_path.join("entities"))?;
    let payloads = entities
        .iter()
        .map(|doc| {
            serde_json::from_str::<Value>(
                &doc.get_string("payload")
                    .expect("payload field")
                    .expect("payload"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let entity = payloads
        .iter()
        .find(|entity| entity["content"]["value"] == preamble)
        .expect("canonical content preserves every whitespace byte");
    let mut covered = vec![false; preamble.len()];
    for fragment in entity["fragments"].as_array().expect("fragments") {
        let selected = &fragment["range"];
        let start = usize::try_from(selected["start_offset"].as_u64().expect("start"))?;
        let end = usize::try_from(selected["end_offset"].as_u64().expect("end"))?;
        assert!(!preamble[start..end].trim().is_empty());
        covered[start..end].fill(true);
    }
    for (offset, character) in preamble.char_indices() {
        if !character.is_whitespace() {
            assert!(
                covered[offset..offset + character.len_utf8()]
                    .iter()
                    .all(|value| *value)
            );
        }
    }
    assert!(covered.iter().any(|value| !value));
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn long_entities_store_original_content_once_and_project_fragment_metadata() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let source = format!(
        "pub fn orchard_process() {{\r\n{}\r\n}}",
        "    let _ = \"果园 orchard 😀\";\r\n".repeat(700)
    );
    let bytes = [0xff, 0xfe]
        .into_iter()
        .chain(source.encode_utf16().flat_map(u16::to_le_bytes))
        .collect::<Vec<_>>();
    fs::write(root.join("orchard.rs"), bytes)?;
    let engine = ZvecGrep::new();
    let indexed = engine.index(index_options(root)).await?;
    assert_eq!(
        (
            indexed.files_added,
            indexed.files_failed,
            indexed.entities_created
        ),
        (1, 0, 1)
    );
    let info = engine.info(info_options(root)).await?;
    let entities = native_documents(&info.index_path.join("entities"))?;
    assert_eq!(entities.len(), 1);
    let payload: Value = serde_json::from_str(
        &entities[0]
            .get_string("payload")?
            .expect("canonical entity"),
    )?;
    let entity = &payload;
    let entity_id = entity["id"].as_str().expect("entity ID");
    assert_eq!(entity_id.len(), 32);
    assert_eq!(entities[0].get_pk(), Some(entity_id));
    assert_eq!(
        entities[0].get_string("entity_id")?.as_deref(),
        Some(entity_id)
    );
    assert_eq!(entity["content"], json!({"kind": "text", "value": source}));
    assert_eq!(entity["source_range"]["kind"], "text");
    assert!(entity.get("range").is_none());
    let metadata: Value = serde_json::from_str(
        &entities[0]
            .get_string("metadata")?
            .expect("entity metadata"),
    )?;
    assert_eq!(metadata["symbol_name"], "orchard_process");
    let fragments = entity["fragments"]
        .as_array()
        .expect("fragment definitions");
    assert!(fragments.len() > 1);
    let mut coverage = vec![false; source.len()];
    let mut ids = BTreeSet::new();
    for (ordinal, fragment) in fragments.iter().enumerate() {
        let id = fragment["id"].as_str().expect("fragment ID");
        assert_eq!(id, format!("{entity_id}{ordinal:08x}"));
        assert_eq!(id.len(), 40);
        assert!(ids.insert(id.to_owned()));
        assert!(fragment.get("content").is_none());
        assert!(fragment.get("metadata").is_none());
        assert_eq!(fragment.as_object().expect("fragment object").len(), 2);
        assert!(fragment.get("content_range").is_none());
        let selected = &fragment["range"];
        assert_eq!(selected["kind"], "byte");
        assert_eq!(selected.as_object().expect("range object").len(), 3);
        let start = usize::try_from(selected["start_offset"].as_u64().expect("start"))?;
        let end = usize::try_from(selected["end_offset"].as_u64().expect("end"))?;
        assert!(source.get(start..end).is_some());
        coverage[start..end].fill(true);
    }
    assert!(coverage.into_iter().all(|covered| covered));
    assert_eq!(server.inputs.load(Ordering::Acquire), fragments.len());
    let collections = model_collections(&info.index_path)?;
    assert_eq!(collections.len(), 1);
    let projected = native_documents(&collections[0])?;
    assert_eq!(projected.len(), fragments.len());
    let mut projected_ids = BTreeSet::new();
    for doc in projected {
        let document_id = doc.get_string("document_id")?.expect("fragment ID");
        assert_eq!(doc.get_pk(), Some(document_id.as_str()));
        assert_eq!(doc.get_string("entity_id")?.as_deref(), Some(entity_id));
        projected_ids.insert(document_id);
        assert_eq!(doc.get_string("symbol_type")?.as_deref(), Some("function"));
        assert!(doc.get_string("symbol_name")?.is_some());
        assert!(
            doc.get_string("text")?
                .expect("FTS projection")
                .contains("orchard_process")
        );
        assert!(!doc.has_field("metadata"));
        assert!(!doc.has_field("payload"));
    }
    assert_eq!(projected_ids, ids);
    engine.close();
    fs::remove_file(root.join("orchard.rs"))?;
    let engine = ZvecGrep::new();
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        let result = engine
            .context(ContextOptions {
                root: Some(root.to_path_buf()),
                routes: vec![ContextRoute {
                    mode,
                    query: "orchard".into(),
                }],
                filter: QueryFilter {
                    symbol_types: vec![zg_engine::api::context::options::SymbolType::Function],
                    ..Default::default()
                },
                auto_update: false,
                allow_remote: true,
                ..Default::default()
            })
            .await?;
        assert_eq!(result.items.len(), 1);
        let item = &result.items[0];
        let Some(zg_engine::api::context::result::ContentRange::Text {
            start_line,
            end_line,
            start_byte_offset,
            end_byte_offset,
            start_byte_column,
            end_byte_column,
        }) = &item.excerpt_range
        else {
            panic!("text source excerpt");
        };
        assert_eq!(
            source.get(*start_byte_offset..*end_byte_offset),
            Some(item.content.as_str())
        );
        for (offset, line, column) in [
            (*start_byte_offset, *start_line, *start_byte_column),
            (*end_byte_offset, *end_line, *end_byte_column),
        ] {
            let prefix = &source[..offset];
            assert_eq!(
                line,
                prefix.bytes().filter(|byte| *byte == b'\n').count() + 1
            );
            assert_eq!(
                column,
                prefix.rfind('\n').map_or(offset, |last| offset - last - 1)
            );
        }
        assert_eq!(
            item.content_role,
            Some(zg_engine::api::context::result::ContextContentRole::Source)
        );
        assert!(serde_json::to_value(item)?.get("outline").is_none());
    }
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
            .all(|record| record["index_status"]["kind"] == "indexed")
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
    // Simulate interruption after recording NotIndexed. Opening the store must
    // preserve this state and any remaining search documents until indexing retries.
    let original: Value = serde_json::from_str(&source)?;
    let id = u32::try_from(original["id"].as_u64().expect("file ID"))?;
    set_native_file_status(&index_path, id, "not_indexed")?;

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
    assert!(!index_path.join("pending.json").exists());
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
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
    assert_eq!(original["index_status"]["kind"], "indexed");
    let recovered: Value = serde_json::from_str(
        &files[0]
            .get_string("payload")?
            .expect("recovered file payload"),
    )?;
    assert!(recovered.get("formats").is_none());
    for field in ["id", "relative_path", "snapshot"] {
        assert_eq!(
            recovered[field], original[field],
            "recovery preserves {field}"
        );
    }
    assert_eq!(recovered["index_status"], json!({"kind": "not_indexed"}));
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
            !native_documents(&collection)?.is_empty(),
            "opening storage preserves intermediate records in {}",
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
    assert_ne!(result.items[0].entity_id, entity_id);
    assert!(
        engine
            .context(query(&relocated_root, "orchard"))
            .await?
            .items
            .is_empty()
    );
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
