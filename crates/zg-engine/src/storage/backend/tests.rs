use super::*;
use crate::domain::{
    Content, Entity, EntityContent, EntityFragment, EntityMetadata, FileFormat, FileSnapshot,
    SourceFile, SourceRange, SymbolType,
};

fn schema() -> WorkspaceIndexEmbeddingSchema {
    WorkspaceIndexEmbeddingSchema {
        provider: "fixture".to_owned(),
        model: "fixture-model".to_owned(),
        dimension: 3,
        metric: EmbeddingMetric::Cosine,
    }
}

fn open(path: &Path, read_only: bool) -> Box<dyn WorkspaceIndexStorage> {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embedding: schema(),
        }
    };
    ZvecStorageFactory::new()
        .open(options)
        .expect("open real zvec storage")
}

fn fixture(root: &Path, id: &str, text: &str, vector: Vec<f32>) -> (StoredFile, IndexedFragment) {
    let id = FileId::new(id).expect("file ID");
    let file = StoredFile {
        source: SourceFile {
            id: id.clone(),
            absolute_path: root.join(format!("{}.txt", id.as_str())),
            relative_path: PathBuf::from(format!("{}.txt", id.as_str())),
            root_path: root.to_owned(),
            formats: vec![FileFormat::Text],
            snapshot: FileSnapshot {
                size_bytes: text.len() as u64,
                modified_epoch_ms: Some(1),
                content_hash: None,
            },
        },
        index_status: None,
    };
    let entry = IndexedFragment {
        fragment: EntityFragment::Standalone(Entity {
            id: EntityId::new(format!("entity-{}", id.as_str())).expect("entity ID"),
            file_id: id,
            range: SourceRange::File,
            content: EntityContent::Source(vec![Content::Text(text.to_owned())]),
            metadata: Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("quoted'\\name\0suffix".to_owned()),
                scope: None,
                node_type: None,
                signature: None,
                documentation: None,
                modifiers: Vec::new(),
            }),
        }),
        vector,
    };
    (file, entry)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the storage lifecycle in execution order"
)]
fn persists_filters_and_replaces_complete_files() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (first, entry) = fixture(home, "first", "orchard\0苹果 数据库", vec![1.0, 0.0, 0.0]);
    let (second, other) = fixture(home, "second", "orchard vineyard", vec![0.0, 1.0, 0.0]);
    storage
        .replace_file(&first, std::slice::from_ref(&entry), None)
        .expect("first file");
    storage
        .replace_file(&second, &[other], None)
        .expect("second file");

    let filter = StorageSearchFilter {
        file_ids: Some(vec![first.source.id.clone()]),
        entity_ids: Some(vec![entry.fragment.entity_id().clone()]),
        symbol_names: Some(vec!["quoted'\\name\0suffix".to_owned()]),
        symbol_types: Some(vec![SymbolType::Function]),
    };
    for query in ["orchard", "数据库", "orchard\0"] {
        let hits = storage
            .search_fts(query, 10, Some(&filter))
            .expect("filtered FTS");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fragment, entry.fragment);
    }
    let hits = storage
        .search_vector(&[1.0, 0.0, 0.0], 10, Some(&filter))
        .expect("filtered ANN");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file.source, first.source);
    let ranked = storage
        .search_vector(&[1.0, 0.0, 0.0], 10, None)
        .expect("ranked ANN");
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].fragment, entry.fragment);
    for rejected in [
        StorageSearchFilter {
            file_ids: Some(vec![second.source.id.clone()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            entity_ids: Some(vec![EntityId::new("missing").expect("entity ID")]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_names: Some(vec!["quoted'\\name suffix".to_owned()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_types: Some(vec![SymbolType::Value]),
            ..filter.clone()
        },
    ] {
        assert!(
            storage
                .search_fts("orchard", 10, Some(&rejected))
                .expect("FTS exclusion")
                .is_empty()
        );
        assert!(
            storage
                .search_vector(&[1.0, 0.0, 0.0], 10, Some(&rejected))
                .expect("ANN exclusion")
                .is_empty()
        );
    }
    assert_eq!(
        storage
            .get_entity(entry.fragment.entity_id())
            .expect("entity")
            .expect("exists")
            .entity,
        *entry.fragment.as_entity().expect("standalone")
    );
    let empty = StorageSearchFilter {
        file_ids: Some(Vec::new()),
        ..StorageSearchFilter::default()
    };
    assert!(
        storage
            .search_fts("orchard", 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(&[1.0, 0.0, 0.0], 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );

    storage
        .mark_file_failed(&first, "fixture extraction error")
        .expect("mark failed");
    assert!(
        storage
            .search_fts("数据库", 10, None)
            .expect("old content removed")
            .is_empty()
    );
    assert!(
        storage
            .get_entity(entry.fragment.entity_id())
            .expect("old entity removed")
            .is_none()
    );
    assert_eq!(
        storage.list_files().expect("files")[0]
            .index_status
            .as_ref()
            .and_then(|status| status.error.as_deref()),
        Some("fixture extraction error")
    );
    storage
        .replace_file(&first, &[entry], None)
        .expect("retry failed file");
    storage.delete_file(&second.source.id).expect("delete file");
    storage.close().expect("close writer");
    assert_eq!(
        storage.list_files().expect_err("closed lease").code(),
        EngineError::RESOURCE_CLOSED
    );
    let reader = open(home, true);
    assert_eq!(reader.list_files().expect("reopened files").len(), 1);
    assert_eq!(
        reader
            .search_fts("数据库", 10, None)
            .expect("reopened FTS")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector(&[1.0, 0.0, 0.0], 10, None)
            .expect("reopened ANN")
            .len(),
        1
    );
    assert!(reader.delete_file(&first.source.id).is_err());
    let second_reader = open(home, true);
    reader.close().expect("close one reader");
    assert_eq!(
        second_reader
            .list_files()
            .expect("other lease survives")
            .len(),
        1
    );
    assert!(ZvecStorageFactory::new().delete(home).is_err());
    second_reader.close().expect("close other reader");
    ZvecStorageFactory::new()
        .delete(home)
        .expect("drop storage");
    assert!(!ZvecStorageFactory::new().exists(home).expect("absence"));
}

#[test]
fn replays_interrupted_writes_before_serving_readers() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let (file, original) = fixture(home, "source", "old apple", vec![1.0, 0.0, 0.0]);
    let storage = open(home, false);
    storage
        .replace_file(&file, &[original], None)
        .expect("initial file");
    storage.close().expect("close writer");
    let (_, replacement) = fixture(home, "source", "new banana", vec![0.0, 1.0, 0.0]);
    let path = home.join("storage");
    let record = JournalRecord {
        version: VERSION,
        operation: replace_operation(&file, &[replacement]).expect("replacement record"),
    };
    write_record(
        &path.join(JOURNAL),
        &serde_json::to_vec(&record).expect("encode journal"),
    )
    .expect("durable intent");
    // Simulate termination after deleting old rows but before publishing replacements.
    let native = NativeStore::open(&path, &schema(), false).expect("native writer");
    native
        .apply_delete(&file.source.id)
        .expect("partial mutation");
    native.flush().expect("persist partial mutation");
    drop(native);
    let competing_reader = acquire_storage_lock(home, true).expect("another reader's lock");
    let recovery_error = ZvecStorageFactory::new()
        .open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.to_owned(),
        })
        .err()
        .expect("recovery requires exclusive access");
    assert_eq!(recovery_error.code(), EngineError::RESOURCE_BUSY);
    assert!(path.join(JOURNAL).exists());
    drop(competing_reader);
    let reader = open(home, true);
    assert!(!path.join(JOURNAL).exists());
    let shared_lock =
        acquire_storage_lock(home, true).expect("recovered reader holds a shared lock");
    assert_eq!(
        acquire_storage_lock(home, false)
            .expect_err("recovered reader excludes writers")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    drop(shared_lock);
    assert!(
        reader
            .search_fts("apple", 10, None)
            .expect("old rows absent")
            .is_empty()
    );
    assert_eq!(
        reader
            .search_fts("banana", 10, None)
            .expect("replacement restored")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector(&[0.0, 1.0, 0.0], 10, None)
            .expect("vectors restored")
            .len(),
        1
    );
    reader.close().expect("close recovered reader");
    drop(acquire_storage_lock(home, false).expect("closing recovered reader releases its lock"));

    let record = JournalRecord {
        version: VERSION,
        operation: Operation::Delete {
            file_id: file.source.id.as_str().to_owned(),
        },
    };
    write_record(
        &path.join(JOURNAL),
        &serde_json::to_vec(&record).expect("encode delete"),
    )
    .expect("pending delete");
    let reader = open(home, true);
    assert!(reader.list_files().expect("recovered deletion").is_empty());
    reader.close().expect("close empty storage");
    // A corrupt journal must fail closed instead of serving inconsistent collections.
    fs::write(path.join(JOURNAL), b"corrupt journal").expect("corrupt fixture");
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned()
            })
            .is_err()
    );
    assert!(path.join(JOURNAL).exists());
}

#[test]
fn rejects_invalid_writes_without_poisoning_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, mut entry) = fixture(home, "source", "healthy document", vec![1.0, 0.0, 0.0]);
    for invalid in [vec![1.0], vec![f32::NAN, 0.0, 0.0], vec![0.0, 0.0, 0.0]] {
        entry.vector = invalid;
        assert_eq!(
            storage
                .replace_file(&file, std::slice::from_ref(&entry), None)
                .expect_err("invalid vector")
                .code(),
            EngineError::INVALID_ARGUMENT
        );
        assert!(!home.join("storage").join(JOURNAL).exists());
        assert!(
            storage
                .list_files()
                .expect("storage still usable")
                .is_empty()
        );
    }
    entry.vector = vec![1.0, 0.0, 0.0];
    storage
        .replace_file(&file, &[entry], None)
        .expect("valid write after rejections");
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned()
            })
            .is_err(),
        "readers cannot open during a writer lease"
    );
    storage.close().expect("close writer");
    let mut incompatible = schema();
    incompatible.dimension = 4;
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: home.to_owned(),
                embedding: incompatible
            })
            .is_err()
    );
    let storage = open(home, false);
    assert_eq!(
        storage
            .list_files()
            .expect("schema rejection preserved index")
            .len(),
        1
    );
    storage.close().expect("close writer");
}

#[test]
fn rejects_legacy_coordinate_schemas_before_opening_collections() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    fs::create_dir(&path).expect("storage directory");
    let mut record = SchemaRecord::new(&schema());
    record.version = 1;
    fs::write(
        path.join("schema.json"),
        serde_json::to_vec(&record).expect("legacy schema"),
    )
    .expect("write legacy schema");
    for options in [
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.to_owned(),
        },
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            embedding: schema(),
        },
    ] {
        let error = ZvecStorageFactory::new()
            .open(options)
            .err()
            .expect("legacy schema is incompatible");
        assert!(
            error
                .message()
                .contains("unsupported storage schema version 1")
        );
        assert!(error.message().contains("rebuild the index"));
    }
    assert_eq!(fs::read_dir(path).expect("storage files").count(), 1);
}

#[test]
fn writes_fragments_across_native_batch_boundaries() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, prototype) = fixture(home, "batch", "harvest", vec![1.0, 0.0, 0.0]);
    let entries = (0..1025)
        .map(|index| {
            let mut entity = prototype.fragment.as_entity().expect("standalone").clone();
            entity.id = EntityId::new(format!("batch-entity-{index}")).expect("entity ID");
            IndexedFragment {
                fragment: EntityFragment::Standalone(entity),
                vector: prototype.vector.clone(),
            }
        })
        .collect::<Vec<_>>();
    storage
        .replace_file(&file, &entries, None)
        .expect("batched write");
    let last = entries.last().expect("last batch entry");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![last.fragment.entity_id().clone()]),
        ..StorageSearchFilter::default()
    };
    assert_eq!(
        storage
            .search_fts("harvest", 1030, None)
            .expect("all batches")
            .len(),
        entries.len()
    );
    assert_eq!(
        storage
            .search_vector(&prototype.vector, 10, Some(&filter))
            .expect("last batch ANN")[0]
            .fragment,
        last.fragment
    );
    storage
        .replace_file(&file, &[], None)
        .expect("replace with empty file");
    assert!(
        storage
            .search_fts("harvest", 10, None)
            .expect("no stale fragments")
            .is_empty()
    );
    assert!(
        storage
            .get_entity(last.fragment.entity_id())
            .expect("no stale entity")
            .is_none()
    );
    storage.close().expect("close storage");
}
