use super::super::pending::{self, PendingChange, PendingChanges};
use super::super::spi::StoragePathFilter;
use super::*;
use crate::domain::SourcePath;
use crate::domain::{
    CodeMetadata, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileRecord,
    FileSnapshot, FragmentId, Range, SymbolType, TableCell, TableCellRole, TableContent,
};

/// Test input keeps canonical entities and model outputs separate, as the writer does.
#[derive(Clone)]
struct FixtureEntity {
    entity: Entity,
    model: String,
    vector: Vec<f32>,
}

fn fixture_records(
    fixtures: &[FixtureEntity],
) -> (Vec<Entity>, Vec<super::super::spi::IndexedFragment>) {
    let entities = fixtures
        .iter()
        .map(|fixture| fixture.entity.clone())
        .collect();
    let entries = fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .entity
                .fragments
                .iter()
                .map(|fragment| super::super::spi::IndexedFragment {
                    entity_id: fixture.entity.id.clone(),
                    fragment_id: fragment.id.clone(),
                    model: fixture.model.clone(),
                    vector: fixture.vector.clone(),
                })
        })
        .collect();
    (entities, entries)
}

trait FixtureWriter {
    fn replace_fixture_file(
        &self,
        file: &FileRecord,
        fixtures: &[FixtureEntity],
    ) -> StorageResult<()>;
}
impl<T: WorkspaceIndexStorage + ?Sized> FixtureWriter for T {
    fn replace_fixture_file(
        &self,
        file: &FileRecord,
        fixtures: &[FixtureEntity],
    ) -> StorageResult<()> {
        let (entities, entries) = fixture_records(fixtures);
        self.replace_file(file, &entities, &entries)
    }
}
impl NativeStore {
    fn apply_fixture_file(
        &self,
        file: &FileRecord,
        fixtures: &[FixtureEntity],
    ) -> StorageResult<()> {
        let (entities, entries) = fixture_records(fixtures);
        self.apply_replace(file, &entities, &entries)
    }
}

fn file_at(storage: &dyn WorkspaceIndexStorage, path: &str) -> (FileRecord, FixtureEntity) {
    let (mut file, mut entry) = fixture(None, "template", "orchard", vec![1.0, 0.0, 0.0]);
    file.relative_path = crate::domain::SourcePath::new(path).expect("source path");
    file.id = storage
        .resolve_file_ids(&[file.relative_path.to_path_buf()])
        .expect("reserve identity")[0];
    let entity = &mut entry.entity;
    entity.file_id = file.id;
    entity.id = EntityId::new(format!("entity-{}", file.id)).expect("entity ID");
    entity.fragments[0].id = FragmentId::new(format!("fragment-{}", file.id)).expect("fragment ID");
    (file, entry)
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Compare FTS and ANN across the same complete metadata fixture"
)]
fn directory_and_filename_filters_match_both_retrieval_collections() {
    let directory = tempfile::tempdir().expect("workspace");
    let storage = open(directory.path(), false);
    let paths = [
        "main.rs",
        "src/main.rs",
        "src/nested/lib.rs",
        "src/main.ts",
        "src-old/main.rs",
        "docs/readme.md",
        "src/under_score.rs",
        "src/percent%.rs",
        "src/quote'name.rs",
    ];
    let mut files = Vec::new();
    for path in paths {
        let (file, entry) = file_at(storage.as_ref(), path);
        storage
            .replace_fixture_file(&file, &[entry])
            .expect("indexed fixture");
        files.push(file);
    }
    let src = SourcePath::new("src").expect("path");
    let nested = SourcePath::new("src/nested").expect("path");
    let cases = [
        (
            StoragePathFilter::Directory(src.clone()),
            vec![1, 2, 3, 6, 7, 8],
        ),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::Directory(src.clone()))),
            vec![0, 4, 5],
        ),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::Or(vec![
                StoragePathFilter::Directory(src.clone()),
                StoragePathFilter::FileNameExact("main.rs".into()),
            ]))),
            vec![5],
        ),
        (
            StoragePathFilter::FileNameExact("main.rs".into()),
            vec![0, 1, 4],
        ),
        (
            StoragePathFilter::FileNamePrefix("main".into()),
            vec![0, 1, 3, 4],
        ),
        (
            StoragePathFilter::FileNameSuffix(".rs".into()),
            vec![0, 1, 2, 4, 6, 7, 8],
        ),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::Not(Box::new(
                StoragePathFilter::FileNameSuffix(".md".into()),
            )))),
            vec![5],
        ),
        (StoragePathFilter::FileNamePrefix("under_".into()), vec![6]),
        (
            StoragePathFilter::FileNameExact("quote'name.rs".into()),
            vec![8],
        ),
        (StoragePathFilter::FileNameSuffix("%.rs".into()), vec![7]),
        (
            StoragePathFilter::And(vec![
                StoragePathFilter::Directory(src.clone()),
                StoragePathFilter::FileNameSuffix(".rs".into()),
                StoragePathFilter::Not(Box::new(StoragePathFilter::Directory(nested.clone()))),
            ]),
            vec![1, 6, 7, 8],
        ),
        (
            StoragePathFilter::Or(vec![
                StoragePathFilter::Directory(nested.clone()),
                StoragePathFilter::FileNameSuffix(".md".into()),
            ]),
            vec![2, 5],
        ),
        (StoragePathFilter::None, vec![]),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::All)),
            vec![],
        ),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::None)),
            (0..files.len()).collect(),
        ),
        (StoragePathFilter::And(vec![]), (0..files.len()).collect()),
        (StoragePathFilter::Or(vec![]), vec![]),
    ];
    for (path, indices) in cases {
        let filter = StorageSearchFilter {
            path: Some(path.clone()),
            ..StorageSearchFilter::default()
        };
        let mut expected = indices
            .into_iter()
            .map(|index| files[index].id)
            .collect::<Vec<_>>();
        expected.sort();
        for hits in [
            storage
                .search_fts("orchard", 20, Some(&filter))
                .expect("FTS"),
            storage
                .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 20, Some(&filter))
                .expect("vector"),
        ] {
            let mut actual = hits.into_iter().map(|hit| hit.file_id).collect::<Vec<_>>();
            actual.sort();
            assert_eq!(actual, expected, "{path:?}");
        }
    }
    storage
        .delete_file(files[2].id)
        .expect("delete nested file");
    let filter = StorageSearchFilter {
        path: Some(StoragePathFilter::Directory(nested.clone())),
        ..StorageSearchFilter::default()
    };
    assert!(
        storage
            .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 20, Some(&filter))
            .expect("no stale vectors")
            .is_empty()
    );
    storage.close().expect("checkpoint");
    let reader = open(directory.path(), true);
    assert!(
        reader
            .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 20, Some(&filter))
            .expect("reopened filter")
            .is_empty()
    );
    assert!(reader.resolve_file_ids(&[PathBuf::from("new.rs")]).is_err());
    reader.close().expect("close reader");
}

#[test]
fn file_ids_are_local_to_index_records_and_rebuilds_are_independent() {
    let temporary = tempfile::tempdir().expect("workspace");
    let first_home = temporary.path().join("first");
    let second_home = temporary.path().join("second");
    let first = open(&first_home, false);
    let (file, entry) = file_at(first.as_ref(), "src/main.rs");
    first.replace_fixture_file(&file, &[entry]).expect("write");
    let reserved = first
        .resolve_file_ids(&[PathBuf::from("pending.rs")])
        .expect("reserve")[0];
    first.close().expect("checkpoint");
    let reopened = open(&first_home, false);
    assert_eq!(
        reopened
            .resolve_file_ids(&[PathBuf::from("src/main.rs")])
            .expect("stored ID"),
        [file.id]
    );
    // An unjournaled reservation is not a durable source record.
    assert_eq!(
        reopened
            .resolve_file_ids(&[PathBuf::from("different.rs")])
            .expect("new ID"),
        [reserved]
    );
    reopened.close().expect("close");
    let second = open(&second_home, false);
    assert_eq!(
        second
            .resolve_file_ids(&[PathBuf::from("unrelated.rs")])
            .expect("new generation"),
        [FileId::new(0)]
    );
    second.close().expect("close");
}

#[test]
fn deleting_a_record_releases_its_path_but_never_reuses_a_live_id() {
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (file, entry) = file_at(storage.as_ref(), "src/main.rs");
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("write");
    storage.delete_file(file.id).expect("delete");
    let (replacement, entry) = file_at(storage.as_ref(), "src/main.rs");
    assert_ne!(file.id, replacement.id);
    storage
        .replace_fixture_file(&replacement, &[entry])
        .expect("replace");
    let (mut mismatched, entry) = file_at(storage.as_ref(), "other.rs");
    mismatched.relative_path = replacement.relative_path.clone();
    assert!(storage.replace_fixture_file(&mismatched, &[entry]).is_err());
    storage.close().expect("checkpoint");
    let reopened = open(temporary.path(), false);
    assert_eq!(
        reopened
            .resolve_file_ids(&[PathBuf::from("src/main.rs")])
            .expect("retained"),
        [replacement.id]
    );
    assert!(
        reopened
            .resolve_file_ids(&[PathBuf::from("new.rs")])
            .expect("new")[0]
            > replacement.id
    );
    reopened.close().expect("close");
}

#[test]
fn failed_files_retain_queryable_paths_and_directory_ownership() {
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (file, _) = file_at(storage.as_ref(), "src/deep/failed.rs");
    let expected = [0, 1];
    storage
        .mark_file_failed(&file, "extraction failed")
        .expect("record failure");
    assert_eq!(
        storage.list_file_paths().expect("paths"),
        [(file.id, file.relative_path.to_path_buf())]
    );
    storage.close().expect("checkpoint");

    let path = temporary.path().join("storage/files");
    let mut options = zvec_rust::CollectionOptions::new().expect("options");
    options.set_read_only(true).expect("read only");
    #[cfg(windows)]
    let path = dunce::simplified(&path);
    let files = zvec_rust::Collection::open(path.to_str().expect("native path"), Some(&options))
        .expect("inspect files");
    let mut docs = files.iter_with_options(None, false).expect("iterator");
    let doc = docs.next().expect("failed file").expect("document");
    for key in expected {
        let mut query = zvec_rust::SearchQuery::scalar(2).expect("query");
        query
            .set_filter(&format!("ancestor_directory_ids CONTAIN_ANY ({key})"))
            .expect("directory filter");
        assert_eq!(files.query(&query).expect("directory membership").len(), 1);
    }
    assert_eq!(
        doc.get_string("file_name").expect("file name"),
        Some("failed.rs".into())
    );
    assert!(docs.next().is_none());
}

#[test]
fn query_attributes_follow_replacement_failure_deletion_and_reopen() {
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (mut file, entry) = file_at(storage.as_ref(), "source.txt");
    file.snapshot.modified_epoch_ms = Some(0);
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("write");
    assert_eq!(
        storage.list_file_attributes().expect("attributes"),
        [StoredFileAttributes::from(&file)]
    );

    file.snapshot.modified_epoch_ms = None;
    storage
        .mark_file_failed(&file, "extractor failed")
        .expect("replace with failed record");
    assert_eq!(
        storage.list_file_attributes().expect("updated attributes"),
        [StoredFileAttributes::from(&file)]
    );
    storage.close().expect("checkpoint");

    let reopened = open(temporary.path(), false);
    assert_eq!(
        reopened
            .list_file_attributes()
            .expect("persisted attributes"),
        [StoredFileAttributes::from(&file)]
    );
    reopened.delete_file(file.id).expect("delete");
    assert!(
        reopened
            .list_file_attributes()
            .expect("deleted attributes")
            .is_empty()
    );
    reopened.close().expect("close");
}

#[test]
fn recovery_validates_source_record_owners_before_mutating_any_collection() {
    let directory = tempfile::tempdir().expect("workspace");
    let home = directory.path();
    let storage = open(home, false);
    let (first, entry) = file_at(storage.as_ref(), "first.rs");
    storage
        .replace_fixture_file(&first, &[entry])
        .expect("first");
    let (mut second, entry) = file_at(storage.as_ref(), "second.rs");
    storage
        .replace_fixture_file(&second, &[entry])
        .expect("second");
    storage.close().expect("checkpoint");
    second.relative_path = crate::domain::SourcePath::new("first.rs").expect("source path");
    let changes = PendingChanges::from([
        (first.id, PendingChange::Delete(first.id)),
        (second.id, PendingChange::Reindex(second)),
    ]);
    pending::write(&home.join("storage"), &changes).expect("corrupted owner intent");
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned(),
            })
            .is_err()
    );
    let native =
        NativeStore::open(&home.join("storage"), &[schema()], true).expect("inspect native data");
    assert_eq!(native.list_files().expect("no mutation").len(), 2);
    assert!(home.join("storage").join(pending::NAME).exists());
}
fn schema() -> EmbeddingModelInfo {
    EmbeddingModelInfo {
        model: crate::domain::model::ModelInfo {
            provider: "fixture".to_owned(),
            name: "fixture-model".to_owned(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    }
}

#[test]
fn stored_model_info_preserves_metadata_and_only_checks_index_fields() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let mut original = schema();
    original.model.endpoint = Some("https://models.example.test/embeddings".into());
    original.metric = Metric::DotProduct;
    original.max_input_tokens = Some(8192);
    original.max_image_bytes = Some(1_048_576);
    let factory = ZvecStorageFactory::new();
    factory
        .open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            embeddings: vec![original.clone()],
        })
        .expect("create index")
        .close()
        .expect("close index");
    let descriptor = home.join("storage/schema.json");
    let persisted = fs::read(&descriptor).expect("read descriptor");
    let record: serde_json::Value = serde_json::from_slice(&persisted).expect("descriptor JSON");
    assert_eq!(record["version"], 4);
    assert_eq!(
        read_json::<SchemaRecord>(&descriptor)
            .expect("read model info")
            .embeddings()
            .expect("valid model info"),
        vec![original.clone()]
    );

    let mut current = original.clone();
    current.model.endpoint = Some("https://new.example.test/embeddings".into());
    current.max_batch_size = 64;

    factory
        .open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            embeddings: vec![current.clone()],
        })
        .expect("runtime metadata changes do not invalidate an index")
        .close()
        .expect("close reused index");

    let mut invalid = current.clone();
    invalid.max_batch_size = 0;
    let error = factory
        .open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            embeddings: vec![invalid],
        })
        .err()
        .expect("reopening must validate incoming metadata too");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(error.message().contains("max_batch_size"));

    for field in [
        "provider",
        "name",
        "dimension",
        "metric",
        "max_input_tokens",
        "max_image_bytes",
    ] {
        let mut changed = current.clone();
        match field {
            "provider" => changed.model.provider = "other".into(),
            "name" => changed.model.name = "other".into(),
            "dimension" => changed.dimension += 1,
            "metric" => changed.metric = Metric::Cosine,
            "max_input_tokens" => changed.max_input_tokens = Some(4096),
            "max_image_bytes" => changed.max_image_bytes = None,
            _ => unreachable!(),
        }
        let error = factory
            .open(WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: home.to_owned(),
                embeddings: vec![changed],
            })
            .err()
            .expect("index fields must match");
        assert!(error.message().contains("rebuild the index"), "{field}");
    }
    assert_eq!(
        fs::read(descriptor).expect("unchanged descriptor"),
        persisted
    );
}

#[test]
fn rejects_corrupt_embedding_limits_before_opening_native_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("storage");
    fs::create_dir(&path).expect("storage directory");
    for field in ["maxBatchSize", "maxInputTokens", "maxImageBytes"] {
        let mut record =
            serde_json::to_value(SchemaRecord::new(&[schema()])).expect("descriptor JSON");
        record["embeddings"][0][field] = serde_json::json!(0);
        fs::write(
            path.join("schema.json"),
            serde_json::to_vec(&record).expect("encode descriptor"),
        )
        .expect("write corrupt descriptor");
        for options in [
            WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: directory.path().to_owned(),
            },
            WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: directory.path().to_owned(),
                embeddings: vec![schema()],
            },
        ] {
            let error = ZvecStorageFactory::new()
                .open(options)
                .err()
                .expect("corrupt metadata");
            assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
            assert!(
                error
                    .message()
                    .contains("invalid stored embedding model information")
            );
        }
    }
    assert_eq!(fs::read_dir(path).expect("storage files").count(), 1);
}

fn open(path: &Path, read_only: bool) -> Box<dyn WorkspaceIndexStorage> {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embeddings: vec![schema()],
        }
    };
    ZvecStorageFactory::new()
        .open(options)
        .expect("open real zvec storage")
}

fn fixture(
    storage: Option<&dyn WorkspaceIndexStorage>,
    name: &str,
    text: &str,
    vector: Vec<f32>,
) -> (FileRecord, FixtureEntity) {
    let relative_path = crate::domain::SourcePath::new(format!("{name}.txt")).expect("source path");
    let id = storage.map_or_else(
        || FileId::new(1),
        |storage| {
            storage
                .resolve_file_ids(&[relative_path.to_path_buf()])
                .expect("reserve file ID")[0]
        },
    );
    let file = FileRecord {
        id,
        relative_path,
        snapshot: FileSnapshot {
            size_bytes: text.len() as u64,
            modified_epoch_ms: Some(1),
            content_hash: Some(crate::utils::sha256_hex(text.as_bytes())),
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    let entry = FixtureEntity {
        model: "fixture/fixture-model".into(),
        entity: Entity {
            id: EntityId::new(format!("entity-{}", id.get())).expect("entity ID"),
            file_id: id,
            source_range: Range::Full,
            fragments: vec![EntityFragment {
                id: FragmentId::new(format!("fragment-{id}")).expect("fragment ID"),
                range: Range::Full,
            }],
            content: Content::Text(text.to_owned()),
            metadata: Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("quoted'\\name\0suffix".to_owned()),
                scope: None,
                signature: None,
                documentation: None,
            })),
        },
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
    let (first, entry) = fixture(
        Some(storage.as_ref()),
        "first",
        "orchard\0苹果 数据库",
        vec![1.0, 0.0, 0.0],
    );
    let (second, other) = fixture(
        Some(storage.as_ref()),
        "second",
        "orchard vineyard",
        vec![0.0, 1.0, 0.0],
    );
    storage
        .replace_fixture_file(&first, std::slice::from_ref(&entry))
        .expect("first file");
    storage
        .replace_fixture_file(&second, &[other])
        .expect("second file");
    let marker = home.join("storage").join(pending::NAME);
    assert!(marker.exists(), "small writes await a checkpoint");

    let filter = StorageSearchFilter {
        path: None,
        file_ids: Some(vec![first.id]),
        entity_ids: Some(vec![entry.entity.id.clone()]),
        symbol_names: Some(vec!["quoted'\\name\0suffix".to_owned()]),
        symbol_types: Some(vec![SymbolType::Function]),
    };
    for query in ["orchard", "数据库", "orchard\0"] {
        let hits = storage
            .search_fts(query, 10, Some(&filter))
            .expect("filtered FTS");
        assert_eq!(hits.len(), 1);
        let loaded = storage.load_search_hits(&hits).expect("FTS result details");
        assert_eq!(
            loaded.fragments[&hits[0].document_id],
            entry.entity.fragments[0]
        );
    }
    let hits = storage
        .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 10, Some(&filter))
        .expect("filtered ANN");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, first.id);
    let loaded = storage.load_search_hits(&hits).expect("ANN result details");
    let stored = &loaded.entities[&hits[0].entity_id];
    assert_eq!(stored.file.snapshot, first.snapshot);
    assert!(stored.file.index_status.is_indexed());
    let ranked = storage
        .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 10, None)
        .expect("ranked ANN");
    assert_eq!(ranked.len(), 2);
    let loaded = storage
        .load_search_hits(&ranked)
        .expect("ranked result details");
    assert_eq!(
        loaded.fragments[&ranked[0].document_id],
        entry.entity.fragments[0]
    );
    for rejected in [
        StorageSearchFilter {
            file_ids: Some(vec![second.id]),
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
                .search_vector(
                    "fixture/fixture-model",
                    &[1.0, 0.0, 0.0],
                    10,
                    Some(&rejected)
                )
                .expect("ANN exclusion")
                .is_empty()
        );
    }
    assert_eq!(loaded.entities[&entry.entity.id].entity, entry.entity);
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
            .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );
    assert!(marker.exists(), "same-session reads must not checkpoint");

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
            .search_vector("fixture/fixture-model", &entry.vector, 10, Some(&filter))
            .expect("old vector removed")
            .is_empty()
    );
    assert_eq!(
        storage.list_files().expect("files")[0].index_status.error(),
        Some("fixture extraction error")
    );
    storage
        .replace_fixture_file(&first, &[entry])
        .expect("retry failed file");
    storage.delete_file(second.id).expect("delete file");
    storage.close().expect("close writer");
    assert!(
        !marker.exists(),
        "normal close checkpoints remaining writes"
    );
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
            .search_vector("fixture/fixture-model", &[1.0, 0.0, 0.0], 10, None)
            .expect("reopened ANN")
            .len(),
        1
    );
    assert!(reader.delete_file(first.id).is_err());
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
#[allow(clippy::too_many_lines)]
fn invalidates_interrupted_batches_before_serving_readers() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, original) = fixture(
        Some(storage.as_ref()),
        "source",
        "old apple",
        vec![1.0, 0.0, 0.0],
    );
    let (deleted, deleted_entry) = fixture(
        Some(storage.as_ref()),
        "deleted",
        "ripe pear",
        vec![0.0, 1.0, 0.0],
    );
    let (unaffected, unaffected_entry) = fixture(
        Some(storage.as_ref()),
        "unaffected",
        "stable vineyard",
        vec![0.0, 0.0, 1.0],
    );
    for (source, entry) in [
        (&file, &original),
        (&deleted, &deleted_entry),
        (&unaffected, &unaffected_entry),
    ] {
        storage
            .replace_fixture_file(source, std::slice::from_ref(entry))
            .expect("initial file");
    }
    let (replacement_file, _) = fixture(
        Some(storage.as_ref()),
        "source",
        "changed apple",
        vec![1.0, 0.0, 0.0],
    );
    let (added, replacement) = fixture(
        Some(storage.as_ref()),
        "added",
        "new banana",
        vec![0.0, 1.0, 0.0],
    );
    storage.close().expect("close writer");
    let path = home.join("storage");
    let changes = PendingChanges::from([
        (file.id, PendingChange::Reindex(replacement_file.clone())),
        (added.id, PendingChange::Reindex(added.clone())),
        (deleted.id, PendingChange::Delete(deleted.id)),
    ]);
    pending::write(&path, &changes).expect("durable batch intent");
    // Persist an incomplete batch: one source was removed, another replaced,
    // and the requested deletion has not started.
    let native = NativeStore::open(&path, &[schema()], false).expect("native writer");
    native.apply_delete(file.id).expect("partial mutation");
    let mut partial = added.clone();
    partial.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    native
        .apply_fixture_file(&partial, std::slice::from_ref(&replacement))
        .expect("uncheckpointed replacement");
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
    assert!(path.join(pending::NAME).exists());
    drop(competing_reader);
    let reader = open(home, true);
    assert!(!path.join(pending::NAME).exists());
    let shared_lock =
        acquire_storage_lock(home, true).expect("recovered reader holds a shared lock");
    assert_eq!(
        acquire_storage_lock(home, false)
            .expect_err("recovered reader excludes writers")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    drop(shared_lock);
    for query in ["apple", "banana", "pear"] {
        assert!(
            reader
                .search_fts(query, 10, None)
                .expect("no partial FTS results")
                .is_empty()
        );
    }
    for (source, entry) in [
        (&file, &original),
        (&added, &replacement),
        (&deleted, &deleted_entry),
    ] {
        let filter = StorageSearchFilter {
            file_ids: Some(vec![source.id]),
            ..StorageSearchFilter::default()
        };
        assert!(
            reader
                .search_vector("fixture/fixture-model", &entry.vector, 10, Some(&filter))
                .expect("no partial vectors")
                .is_empty()
        );
    }
    let recovered = reader.list_files().expect("recovered source metadata");
    assert_eq!(recovered.len(), 3);
    for source in [&replacement_file, &added] {
        let stored = recovered
            .iter()
            .find(|file| file.id == source.id)
            .expect("pending source retained");
        assert_eq!(stored, source);
        let status = &stored.index_status;
        assert_eq!(status.indexed_epoch_ms(), None);
        assert_eq!(status.entity_count(), 0);
        assert_eq!(status.error(), None);
    }
    assert!(!recovered.iter().any(|file| file.id == deleted.id));
    assert_eq!(
        reader
            .search_fts("vineyard", 10, None)
            .expect("unaffected FTS")
            .len(),
        1
    );
    let hits = reader
        .search_vector("fixture/fixture-model", &unaffected_entry.vector, 10, None)
        .expect("unaffected vector");
    assert_eq!(hits.len(), 1);
    let loaded = reader
        .load_search_hits(&hits)
        .expect("unaffected result details");
    assert_eq!(
        loaded.entities[&unaffected_entry.entity.id].entity,
        unaffected_entry.entity
    );
    reader.close().expect("close recovered reader");
    drop(acquire_storage_lock(home, false).expect("closing recovered reader releases its lock"));

    let reader = open(home, true);
    assert_eq!(reader.list_files().expect("durable recovery"), recovered);
    reader.close().expect("close second reader");
    // A corrupt marker must fail closed instead of serving inconsistent collections.
    fs::write(path.join(pending::NAME), b"corrupt pending record").expect("corrupt fixture");
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned(),
            })
            .is_err()
    );
    assert!(path.join(pending::NAME).exists());
}

#[test]
fn prepared_replacements_share_one_durable_marker_and_recover_unwritten_files() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let fixtures = (0..4)
        .map(|index| {
            fixture(
                Some(storage.as_ref()),
                &format!("prepared-{index}"),
                "orchard",
                vec![1.0, 0.0, 0.0],
            )
        })
        .collect::<Vec<_>>();
    let files = fixtures.iter().map(|(file, _)| file).collect::<Vec<_>>();
    let before = pending::WRITE_COUNT.get();
    storage
        .prepare_file_replacements(&files)
        .expect("prepare batch");
    assert_eq!(pending::WRITE_COUNT.get(), before + 1);
    assert_eq!(
        pending::read(&home.join("storage"))
            .expect("durable intent")
            .len(),
        4
    );
    assert!(storage.list_files().expect("not yet published").is_empty());
    storage
        .prepare_file_replacements(&files)
        .expect("repeat hint");
    for (file, entry) in &fixtures[..3] {
        storage
            .replace_fixture_file(file, std::slice::from_ref(entry))
            .expect("prepared write");
    }
    assert_eq!(
        pending::WRITE_COUNT.get(),
        before + 1,
        "no per-file marker rewrite"
    );
    assert_eq!(
        storage
            .search_fts("orchard", 10, None)
            .expect("writer reads its writes")
            .len(),
        3
    );
    drop(storage);

    let reader = open(home, true);
    let recovered = reader.list_files().expect("recovered batch");
    assert_eq!(recovered.len(), 4);
    assert!(recovered.iter().all(|file| !file.index_status.is_indexed()));
    assert!(
        reader
            .search_fts("orchard", 10, None)
            .expect("discard interrupted batch")
            .is_empty()
    );
    reader.close().expect("close recovered reader");
}

#[test]
fn prepared_replacements_are_rejournaled_after_a_checkpoint() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let fixtures = (0..=CHECKPOINT_OPERATIONS)
        .map(|index| {
            fixture(
                Some(storage.as_ref()),
                &format!("prepared-{index}"),
                "orchard",
                vec![1.0, 0.0, 0.0],
            )
        })
        .collect::<Vec<_>>();
    let files = fixtures.iter().map(|(file, _)| file).collect::<Vec<_>>();
    storage
        .prepare_file_replacements(&files)
        .expect("prepare batch");
    let before = pending::WRITE_COUNT.get();
    for (file, entry) in &fixtures[..CHECKPOINT_OPERATIONS] {
        storage
            .replace_fixture_file(file, std::slice::from_ref(entry))
            .expect("prepared write");
    }
    assert_eq!(pending::WRITE_COUNT.get(), before);
    assert!(!home.join("storage").join(pending::NAME).exists());
    let (last, entry) = fixtures.last().expect("last fixture");
    storage
        .replace_fixture_file(last, std::slice::from_ref(entry))
        .expect("fresh intent after checkpoint");
    assert_eq!(pending::WRITE_COUNT.get(), before + 1);
    assert_eq!(
        pending::read(&home.join("storage"))
            .expect("new batch")
            .len(),
        1
    );
    drop(storage);
    let reader = open(home, true);
    let files = reader.list_files().expect("recover only the last write");
    assert_eq!(
        files
            .iter()
            .filter(|file| file.index_status.is_indexed())
            .count(),
        CHECKPOINT_OPERATIONS
    );
    assert!(
        !files
            .iter()
            .find(|file| file.id == last.id)
            .expect("last file")
            .index_status
            .is_indexed()
    );
    reader.close().expect("close reader");
}

#[test]
fn changed_prepared_metadata_and_deletions_refresh_recovery_intent() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (old, _) = fixture(
        Some(storage.as_ref()),
        "source",
        "old orchard",
        vec![1.0, 0.0, 0.0],
    );
    storage
        .prepare_file_replacements(&[&old])
        .expect("prepare old snapshot");
    let before = pending::WRITE_COUNT.get();
    let (latest, entry) = fixture(
        Some(storage.as_ref()),
        "source",
        "new orchard",
        vec![1.0, 0.0, 0.0],
    );
    storage
        .replace_fixture_file(&latest, &[entry])
        .expect("changed snapshot");
    assert_eq!(pending::WRITE_COUNT.get(), before + 1);
    assert_eq!(
        pending::read(&home.join("storage"))
            .expect("latest intent")
            .get(&latest.id),
        Some(&PendingChange::reindex(&latest))
    );
    storage
        .delete_file(latest.id)
        .expect("delete prepared file");
    assert_eq!(pending::WRITE_COUNT.get(), before + 2);
    drop(storage);
    let reader = open(home, true);
    assert!(reader.list_files().expect("deletion wins").is_empty());
    reader.close().expect("close reader");
}

#[test]
fn checkpoints_batches_at_the_operation_limit() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let marker = home.join("storage").join(pending::NAME);
    for index in 0..CHECKPOINT_OPERATIONS {
        let (file, entry) = fixture(
            Some(storage.as_ref()),
            &format!("batch-{index}"),
            "orchard",
            vec![1.0, 0.0, 0.0],
        );
        storage
            .replace_fixture_file(&file, &[entry])
            .expect("batch write");
        assert_eq!(marker.exists(), index + 1 < CHECKPOINT_OPERATIONS);
    }

    let (later, entry) = fixture(
        Some(storage.as_ref()),
        "later",
        "uncheckpointed banana",
        vec![0.0, 1.0, 0.0],
    );
    storage
        .replace_fixture_file(&later, &[entry])
        .expect("next batch");
    assert_eq!(
        pending::read(&home.join("storage"))
            .expect("next batch marker")
            .len(),
        1
    );
    drop(storage);
    assert!(
        marker.exists(),
        "dropping a writer must retain unfinished batch intent"
    );

    let reader = open(home, true);
    assert!(!marker.exists());
    let files = reader.list_files().expect("recovered files");
    assert_eq!(files.len(), CHECKPOINT_OPERATIONS + 1);
    assert_eq!(
        files
            .iter()
            .filter(|file| file.index_status.is_indexed())
            .count(),
        CHECKPOINT_OPERATIONS
    );
    assert_eq!(
        reader
            .search_fts("orchard", CHECKPOINT_OPERATIONS + 1, None)
            .expect("previous checkpoint survives")
            .len(),
        CHECKPOINT_OPERATIONS
    );
    assert!(
        reader
            .search_fts("banana", 10, None)
            .expect("new batch requires reindexing")
            .is_empty()
    );
    reader.close().expect("close reader");
}

#[test]
fn checkpoints_large_sources_before_the_operation_limit() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let marker = home.join("storage").join(pending::NAME);
    for id in ["first-large", "second-large"] {
        let (mut file, entry) = fixture(Some(storage.as_ref()), id, "orchard", vec![1.0, 0.0, 0.0]);
        // Source metadata represents a large file without allocating its full contents.
        file.snapshot.size_bytes = CHECKPOINT_BYTES / 2;
        storage
            .replace_fixture_file(&file, &[entry])
            .expect("large source write");
        assert_eq!(marker.exists(), id == "first-large");
    }
    drop(storage);
    let reader = open(home, true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, None)
            .expect("byte checkpoint persisted both sources")
            .len(),
        2
    );
    reader.close().expect("close reader");
}

#[tokio::test]
async fn finalizes_small_batches_and_preserves_failure_status() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, entry) = fixture(
        Some(storage.as_ref()),
        "source",
        "orchard",
        vec![1.0, 0.0, 0.0],
    );
    let (failed, _) = fixture(
        Some(storage.as_ref()),
        "failed",
        "unreadable",
        vec![0.0, 1.0, 0.0],
    );
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("small write");
    storage
        .mark_file_failed(&failed, "fixture failure")
        .expect("failed source");
    let marker = home.join("storage").join(pending::NAME);
    assert!(marker.exists());
    storage
        .finalize_writes()
        .await
        .expect("explicit checkpoint");
    assert!(!marker.exists());
    storage
        .finalize_writes()
        .await
        .expect("empty checkpoint is idempotent");
    drop(storage);

    let reader = open(home, true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, None)
            .expect("finalized source")
            .len(),
        1
    );
    let files = reader.list_files().expect("finalized file status");
    let stored = files
        .iter()
        .find(|file| file.id == failed.id)
        .expect("failed source remains");
    let status = &stored.index_status;
    assert_eq!(status.error(), Some("fixture failure"));
    assert_eq!(status.indexed_epoch_ms(), None);
    reader.close().expect("close reader");
}

#[test]
fn retains_only_the_latest_intent_for_repeated_file_updates() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    let storage = open(home, false);
    let (file, original) = fixture(
        Some(storage.as_ref()),
        "source",
        "old apple",
        vec![1.0, 0.0, 0.0],
    );
    storage
        .replace_fixture_file(&file, &[original])
        .expect("initial replacement");
    storage.delete_file(file.id).expect("temporary deletion");
    let (mut latest, replacement) = fixture(
        Some(storage.as_ref()),
        "source",
        "new banana",
        vec![0.0, 1.0, 0.0],
    );
    latest.snapshot.modified_epoch_ms = Some(2);
    storage
        .replace_fixture_file(&latest, std::slice::from_ref(&replacement))
        .expect("latest replacement");
    let (deleted, entry) = fixture(
        Some(storage.as_ref()),
        "deleted",
        "ripe pear",
        vec![0.0, 0.0, 1.0],
    );
    storage
        .replace_fixture_file(&deleted, &[entry])
        .expect("another replacement");
    storage.delete_file(deleted.id).expect("final deletion");
    assert_eq!(
        pending::read(&path).expect("pending intentions"),
        PendingChanges::from([
            (file.id, PendingChange::Delete(file.id)),
            (latest.id, PendingChange::Reindex(latest.clone())),
            (deleted.id, PendingChange::Delete(deleted.id)),
        ])
    );
    assert_eq!(
        storage
            .search_fts("banana", 10, None)
            .expect("latest write visible before checkpoint")
            .len(),
        1
    );
    drop(storage);
    assert!(path.join(pending::NAME).exists());

    let reader = open(home, true);
    let files = reader.list_files().expect("recovered latest intention");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], latest);
    assert_eq!(files[0].index_status.indexed_epoch_ms(), None);
    assert!(
        reader
            .search_vector("fixture/fixture-model", &replacement.vector, 10, None)
            .expect("requires fresh embeddings")
            .is_empty()
    );
    assert!(!path.join(pending::NAME).exists());
    reader.close().expect("close reader");
}

#[tokio::test]
async fn failed_marker_write_blocks_access_and_close_releases_the_lease() {
    assert_failed_marker_write(false).await;
}

#[tokio::test]
async fn failed_batch_marker_write_blocks_access_and_close_releases_the_lease() {
    assert_failed_marker_write(true).await;
}

async fn assert_failed_marker_write(prepared: bool) {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    let storage = open(home, false);
    let (file, entry) = fixture(
        Some(storage.as_ref()),
        "source",
        "orchard",
        vec![1.0, 0.0, 0.0],
    );
    storage
        .replace_fixture_file(&file, std::slice::from_ref(&entry))
        .expect("healthy pending write");
    let marker = path.join(pending::NAME);
    let preserved = path.join("preserved-pending.json");
    fs::rename(&marker, &preserved).expect("preserve durable intent");
    fs::create_dir(&marker).expect("obstruct marker replacement");
    let (other, replacement) = fixture(
        Some(storage.as_ref()),
        "other",
        "banana",
        vec![0.0, 1.0, 0.0],
    );
    if prepared {
        assert!(storage.prepare_file_replacements(&[&other]).is_err());
    } else {
        assert!(
            storage
                .replace_fixture_file(&other, &[replacement])
                .is_err()
        );
    }
    for error in [
        storage
            .list_files()
            .expect_err("failed writer cannot list sources"),
        storage
            .search_fts("orchard", 10, None)
            .expect_err("failed writer cannot search"),
        storage
            .search_vector("fixture/fixture-model", &entry.vector, 10, None)
            .expect_err("failed writer cannot search vectors"),
        storage
            .load_search_hits(&[])
            .err()
            .expect("failed writer cannot load search results"),
        storage
            .delete_file(file.id)
            .expect_err("failed writer cannot mutate"),
        storage
            .finalize_writes()
            .await
            .expect_err("failed writer cannot checkpoint"),
    ] {
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    }
    fs::remove_dir(&marker).expect("remove obstruction");
    fs::rename(&preserved, &marker).expect("restore durable intent");
    assert!(
        storage.close().is_err(),
        "closing reports unfinished writes"
    );
    assert!(marker.exists(), "failed close must retain recovery intent");
    drop(acquire_storage_lock(home, false).expect("failed close releases its exclusive lease"));

    let reader = open(home, true);
    let files = reader.list_files().expect("recover earlier writes");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], file);
    assert!(
        reader
            .search_fts("orchard", 10, None)
            .expect("earlier pending write invalidated")
            .is_empty()
    );
    assert!(!marker.exists());
    reader.close().expect("close reader");
}

#[test]
fn invalid_prepared_files_leave_the_writer_usable_and_readers_reject_preparation() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, entry) = fixture(
        Some(storage.as_ref()),
        "source",
        "orchard",
        vec![1.0, 0.0, 0.0],
    );
    let mut invalid = file.clone();
    invalid.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    invalid.snapshot.content_hash = None;
    assert!(
        storage
            .prepare_file_replacements(&[&file, &invalid])
            .is_err()
    );
    assert!(!home.join("storage").join(pending::NAME).exists());
    storage.prepare_file_replacements(&[]).expect("empty hint");
    assert!(!home.join("storage").join(pending::NAME).exists());
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("writer remains usable");
    storage.close().expect("close writer");
    let reader = open(home, true);
    assert!(reader.prepare_file_replacements(&[&file]).is_err());
    assert!(!home.join("storage").join(pending::NAME).exists());
    reader.close().expect("close reader");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "verify invalid writes leave the same pending session usable"
)]
fn rejects_invalid_writes_without_poisoning_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, mut entry) = fixture(
        Some(storage.as_ref()),
        "source",
        "healthy document",
        vec![1.0, 0.0, 0.0],
    );
    for invalid in [vec![1.0], vec![f32::NAN, 0.0, 0.0], vec![0.0, 0.0, 0.0]] {
        entry.vector = invalid;
        assert_eq!(
            storage
                .replace_fixture_file(&file, std::slice::from_ref(&entry))
                .expect_err("invalid vector")
                .code(),
            EngineError::INVALID_ARGUMENT
        );
        assert!(!home.join("storage").join(pending::NAME).exists());
        assert!(
            storage
                .list_files()
                .expect("storage still usable")
                .is_empty()
        );
    }
    entry.vector = vec![1.0, 0.0, 0.0];
    storage
        .replace_fixture_file(&file, std::slice::from_ref(&entry))
        .expect("valid write after rejections");
    let marker = home.join("storage").join(pending::NAME);
    let healthy_pending = fs::read(&marker).expect("healthy pending batch");
    entry.vector = vec![1.0];
    assert_eq!(
        storage
            .replace_fixture_file(&file, &[entry])
            .expect_err("invalid replacement of pending source")
            .code(),
        EngineError::INVALID_ARGUMENT
    );
    assert_eq!(
        fs::read(&marker).expect("pending batch preserved"),
        healthy_pending
    );
    let (_, mut invalid_table) = fixture(
        Some(storage.as_ref()),
        "source",
        "rejected table",
        vec![1.0, 0.0, 0.0],
    );
    let entity = &mut invalid_table.entity;
    entity.content = Content::Table(TableContent {
        row_count: 1,
        column_count: 1,
        cells: vec![TableCell {
            row: 0,
            column: 0,
            row_span: 0,
            column_span: 1,
            contents: vec![Content::Text("invalid zero-height cell".to_owned())],
            kind: TableCellRole::Data,
        }],
    });
    let error = storage
        .replace_fixture_file(&file, &[invalid_table])
        .expect_err("invalid table must be rejected before writing intent");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(error.message().contains("table cell span"));
    assert_eq!(
        fs::read(&marker).expect("invalid table preserves pending batch"),
        healthy_pending
    );
    assert_eq!(
        storage
            .search_fts("healthy", 10, None)
            .expect("invalid input leaves pending writes readable")
            .len(),
        1
    );
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned(),
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
                embeddings: vec![incompatible],
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
fn invalid_file_states_and_owners_never_start_a_pending_batch() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let storage = open(directory.path(), false);
    let (file, entry) = fixture(
        Some(storage.as_ref()),
        "state",
        "source content",
        vec![1.0, 0.0, 0.0],
    );
    let marker = directory.path().join("storage").join(pending::NAME);
    let mut unread = file.clone();
    unread.snapshot.content_hash = None;
    assert!(
        storage
            .replace_fixture_file(&unread, std::slice::from_ref(&entry))
            .is_err()
    );
    let mut other = file.clone();
    other.id = FileId::new(99);
    assert!(
        storage
            .replace_fixture_file(&other, std::slice::from_ref(&entry))
            .is_err()
    );
    let (entities, entries) = fixture_records(std::slice::from_ref(&entry));
    let mut wrong_owner = entries[0].clone();
    wrong_owner.entity_id = EntityId::new("unrelated-owner").expect("owner ID");
    let mut unknown_fragment = entries[0].clone();
    unknown_fragment.fragment_id = FragmentId::new("unknown-fragment").expect("fragment ID");
    for invalid in [
        vec![],
        vec![entries[0].clone(), entries[0].clone()],
        vec![wrong_owner],
        vec![unknown_fragment],
    ] {
        assert!(
            storage.replace_file(&file, &entities, &invalid).is_err(),
            "every canonical fragment requires exactly one projection of its owner"
        );
    }
    assert!(!marker.exists());
    assert!(storage.list_files().expect("no partial records").is_empty());
    storage
        .replace_fixture_file(&file, &[])
        .expect("successful empty extraction");
    storage.close().expect("checkpoint empty result");
    let reader = open(directory.path(), true);
    let files = reader.list_files().expect("read empty indexed source");
    assert_eq!(files.len(), 1);
    assert!(files[0].index_status.is_indexed());
    assert_eq!(files[0].index_status.entity_count(), 0);
    assert_eq!(files[0].snapshot, file.snapshot);
    reader.close().expect("close reader");
}

#[test]
fn native_replacements_require_a_consistent_complete_file_state() {
    let directory = tempfile::tempdir().expect("fixture directory");
    open(directory.path(), false)
        .close()
        .expect("initialize storage");
    let native = NativeStore::open(&directory.path().join("storage"), &[schema()], false)
        .expect("native writer");
    let (mut file, entry) = fixture(None, "state", "source content", vec![1.0, 0.0, 0.0]);
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Failed {
            error: "extraction failed".to_owned(),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 0,
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 2,
        },
    ] {
        file.index_status = status;
        assert!(
            native
                .apply_fixture_file(&file, std::slice::from_ref(&entry))
                .is_err()
        );
    }
    assert!(
        native
            .list_files()
            .expect("no partial mutations")
            .is_empty()
    );
    file.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    native
        .apply_fixture_file(&file, std::slice::from_ref(&entry))
        .expect("consistent result");
    let hits = native
        .search_vector("fixture/fixture-model", &entry.vector, 10, None)
        .expect("stored vector");
    assert_eq!(hits.len(), 1);
    let loaded = native
        .load_search_hits(&hits)
        .expect("stored result details");
    assert_eq!(loaded.entities[&entry.entity.id].file, file);
    file.index_status = FileIndexStatus::NotIndexed;
    native
        .apply_fixture_file(&file, &[])
        .expect("discard interrupted result");
    assert!(
        native
            .search_vector("fixture/fixture-model", &entry.vector, 10, None)
            .expect("no partial vector")
            .is_empty()
    );
    assert_eq!(native.list_files().expect("reindex marker"), vec![file]);
}

#[test]
fn writes_fragments_across_native_batch_boundaries() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, prototype) = fixture(
        Some(storage.as_ref()),
        "batch",
        "harvest",
        vec![1.0, 0.0, 0.0],
    );
    let entries = (0..1025)
        .map(|index| {
            let mut entity = prototype.entity.clone();
            entity.id = EntityId::new(format!("batch-entity-{index}")).expect("entity ID");
            entity.fragments[0].id =
                FragmentId::new(format!("batch-fragment-{index}")).expect("fragment ID");
            FixtureEntity {
                model: "fixture/fixture-model".into(),
                entity,
                vector: prototype.vector.clone(),
            }
        })
        .collect::<Vec<_>>();
    storage
        .replace_fixture_file(&file, &entries)
        .expect("batched write");
    let last = entries.last().expect("last batch entry");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![last.entity.id.clone()]),
        ..StorageSearchFilter::default()
    };
    assert_eq!(
        storage
            .search_fts("harvest", 1030, None)
            .expect("all batches")
            .len(),
        entries.len()
    );
    let hits = storage
        .search_vector(
            "fixture/fixture-model",
            &prototype.vector,
            10,
            Some(&filter),
        )
        .expect("last batch ANN");
    assert_eq!(hits.len(), 1);
    let loaded = storage
        .load_search_hits(&hits)
        .expect("last batch result details");
    assert_eq!(
        loaded.fragments[&hits[0].document_id],
        last.entity.fragments[0]
    );
    storage
        .replace_fixture_file(&file, &[])
        .expect("replace with empty file");
    assert!(
        storage
            .search_fts("harvest", 10, None)
            .expect("no stale fragments")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(
                "fixture/fixture-model",
                &prototype.vector,
                10,
                Some(&filter)
            )
            .expect("no stale vector")
            .is_empty()
    );
    storage.close().expect("close storage");
}

#[cfg(unix)]
#[test]
fn reopened_readers_detect_non_unicode_names_without_loading_the_allocation_cache() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (mut file, _) = fixture(None, "native", "source", vec![1.0, 0.0, 0.0]);
    file.relative_path =
        SourcePath::new(OsString::from_vec(b"src/\xff.rs".to_vec())).expect("native path");
    file.id = storage
        .resolve_file_ids(&[file.relative_path.to_path_buf()])
        .expect("ID")[0];
    storage
        .mark_file_failed(&file, "fixture")
        .expect("file record");
    assert!(storage.has_non_unicode_file_names().expect("writer"));
    storage.close().expect("checkpoint");
    let reader = open(temporary.path(), true);
    assert!(reader.has_non_unicode_file_names().expect("reader"));
    reader.close().expect("close");
    let writer = open(temporary.path(), false);
    writer.delete_file(file.id).expect("delete");
    writer.close().expect("checkpoint");
    let reader = open(temporary.path(), true);
    assert!(!reader.has_non_unicode_file_names().expect("empty reader"));
    reader.close().expect("close");
}

#[test]
fn directory_collection_preserves_membership_after_reopen_and_recovery() {
    let home = tempfile::tempdir().expect("workspace");
    let storage = open(home.path(), false);
    let (file, entry) = file_at(storage.as_ref(), "src/nested/file.rs");
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("indexed source");
    storage.close().expect("checkpoint");
    assert!(home.path().join("storage/directories").is_dir());
    let filter = StorageSearchFilter {
        path: Some(StoragePathFilter::Directory(
            crate::domain::SourcePath::new("src").expect("path"),
        )),
        ..StorageSearchFilter::default()
    };
    let reader = open(home.path(), true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, Some(&filter))
            .expect("directory query")
            .len(),
        1
    );
    reader.close().expect("close reader");
    pending::write(
        &home.path().join("storage"),
        &PendingChanges::from([(file.id, PendingChange::reindex(&file))]),
    )
    .expect("intent");
    let writer = open(home.path(), false);
    let (_, entry) = file_at(writer.as_ref(), "src/nested/file.rs");
    writer
        .replace_fixture_file(&file, &[entry])
        .expect("reindex source");
    writer.close().expect("checkpoint recovered source");
    let reader = open(home.path(), true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, Some(&filter))
            .expect("recovered directory query")
            .len(),
        1
    );
    reader.close().expect("close reader");
}

fn multi_model_schema() -> Vec<EmbeddingModelInfo> {
    let text = schema();
    let mut vision = schema();
    vision.model.name = "vision".into();
    vision.dimension = 2;
    vec![text, vision]
}

fn open_multi_model(path: &Path) -> Box<dyn WorkspaceIndexStorage> {
    ZvecStorageFactory::new()
        .open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embeddings: multi_model_schema(),
        })
        .expect("multi-model storage")
}

fn multi_model_file(storage: &dyn WorkspaceIndexStorage) -> (FileRecord, Vec<FixtureEntity>) {
    let (file, text) = fixture(
        Some(storage),
        "nested/mixed",
        "orchard text",
        vec![1.0, 0.0, 0.0],
    );
    let mut image = text.clone();
    image.model = "fixture/vision".into();
    image.vector = vec![0.0, 1.0];
    let entity = &mut image.entity;
    entity.id = EntityId::new("image-entity").expect("image ID");
    entity.fragments[0].id = FragmentId::new("image-fragment").expect("image fragment ID");
    entity.content = Content::Image(
        crate::domain::ImageContent::new(vec![1, 2, 3], crate::domain::FileFormat::Png)
            .expect("image"),
    );
    (file, vec![text, image])
}

#[test]
fn model_tables_partition_fragments_and_failed_files_clear_every_partition() {
    let home = tempfile::tempdir().expect("workspace");
    let writer = open_multi_model(home.path());
    let (file, entries) = multi_model_file(writer.as_ref());
    writer
        .replace_fixture_file(&file, &entries)
        .expect("complete file");
    writer.close().expect("checkpoint");

    let storage_path = home.path().join("storage");
    let mut collections = fs::read_dir(&storage_path)
        .expect("storage layout")
        .map(|entry| entry.expect("directory entry"))
        .filter(|entry| entry.file_type().expect("type").is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    collections.sort();
    let mut expected = vec![
        "directories".to_owned(),
        "files".to_owned(),
        "entities".to_owned(),
    ];
    expected.extend(
        multi_model_schema()
            .iter()
            .map(super::super::zvec::fragment_collection_name),
    );
    expected.sort();
    assert_eq!(
        collections, expected,
        "three canonical collections plus one per model"
    );

    let reader = open(home.path(), true);
    for (model, vector, id) in [
        (
            "fixture/fixture-model",
            vec![1.0, 0.0, 0.0],
            entries[0].entity.fragments[0].id.as_str(),
        ),
        (
            "fixture/vision",
            vec![0.0, 1.0],
            entries[1].entity.fragments[0].id.as_str(),
        ),
    ] {
        let hits = reader
            .search_vector(model, &vector, 10, None)
            .expect("partition query");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document_id, id);
        let loaded = reader.load_search_hits(&hits).expect("canonical fragment");
        assert_eq!(loaded.fragments[id].id.as_str(), id);
    }
    assert!(
        reader
            .search_vector("fixture/unknown", &[1.0, 0.0], 10, None)
            .is_err()
    );
    assert!(
        reader
            .search_vector("fixture/vision", &[1.0, 0.0, 0.0], 10, None)
            .is_err()
    );
    // Indexed owner metadata is searchable in both disjoint partitions.
    let hits = reader.search_fts("name", 10, None).expect("all-table FTS");
    assert_eq!(hits.len(), 2);
    assert_ne!(hits[0].document_id, hits[1].document_id);
    reader.close().expect("close reader");

    let writer = open_multi_model(home.path());
    writer
        .mark_file_failed(&file, "vision request failed")
        .expect("file failure");
    let failed = writer.list_files().expect("failed files");
    assert!(
        matches!(&failed[0].index_status, FileIndexStatus::Failed { error } if error == "vision request failed")
    );
    assert!(
        writer
            .search_fts("name", 10, None)
            .expect("no FTS remnants")
            .is_empty()
    );
    for entry in &entries {
        assert!(
            writer
                .search_vector(&entry.model, &entry.vector, 10, None)
                .expect("no vector remnants")
                .is_empty()
        );
    }
    writer.close().expect("persist failure");
    let native =
        NativeStore::open(&storage_path, &multi_model_schema(), true).expect("inspect failed file");
    assert_eq!(native.list_files().expect("failure remains").len(), 1);
    assert!(
        native.load_search_hits(&hits).is_err(),
        "canonical entities were removed too"
    );
}

#[test]
fn model_set_changes_require_rebuild_and_order_does_not() {
    let home = tempfile::tempdir().expect("workspace");
    open_multi_model(home.path()).close().expect("checkpoint");
    let mut reversed = multi_model_schema();
    reversed.reverse();
    let factory = ZvecStorageFactory::new();
    factory
        .open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.path().to_owned(),
            embeddings: reversed,
        })
        .expect("same models in different order")
        .close()
        .expect("close");
    for embeddings in [vec![schema()], vec![schema(), schema()]] {
        assert!(
            factory
                .open(WorkspaceIndexStorageOptions::ReadWrite {
                    storage_path: home.path().to_owned(),
                    embeddings,
                })
                .is_err()
        );
    }
}

#[test]
fn pending_multi_model_file_recovery_discards_every_partition() {
    let home = tempfile::tempdir().expect("workspace");
    let writer = open_multi_model(home.path());
    let (file, entries) = multi_model_file(writer.as_ref());
    writer
        .replace_fixture_file(&file, &entries)
        .expect("complete file");
    writer.close().expect("checkpoint");
    pending::write(
        &home.path().join("storage"),
        &PendingChanges::from([(file.id, PendingChange::reindex(&file))]),
    )
    .expect("interrupted replacement intent");
    let reader = open(home.path(), true);
    assert!(matches!(
        reader.list_files().expect("recovered source")[0].index_status,
        FileIndexStatus::NotIndexed
    ));
    for entry in &entries {
        assert!(
            reader
                .search_vector(&entry.model, &entry.vector, 10, None)
                .expect("recovered partition")
                .is_empty()
        );
    }
    assert!(
        reader
            .search_fts("name", 10, None)
            .expect("recovered full text")
            .is_empty()
    );
    reader.close().expect("close reader");
}
