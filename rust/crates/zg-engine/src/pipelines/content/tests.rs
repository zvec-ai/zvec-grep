use std::{collections::BTreeMap, fs, sync::Arc};

use crate::{
    ZvecGrep,
    api::content::ContentRef,
    domain::{
        Content, ContentKind, Entity, EntityFragment, EntityId, FileFormat, FileIndexStatus,
        FileRecord, FileSnapshot, FragmentId, ImageContent, IndexDescriptor, IndexState, Range,
        ScanRules, SourcePath, Workspace,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    },
    pipelines::indexed_search::writer::WriterSession,
    storage::types::{IndexedFragment, WorkspaceIndexStorageOptions},
    workspace::{
        CURRENT_INDEX_VERSION,
        lock::{LockMode, acquire_home_lock},
        manifest::write_workspace_manifest,
    },
};

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    manifest: WorkspaceManifest,
    entity: Entity,
    file: FileRecord,
}

impl Fixture {
    fn new(content: Content) -> Self {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path().canonicalize().expect("root");
        let home = root.join(".zvec-grep");
        let generation = uuid::Uuid::new_v4().to_string();
        let schema = EmbeddingModelInfo {
            model: ModelInfo::new(
                "fixture",
                "vision",
                [ContentKind::Text, ContentKind::Code, ContentKind::Image],
            )
            .expect("model"),
            dimension: 2,
            metric: EmbeddingMetric::Cosine,
            max_batch_size: 1,
            max_input_tokens: None,
            max_image_bytes: Some(5 * 1024 * 1024),
        };
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "content-fixture".into(),
                root: root.clone(),
                scan: ScanRules::default(),
                index: IndexState::Enabled(IndexDescriptor::single(schema.clone())),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            home.clone(),
            Some(CURRENT_INDEX_VERSION),
            BTreeMap::new(),
        )
        .expect("manifest");
        manifest.storage_generation = Some(generation);
        fs::create_dir_all(manifest.storage_home()).expect("generation directory");
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: manifest.storage_home(),
            embeddings: vec![schema],
        })
        .expect("storage");
        let relative_path = SourcePath::new("image.png").expect("relative path");
        let file_id = storage
            .resolve_file_ids(&[relative_path.to_path_buf()])
            .expect("file ID")[0];
        let file = FileRecord {
            id: file_id,
            relative_path,
            snapshot: FileSnapshot {
                size_bytes: 3,
                modified_epoch_ms: None,
                content_hash: Some("fixture-content".into()),
            },
            index_status: FileIndexStatus::NotIndexed,
        };
        let id = EntityId::new(file_id, &content, Range::Full).expect("entity ID");
        let entity = Entity {
            id: id.clone(),
            file_id,
            source_range: Range::Full,
            content,
            metadata: None,
            fragments: vec![EntityFragment {
                id: FragmentId::new(&id, 0),
                range: Range::Full,
            }],
        };
        storage
            .replace_file(
                &file,
                std::slice::from_ref(&entity),
                &[IndexedFragment {
                    entity_id: id,
                    fragment_id: entity.fragments[0].id.clone(),
                    model: "fixture/vision".into(),
                    vector: vec![1.0, 0.0],
                    fts_text: String::new(),
                }],
            )
            .expect("canonical snapshot");
        storage.close().expect("close writer");
        write_workspace_manifest(&home, &manifest).expect("publish manifest");
        Self {
            _directory: directory,
            manifest,
            entity,
            file,
        }
    }

    fn image() -> Self {
        Self::new(Content::Image(
            ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image"),
        ))
    }

    fn options(&self) -> ReadContentOptions {
        ReadContentOptions {
            root: Some(self.manifest.workspace.root.clone()),
            ..ReadContentOptions::new(ContentRef {
                generation: self
                    .manifest
                    .storage_generation
                    .clone()
                    .expect("generation"),
                entity_id: self.entity.id.as_str().into(),
            })
        }
    }

    fn writer(&self) -> IndexStore {
        IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: self.manifest.storage_home(),
            embeddings: self.manifest.embeddings().to_vec(),
        })
        .expect("writer")
    }
}

#[tokio::test]
async fn reads_indexed_bytes_even_after_source_changes_and_rejects_stale_generation() {
    let mut fixture = Fixture::image();
    let engine = ZvecGrep::new();
    let options = fixture.options();
    let source = fixture.manifest.workspace.root.join("image.png");
    fs::write(&source, [9, 8, 7]).expect("changed source");
    let result = engine
        .read_content(options.clone())
        .await
        .expect("indexed image");
    assert_eq!(result.content, fixture.entity.content);
    assert_eq!(result.path, source);
    let json = serde_json::to_value(&result).expect("content JSON");
    assert_eq!(json["content"]["kind"], "image");
    assert_eq!(json["content"]["value"]["format"], "png");
    assert_eq!(json["content"]["value"]["data"], "AQID");
    assert_eq!(
        serde_json::from_value::<ContentResult>(json).expect("content decode"),
        result
    );
    fs::remove_file(&source).expect("remove source");
    assert_eq!(
        engine
            .read_content(options.clone())
            .await
            .expect("source absent")
            .content,
        fixture.entity.content
    );
    fixture.manifest.storage_generation = Some(uuid::Uuid::new_v4().to_string());
    write_workspace_manifest(&fixture.manifest.path, &fixture.manifest).expect("new generation");
    let error = engine
        .read_content(options)
        .await
        .expect_err("stale reference");
    assert_eq!(error.code(), EngineError::NOT_FOUND);
    assert!(error.message().contains("inactive storage generation"));
}

#[tokio::test]
async fn rejects_missing_cancelled_and_closed_reads() {
    let fixture = Fixture::image();
    let engine = ZvecGrep::new();
    let mut missing = fixture.options();
    missing.reference.entity_id = "missing".into();
    assert_eq!(
        engine
            .read_content(missing)
            .await
            .expect_err("missing")
            .code(),
        EngineError::NOT_FOUND
    );
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    assert_eq!(
        engine
            .read_content(ReadContentOptions {
                signal: Some(signal),
                ..fixture.options()
            })
            .await
            .expect_err("cancelled")
            .code(),
        EngineError::CANCELLED
    );
    let writer = fixture.writer();
    writer
        .delete_file(fixture.file.id)
        .expect("delete snapshot");
    writer.close().expect("close writer");
    assert_eq!(
        engine
            .read_content(fixture.options())
            .await
            .expect_err("removed entity")
            .code(),
        EngineError::NOT_FOUND
    );
    engine.close();
    assert_eq!(
        engine
            .read_content(fixture.options())
            .await
            .expect_err("closed")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
}

#[tokio::test]
async fn content_reads_borrow_active_writer_without_waiting_for_its_home_lock() {
    let fixture = Fixture::image();
    let indexing = WorkspaceIndexService::new();
    let home_lock = acquire_home_lock(&fixture.manifest.path, LockMode::Write, "test.writer")
        .expect("writer lock");
    let registration = indexing.writers.register(
        WriterSession::new(
            fixture.writer(),
            Vec::new(),
            fixture.manifest.clone(),
            Arc::new(home_lock),
        ),
        true,
    );
    let result = read_content(
        &indexing,
        &ReadContentOptions {
            lock_timeout_ms: Some(0),
            ..fixture.options()
        },
        None,
    )
    .await
    .expect("borrowed writer snapshot");
    assert_eq!(result.content, fixture.entity.content);
    registration.retire().await;
}

#[tokio::test]
async fn reads_complete_text_code_and_image_without_source_files() {
    let image = Content::Image(ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image"));
    for content in [
        Content::Text("first\nsecond\n".into()),
        Content::Code("fn main() {}\n".into()),
        image,
    ] {
        let fixture = Fixture::new(content.clone());
        assert!(!fixture.manifest.workspace.root.join("image.png").exists());
        let engine = ZvecGrep::new();
        let result = engine
            .read_content(fixture.options())
            .await
            .expect("stored content");
        assert_eq!(result.content, content);
        let bytes = serde_json::to_vec(&result).expect("encode result");
        assert_eq!(
            serde_json::from_slice::<ContentResult>(&bytes).expect("decode result"),
            result
        );
    }
}

#[tokio::test]
async fn deleted_reference_cannot_read_another_file_with_identical_content_after_reopen() {
    let fixture = Fixture::image();
    let original = fixture.options();
    let writer = fixture.writer();
    writer
        .delete_file(fixture.file.id)
        .expect("delete original");
    writer.close().expect("close after deleting highest ID");

    let writer = fixture.writer();
    let mut file = fixture.file.clone();
    file.relative_path = SourcePath::new("other.png").expect("new source path");
    file.id = writer
        .resolve_file_ids(&[file.relative_path.to_path_buf()])
        .expect("new identity")[0];
    assert_ne!(file.id, fixture.file.id);
    let mut entity = fixture.entity.clone();
    entity.file_id = file.id;
    entity.id = EntityId::new(file.id, &entity.content, entity.source_range).expect("entity ID");
    entity.fragments[0].id = FragmentId::new(&entity.id, 0);
    writer
        .replace_file(
            &file,
            std::slice::from_ref(&entity),
            &[IndexedFragment {
                entity_id: entity.id.clone(),
                fragment_id: entity.fragments[0].id.clone(),
                model: "fixture/vision".into(),
                vector: vec![1.0, 0.0],
                fts_text: String::new(),
            }],
        )
        .expect("index same content at new source");
    writer.close().expect("close replacement");

    let engine = ZvecGrep::new();
    assert_eq!(
        engine
            .read_content(original.clone())
            .await
            .expect_err("old entity is gone")
            .code(),
        EngineError::NOT_FOUND
    );
    let mut current = original;
    current.reference.entity_id = entity.id.as_str().into();
    let result = engine
        .read_content(current)
        .await
        .expect("new source reference");
    assert_eq!(
        result.path,
        fixture.manifest.workspace.root.join("other.png")
    );
    assert_eq!(result.content, fixture.entity.content);
}
