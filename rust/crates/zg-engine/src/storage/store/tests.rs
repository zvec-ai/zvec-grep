use super::super::types::StoragePathFilter;
use super::*;
use crate::domain::SourcePath;
use crate::domain::model::EmbeddingMetric;
use crate::domain::{
    CodeMetadata, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileRecord,
    FileSnapshot, FragmentId, Range, SymbolType,
};
use crate::storage::zvec::native;

/// Test input keeps canonical entities and model outputs separate, as the writer does.
#[derive(Clone)]
struct FixtureEntity {
    entity: Entity,
    kind: ContentKind,
    vector: Vec<f32>,
    fts_text: String,
}

fn fixture_records(
    fixtures: &[FixtureEntity],
) -> (Vec<Entity>, Vec<super::super::types::IndexedFragment>) {
    let entities = fixtures
        .iter()
        .map(|fixture| fixture.entity.clone())
        .collect();
    let entries =
        fixtures
            .iter()
            .flat_map(|fixture| {
                fixture.entity.fragments.iter().map(|fragment| {
                    super::super::types::IndexedFragment {
                        entity_id: fixture.entity.id.clone(),
                        fragment_id: fragment.id.clone(),
                        kind: fixture.entity.content.kind(),
                        vector: fixture.vector.clone(),
                        fts_text: fixture.fts_text.clone(),
                    }
                })
            })
            .collect();
    (entities, entries)
}

impl IndexStore {
    fn replace_fixture_file(
        &self,
        file: &FileRecord,
        fixtures: &[FixtureEntity],
    ) -> EngineResult<()> {
        let (entities, entries) = fixture_records(fixtures);
        self.replace_file(file, &entities, &entries)
    }
}

fn file_at(storage: &IndexStore, path: &str) -> (FileRecord, FixtureEntity) {
    let (mut file, mut entry) = fixture(None, "template", "orchard", vec![1.0, 0.0, 0.0]);
    file.relative_path = crate::domain::SourcePath::new(path).expect("source path");
    file.id = storage
        .resolve_file_ids(&[file.relative_path.to_path_buf()])
        .expect("reserve identity")[0];
    let entity = &mut entry.entity;
    entity.file_id = file.id;
    entity.id = EntityId::new(file.id, &entity.content, entity.source_range).expect("entity id");
    entity.fragments[0].id = FragmentId::new(&entity.id, 0);
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
        let (file, entry) = file_at(&storage, path);
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
                .search_fts(ContentKind::Text, "orchard", 20, Some(&filter))
                .expect("FTS"),
            storage
                .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 20, Some(&filter))
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
            .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 20, Some(&filter))
            .expect("no stale vectors")
            .is_empty()
    );
    storage.close().expect("checkpoint");
    let reader = open(directory.path(), true);
    assert!(
        reader
            .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 20, Some(&filter))
            .expect("reopened filter")
            .is_empty()
    );
    assert!(reader.resolve_file_ids(&[PathBuf::from("new.rs")]).is_err());
    reader.close().expect("close reader");
}

#[test]
fn file_id_reservations_are_durable_and_rebuilds_are_independent() {
    let temporary = tempfile::tempdir().expect("workspace");
    let first_home = temporary.path().join("first");
    let second_home = temporary.path().join("second");
    let first = open(&first_home, false);
    let (file, entry) = file_at(&first, "src/main.rs");
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
    // Reserving an ID consumes it even if its file record is never stored.
    assert_eq!(
        reopened
            .resolve_file_ids(&[PathBuf::from("different.rs")])
            .expect("new ID"),
        [FileId::new(reserved.get() + 1)]
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
fn deleted_file_ids_are_not_reused_after_reopening_a_generation() {
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (retained, retained_entry) = file_at(&storage, "retained.rs");
    storage
        .replace_fixture_file(&retained, &[retained_entry])
        .expect("retained file");
    let (deleted, deleted_entry) = file_at(&storage, "old.rs");
    storage
        .replace_fixture_file(&deleted, std::slice::from_ref(&deleted_entry))
        .expect("old file");
    storage.delete_file(deleted.id).expect("delete highest ID");
    storage.close().expect("close after deletion");

    let reopened = open(temporary.path(), false);
    let (replacement, replacement_entry) = file_at(&reopened, "new.rs");
    assert!(replacement.id > deleted.id);
    assert_eq!(
        replacement_entry.entity.content,
        deleted_entry.entity.content
    );
    assert_ne!(replacement_entry.entity.id, deleted_entry.entity.id);
    reopened
        .replace_fixture_file(&replacement, &[replacement_entry])
        .expect("same content at a new path");
    assert!(
        reopened
            .read_entity(&deleted_entry.entity.id)
            .expect("old entity lookup")
            .is_none()
    );
    reopened
        .delete_file(retained.id)
        .expect("delete retained file");
    reopened
        .delete_file(replacement.id)
        .expect("delete replacement");
    reopened.close().expect("close empty index");

    let empty = open(temporary.path(), false);
    assert!(empty.list_files().expect("no files").is_empty());
    let (new_file, new_entry) = file_at(&empty, "new.rs");
    assert!(new_file.id > replacement.id);
    assert_ne!(new_entry.entity.id, deleted_entry.entity.id);
    empty.close().expect("close");
}

#[test]
fn deleting_a_record_releases_its_path_but_never_reuses_a_live_id() {
    let temporary = tempfile::tempdir().expect("workspace");
    let storage = open(temporary.path(), false);
    let (file, entry) = file_at(&storage, "src/main.rs");
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("write");
    storage.delete_file(file.id).expect("delete");
    let (replacement, entry) = file_at(&storage, "src/main.rs");
    assert_ne!(file.id, replacement.id);
    storage
        .replace_fixture_file(&replacement, &[entry])
        .expect("replace");
    let (mut mismatched, entry) = file_at(&storage, "other.rs");
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
    let (file, _) = file_at(&storage, "src/deep/failed.rs");
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
    let (mut file, entry) = file_at(&storage, "source.txt");
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

fn text_table(embedding: EmbeddingModelInfo) -> IndexTable {
    IndexTable {
        kind: ContentKind::Text,
        embedding,
    }
}

fn schema() -> EmbeddingModelInfo {
    EmbeddingModelInfo {
        space: crate::domain::model::EmbeddingSpace::fixture(),
        retrieval: crate::domain::model::EmbeddingRetrieval::Text,
        model: crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture-model",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
            ],
        )
        .expect("fixture model identity"),
        dimension: 3,
        metric: EmbeddingMetric::Cosine,
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
    original.metric = EmbeddingMetric::DotProduct;
    original.max_input_tokens = Some(8192);
    original.max_image_bytes = Some(1_048_576);
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.to_owned(),
        tables: vec![text_table(original.clone())],
    })
    .expect("create index")
    .close()
    .expect("close index");
    let descriptor = home.join("storage/schema.json");
    let persisted = fs::read(&descriptor).expect("read descriptor");
    let record: serde_json::Value = serde_json::from_slice(&persisted).expect("descriptor JSON");
    assert_eq!(
        record,
        serde_json::json!({"tables": [{"kind": "text", "embedding": original}]})
    );
    assert_eq!(
        read_json::<SchemaRecord>(&descriptor)
            .expect("read model info")
            .tables()
            .expect("valid model info"),
        vec![text_table(original.clone())]
    );

    let mut current = original.clone();
    current.max_batch_size = 64;

    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.to_owned(),
        tables: vec![text_table(current.clone())],
    })
    .expect("runtime metadata changes do not invalidate an index")
    .close()
    .expect("close reused index");

    let mut invalid = current.clone();
    invalid.max_batch_size = 0;
    let error = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.to_owned(),
        tables: vec![text_table(invalid)],
    })
    .err()
    .expect("reopening must validate incoming metadata too");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(error.message().contains("max_batch_size"));

    for field in [
        "provider",
        "name",
        "content_kinds",
        "dimension",
        "metric",
        "max_input_tokens",
        "max_image_bytes",
    ] {
        let mut changed = current.clone();
        match field {
            "provider" => {
                changed.model = crate::domain::model::ModelInfo::new(
                    "other",
                    changed.model.name(),
                    changed.model.content_kinds().iter().copied(),
                )
                .expect("fixture model identity");
            }
            "name" => {
                changed.model = crate::domain::model::ModelInfo::new(
                    changed.model.provider(),
                    "other",
                    changed.model.content_kinds().iter().copied(),
                )
                .expect("fixture model identity");
            }
            "content_kinds" => {
                changed.model = crate::domain::model::ModelInfo::new(
                    changed.model.provider(),
                    changed.model.name(),
                    [crate::domain::ContentKind::Text],
                )
                .expect("different content kinds");
            }
            "dimension" => changed.dimension += 1,
            "metric" => changed.metric = EmbeddingMetric::Cosine,
            "max_input_tokens" => changed.max_input_tokens = Some(4096),
            "max_image_bytes" => changed.max_image_bytes = None,
            _ => unreachable!(),
        }
        let error = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            tables: vec![text_table(changed)],
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
        let mut record = serde_json::to_value(SchemaRecord::new(&[text_table(schema())]))
            .expect("descriptor JSON");
        record["tables"][0]["embedding"][field] = serde_json::json!(0);
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
                tables: vec![text_table(schema())],
            },
        ] {
            let error = IndexStore::open(options).err().expect("corrupt metadata");
            assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
            assert!(
                error
                    .message()
                    .contains("invalid stored index table information")
            );
        }
    }
    assert_eq!(fs::read_dir(path).expect("storage files").count(), 1);
}

#[test]
fn initializes_schema_only_in_empty_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("storage");
    fs::create_dir(&path).expect("empty storage directory");
    open(directory.path(), false)
        .close()
        .expect("initialize empty storage");
    assert!(path.join("schema.json").is_file());
    open(directory.path(), true)
        .close()
        .expect("reopen initialized storage");
}

#[test]
fn rejects_existing_collections_without_schema_before_opening_them() {
    for collection in ["files", "directories", "entities", "fragments_existing"] {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("storage");
        let collection_path = path.join(collection);
        fs::create_dir_all(&collection_path).expect("existing collection");
        let marker = collection_path.join("preserved");
        fs::write(&marker, "existing data").expect("existing collection data");
        for options in [
            WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: directory.path().to_owned(),
            },
            WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: directory.path().to_owned(),
                tables: vec![text_table(schema())],
            },
        ] {
            let error = IndexStore::open(options)
                .err()
                .expect("missing schema must reject existing storage");
            assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
            assert!(error.message().contains("missing its schema"));
            assert!(error.message().contains("rebuild the index"));
            assert!(!path.join("schema.json").exists());
            assert_eq!(
                fs::read_to_string(&marker).expect("preserved data"),
                "existing data"
            );
        }
    }
}

fn open(path: &Path, read_only: bool) -> IndexStore {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            tables: vec![text_table(schema())],
        }
    };
    IndexStore::open(options).expect("open real zvec storage")
}

fn fixture(
    storage: Option<&IndexStore>,
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
    let content = Content::Text(text.to_owned());
    let entity_id = EntityId::new(id, &content, Range::Full).expect("entity id");
    let entry = FixtureEntity {
        fts_text: format!("quoted'\\name\0suffix\n{text}\n"),
        kind: ContentKind::Text,
        entity: Entity {
            id: entity_id.clone(),
            file_id: id,
            source_range: Range::Full,
            fragments: vec![EntityFragment {
                id: FragmentId::new(&entity_id, 0),
                range: Range::Full,
            }],
            content,
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
        Some(&storage),
        "first",
        "orchard\0苹果 数据库",
        vec![1.0, 0.0, 0.0],
    );
    let (second, other) = fixture(
        Some(&storage),
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

    let filter = StorageSearchFilter {
        content_kinds: None,
        path: None,
        file_ids: Some(vec![first.id]),
        entity_ids: Some(vec![entry.entity.id.clone()]),
        symbol_names: Some(vec!["quoted'\\name\0suffix".to_owned()]),
        symbol_types: Some(vec![SymbolType::Function]),
    };
    for query in ["orchard", "数据库", "orchard\0"] {
        let hits = storage
            .search_fts(ContentKind::Text, query, 10, Some(&filter))
            .expect("filtered FTS");
        assert_eq!(hits.len(), 1);
        let loaded = storage.load_search_hits(&hits).expect("FTS result details");
        assert_eq!(
            loaded.fragments[&hits[0].document_id],
            entry.entity.fragments[0]
        );
    }
    let hits = storage
        .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 10, Some(&filter))
        .expect("filtered ANN");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, first.id);
    let loaded = storage.load_search_hits(&hits).expect("ANN result details");
    let stored = &loaded.entities[&hits[0].entity_id];
    assert_eq!(stored.file.snapshot, first.snapshot);
    assert!(stored.file.index_status.is_indexed());
    let ranked = storage
        .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 10, None)
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
            entity_ids: Some(vec![
                EntityId::new(first.id, &Content::Text("missing".into()), Range::Full)
                    .expect("entity id"),
            ]),
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
                .search_fts(ContentKind::Text, "orchard", 10, Some(&rejected))
                .expect("FTS exclusion")
                .is_empty()
        );
        assert!(
            storage
                .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 10, Some(&rejected))
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
            .search_fts(ContentKind::Text, "orchard", 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );

    storage
        .mark_file_failed(&first, "fixture extraction error")
        .expect("mark failed");
    assert!(
        storage
            .search_fts(ContentKind::Text, "数据库", 10, None)
            .expect("old content removed")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(ContentKind::Text, &entry.vector, 10, Some(&filter))
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
    assert_eq!(
        storage.list_files().expect_err("closed lease").code(),
        EngineError::RESOURCE_CLOSED
    );
    let reader = open(home, true);
    assert_eq!(reader.list_files().expect("reopened files").len(), 1);
    assert_eq!(
        reader
            .search_fts(ContentKind::Text, "数据库", 10, None)
            .expect("reopened FTS")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 10, None)
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
    assert!(IndexStore::delete(home).is_err());
    second_reader.close().expect("close other reader");
    IndexStore::delete(home).expect("drop storage");
    assert!(!IndexStore::exists(home).expect("absence"));
}

#[test]
fn checkpoint_preserves_failure_status() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, entry) = fixture(Some(&storage), "source", "orchard", vec![1.0, 0.0, 0.0]);
    let (failed, _) = fixture(Some(&storage), "failed", "unreadable", vec![0.0, 1.0, 0.0]);
    storage
        .replace_fixture_file(&file, &[entry])
        .expect("small write");
    storage
        .mark_file_failed(&failed, "fixture failure")
        .expect("failed source");
    storage.checkpoint().expect("explicit checkpoint");
    storage
        .checkpoint()
        .expect("empty checkpoint is idempotent");
    drop(storage);

    let reader = open(home, true);
    assert_eq!(
        reader
            .search_fts(ContentKind::Text, "orchard", 10, None)
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
fn rejects_invalid_writes_without_poisoning_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, mut entry) = fixture(
        Some(&storage),
        "source",
        "healthy document",
        vec![1.0, 0.0, 0.0],
    );
    for invalid in [vec![1.0], vec![f32::NAN, 0.0, 0.0]] {
        entry.vector = invalid;
        assert_eq!(
            storage
                .replace_fixture_file(&file, std::slice::from_ref(&entry))
                .expect_err("invalid vector")
                .code(),
            EngineError::INVALID_ARGUMENT
        );
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
    let healthy_files = storage.list_files().expect("healthy file records");
    entry.vector = vec![1.0];
    assert_eq!(
        storage
            .replace_fixture_file(&file, &[entry])
            .expect_err("invalid replacement of source")
            .code(),
        EngineError::INVALID_ARGUMENT
    );
    assert_eq!(
        storage.list_files().expect("preserved file records"),
        healthy_files
    );
    assert_eq!(
        storage
            .search_fts(ContentKind::Text, "healthy", 10, None)
            .expect("invalid input leaves accepted writes readable")
            .len(),
        1
    );
    assert!(
        IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.to_owned(),
        })
        .is_err(),
        "readers cannot open during a writer lease"
    );
    storage.close().expect("close writer");
    let mut incompatible = schema();
    incompatible.dimension = 4;
    assert!(
        IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            tables: vec![text_table(incompatible)],
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
fn persists_zero_cosine_vector_without_failing_its_file() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let writer = open(directory.path(), false);
    let (file, entry) = fixture(
        Some(&writer),
        "zero",
        "hexadecimal shading data",
        vec![0.0, 0.0, 0.0],
    );
    writer
        .replace_fixture_file(&file, &[entry])
        .expect("zvec accepts a zero vector");
    writer.close().expect("persist zero vector");

    let reader = open(directory.path(), true);
    let files = reader.list_files().expect("read indexed file");
    assert_eq!(files.len(), 1);
    assert!(files[0].index_status.is_indexed());
    assert_eq!(
        reader
            .search_fts(ContentKind::Text, "hexadecimal", 10, None)
            .expect("search text from zero-vector fragment")
            .len(),
        1
    );
    reader.close().expect("close reader");
}

#[test]
fn invalid_file_states_and_owners_leave_storage_unchanged() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let storage = open(directory.path(), false);
    let (file, entry) = fixture(
        Some(&storage),
        "state",
        "source content",
        vec![1.0, 0.0, 0.0],
    );
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
    let mut duplicate_entity = entry.clone();
    duplicate_entity.entity.fragments[0].id = FragmentId::new(&entry.entity.id, 1);
    let mut duplicate_fragment = entry.clone();
    duplicate_fragment.entity.id =
        EntityId::new(file.id, &Content::Text("other".into()), Range::Full).expect("entity id");
    for (duplicate, expected_error) in [
        (duplicate_entity, "duplicate entity id"),
        (duplicate_fragment, "duplicate fragment id"),
    ] {
        let error = storage
            .replace_fixture_file(&file, &[entry.clone(), duplicate])
            .expect_err("canonical IDs must be unique across the batch");
        assert_eq!(error.message(), expected_error);
    }
    let (entities, entries) = fixture_records(std::slice::from_ref(&entry));
    let mut wrong_owner = entries[0].clone();
    wrong_owner.entity_id =
        EntityId::new(file.id, &Content::Text("unrelated".into()), Range::Full).expect("entity id");
    let mut unknown_fragment = entries[0].clone();
    unknown_fragment.fragment_id = FragmentId::new(&entry.entity.id, 1);
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
fn writes_fragments_across_native_batch_boundaries() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, prototype) = fixture(Some(&storage), "batch", "harvest", vec![1.0, 0.0, 0.0]);
    let entries = (0..1025)
        .map(|index| {
            let mut entity = prototype.entity.clone();
            entity.content = Content::Text(format!("harvest {index}"));
            entity.id =
                EntityId::new(file.id, &entity.content, entity.source_range).expect("entity id");
            entity.fragments[0].id = FragmentId::new(&entity.id, 0);
            FixtureEntity {
                kind: ContentKind::Text,
                entity,
                vector: prototype.vector.clone(),
                fts_text: prototype.fts_text.clone(),
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
            .search_fts(ContentKind::Text, "harvest", 1030, None)
            .expect("all batches")
            .len(),
        entries.len()
    );
    let hits = storage
        .search_vector(ContentKind::Text, &prototype.vector, 10, Some(&filter))
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
            .search_fts(ContentKind::Text, "harvest", 10, None)
            .expect("no stale fragments")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(ContentKind::Text, &prototype.vector, 10, Some(&filter))
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
fn directory_collection_preserves_membership_after_reopen_and_replacement() {
    let home = tempfile::tempdir().expect("workspace");
    let storage = open(home.path(), false);
    let (file, entry) = file_at(&storage, "src/nested/file.rs");
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
            .search_fts(ContentKind::Text, "orchard", 10, Some(&filter))
            .expect("directory query")
            .len(),
        1
    );
    reader.close().expect("close reader");
    let writer = open(home.path(), false);
    let (_, entry) = file_at(&writer, "src/nested/file.rs");
    writer
        .replace_fixture_file(&file, &[entry])
        .expect("reindex source");
    writer.close().expect("checkpoint recovered source");
    let reader = open(home.path(), true);
    assert_eq!(
        reader
            .search_fts(ContentKind::Text, "orchard", 10, Some(&filter))
            .expect("recovered directory query")
            .len(),
        1
    );
    reader.close().expect("close reader");
}

fn multi_model_schema() -> Vec<IndexTable> {
    let mut text = schema();
    text.model = crate::domain::model::ModelInfo::new(
        text.model.provider(),
        text.model.name(),
        [
            crate::domain::ContentKind::Text,
            crate::domain::ContentKind::Code,
            crate::domain::ContentKind::Image,
        ],
    )
    .expect("multimodal fixture");
    text.retrieval = crate::domain::model::EmbeddingRetrieval::TextImage;
    let mut vision = text.clone();
    vision.model = crate::domain::model::ModelInfo::new(
        vision.model.provider(),
        "vision",
        vision.model.content_kinds().iter().copied(),
    )
    .expect("fixture model identity");
    vision.dimension = 2;
    vec![
        text_table(text),
        IndexTable {
            kind: ContentKind::Image,
            embedding: vision,
        },
    ]
}

fn open_multi_model(path: &Path) -> IndexStore {
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: path.to_owned(),
        tables: multi_model_schema(),
    })
    .expect("multi-model storage")
}

fn multi_model_file(storage: &IndexStore) -> (FileRecord, Vec<FixtureEntity>) {
    let (file, text) = fixture(
        Some(storage),
        "nested/mixed",
        "orchard text",
        vec![1.0, 0.0, 0.0],
    );
    let mut image = text.clone();
    image.kind = ContentKind::Image;
    image.vector = vec![0.0, 1.0];
    let entity = &mut image.entity;
    entity.content = Content::Image(
        crate::domain::ImageContent::new(vec![1, 2, 3], crate::domain::FileFormat::Png)
            .expect("image"),
    );
    entity.id = EntityId::new(file.id, &entity.content, entity.source_range).expect("entity id");
    entity.fragments[0].id = FragmentId::new(&entity.id, 0);
    image.fts_text = "quoted'\\name\0suffix\n[image:png]\n".into();
    (file, vec![text, image])
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Verify multi-table persistence, query isolation and failure cleanup in one lifecycle"
)]
fn model_tables_partition_fragments_and_failed_files_clear_every_partition() {
    let home = tempfile::tempdir().expect("workspace");
    let writer = open_multi_model(home.path());
    let (file, entries) = multi_model_file(&writer);
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
            .map(|table| super::super::fragments::fragment_collection_name(table.kind)),
    );
    expected.sort();
    assert_eq!(
        collections, expected,
        "three canonical collections plus one per model"
    );

    let reader = open(home.path(), true);
    for (model, vector, id) in [
        (
            ContentKind::Text,
            vec![1.0, 0.0, 0.0],
            entries[0].entity.fragments[0].id.as_str(),
        ),
        (
            ContentKind::Image,
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
            .search_vector(ContentKind::Code, &[1.0, 0.0], 10, None)
            .is_err()
    );
    assert!(
        reader
            .search_vector(ContentKind::Image, &[1.0, 0.0, 0.0], 10, None)
            .is_err()
    );
    // FTS searches only the selected model; image metadata does not become searchable text.
    let hits = reader
        .search_fts(ContentKind::Text, "name", 10, None)
        .expect("selected FTS");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entity_id, entries[0].entity.id);
    assert!(
        reader
            .search_fts(ContentKind::Image, "name", 10, None)
            .is_err()
    );
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
            .search_fts(ContentKind::Text, "name", 10, None)
            .expect("no FTS remnants")
            .is_empty()
    );
    for entry in &entries {
        assert!(
            writer
                .search_vector(entry.kind, &entry.vector, 10, None)
                .expect("no vector remnants")
                .is_empty()
        );
    }
    writer.close().expect("persist failure");
    let native = open(home.path(), true);
    assert_eq!(native.list_files().expect("failure remains").len(), 1);
    let loaded = native
        .load_search_hits(&hits)
        .expect("removed canonical entities are skipped");
    assert!(loaded.entities.is_empty());
    assert!(loaded.fragments.is_empty());
}

#[test]
fn model_set_changes_require_rebuild_and_order_does_not() {
    let home = tempfile::tempdir().expect("workspace");
    open_multi_model(home.path()).close().expect("checkpoint");
    let mut reversed = multi_model_schema();
    reversed.reverse();
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        tables: reversed,
    })
    .expect("same models in different order")
    .close()
    .expect("close");
    for tables in [vec![text_table(schema())], vec![text_table(schema()); 2]] {
        assert!(
            IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: home.path().to_owned(),
                tables,
            })
            .is_err()
        );
    }
}

#[test]
fn shared_model_kinds_have_separate_tables_and_filter_before_top_k() {
    let home = tempfile::tempdir().expect("workspace");
    let model = multi_model_schema()[0].embedding.clone();
    let writer = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        tables: [ContentKind::Text, ContentKind::Code, ContentKind::Image]
            .into_iter()
            .map(|kind| IndexTable {
                kind,
                embedding: model.clone(),
            })
            .collect(),
    })
    .expect("shared model storage");
    let (file, mut entries) = multi_model_file(&writer);
    entries[1].vector = vec![0.0, 1.0, 0.0];
    let mut code = entries[0].clone();
    code.kind = ContentKind::Code;
    code.entity.content = Content::Code("fn orchard() {}".into());
    code.entity.id = EntityId::new(file.id, &code.entity.content, Range::Full).expect("code ID");
    code.entity.fragments[0].id = FragmentId::new(&code.entity.id, 0);
    entries.push(code);
    writer
        .replace_fixture_file(&file, &entries)
        .expect("three kinds");
    let (other_file, other) = fixture(Some(&writer), "other", "orchard", vec![0.0, 1.0, 0.0]);
    writer
        .replace_fixture_file(&other_file, &[other])
        .expect("other text");
    assert_eq!(
        writer.entity_counts().expect("counts"),
        BTreeMap::from([
            (ContentKind::Text, 2),
            (ContentKind::Code, 1),
            (ContentKind::Image, 1),
        ])
    );
    writer.close().expect("close");
    let reader = open(home.path(), true);
    for (kind, expected) in [
        (ContentKind::Text, &entries[0]),
        (ContentKind::Code, &entries[2]),
        (ContentKind::Image, &entries[1]),
    ] {
        let hits = reader
            .search_vector(kind, &expected.vector, 1, None)
            .expect("kind query");
        assert_eq!(hits[0].entity_id, expected.entity.id);
        let stored = reader.load_search_hits(&hits).expect("snapshot");
        assert_eq!(
            stored.entities[&hits[0].entity_id].entity.content,
            expected.entity.content
        );
        let schema = reader
            .read(|state| native(state.fragments.collection(kind)?.schema(), "schema"))
            .expect("schema");
        assert_eq!(schema.has_field("text"), kind != ContentKind::Image);
        assert_eq!(schema.has_index("text"), kind != ContentKind::Image);
        assert!(
            home.path()
                .join("storage")
                .join(fragments::fragment_collection_name(kind))
                .is_dir()
        );
    }
    let filter = StorageSearchFilter {
        path: Some(StoragePathFilter::FileNameExact("other.txt".into())),
        ..Default::default()
    };
    let hits = reader
        .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 1, Some(&filter))
        .expect("filtered top-k");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, other_file.id);
    for kind in [ContentKind::Text, ContentKind::Code] {
        let hits = reader
            .search_fts(kind, "orchard", 10, None)
            .expect("kind FTS");
        let loaded = reader.load_search_hits(&hits).expect("typed snapshots");
        assert!(
            loaded
                .entities
                .values()
                .all(|owner| owner.entity.content.kind() == kind)
        );
    }
}

#[test]
fn five_mebibyte_canonical_image_roundtrips_and_disappears_after_deletion() {
    use crate::domain::{FileFormat, ImageContent};

    let home = tempfile::tempdir().expect("workspace");
    let writer = open_multi_model(home.path());
    let (mut file, mut entries) = multi_model_file(&writer);
    let image = &mut entries[1].entity;
    let bytes: Vec<u8> = (0_u8..=255).cycle().take(5 * 1024 * 1024).collect();
    file.snapshot.size_bytes = u64::try_from(bytes.len()).expect("size");
    image.content =
        Content::Image(ImageContent::new(bytes.clone(), FileFormat::Png).expect("image"));
    image.id = EntityId::new(file.id, &image.content, Range::Full).expect("identity");
    image.fragments[0].id = FragmentId::new(&image.id, 0);
    let id = image.id.clone();
    writer
        .replace_fixture_file(&file, &entries[1..])
        .expect("persist large image");
    writer.close().expect("flush large image");
    let reader = open(home.path(), true);
    let stored = reader
        .read_entity(&id)
        .expect("canonical read")
        .expect("image present");
    let Content::Image(image) = stored.entity.content else {
        panic!("image content");
    };
    assert_eq!(image.data(), bytes);
    assert_eq!(image.format(), FileFormat::Png);
    assert_eq!(stored.file.relative_path, file.relative_path);
    assert!(
        reader
            .read_entity(&EntityId::from_string("missing".into()))
            .expect("missing read")
            .is_none()
    );
    reader.close().expect("close reader");
    let writer = open_multi_model(home.path());
    writer.delete_file(file.id).expect("delete indexed file");
    assert!(writer.read_entity(&id).expect("deleted read").is_none());
}

#[test]
fn previous_development_schema_requires_rebuild_without_mutating_storage() {
    let home = tempfile::tempdir().expect("workspace");
    let storage = home.path().join("storage");
    fs::create_dir(&storage).expect("storage directory");
    let bytes =
        serde_json::to_vec(&serde_json::json!({"embeddings": [schema()]})).expect("old schema");
    fs::write(storage.join("schema.json"), &bytes).expect("write old schema");
    for options in [
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.path().to_owned(),
        },
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.path().to_owned(),
            tables: vec![text_table(schema())],
        },
    ] {
        let error = IndexStore::open(options).err().expect("reject old schema");
        assert!(error.message().contains("rebuild the index"));
        assert_eq!(
            fs::read(storage.join("schema.json")).expect("unchanged schema"),
            bytes
        );
        assert_eq!(fs::read_dir(&storage).expect("directory").count(), 1);
    }
}

#[test]
fn missing_kind_table_requires_rebuild_instead_of_opening_an_empty_table() {
    let home = tempfile::tempdir().expect("workspace");
    open(home.path(), false).close().expect("create storage");
    let missing = home.path().join("storage/fragments_text");
    fs::remove_dir_all(&missing).expect("simulate lost collection");
    for options in [
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.path().to_owned(),
        },
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.path().to_owned(),
            tables: vec![text_table(schema())],
        },
    ] {
        let error = IndexStore::open(options)
            .err()
            .expect("reject incomplete storage");
        assert!(error.message().contains("rebuild the index"));
        assert!(!missing.exists());
    }
}

#[test]
fn interrupted_kind_replacement_is_hidden_before_top_k_and_recovers_after_reopen() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open_multi_model(home.path());
    let (file, entries) = multi_model_file(&store);
    store
        .replace_fixture_file(&file, &entries)
        .expect("original mixed file");
    let (healthy_file, healthy) = fixture(Some(&store), "healthy", "orchard", vec![0.0, 1.0, 0.0]);
    store
        .replace_fixture_file(&healthy_file, &[healthy])
        .expect("healthy fallback");
    let (entities, projections) = fixture_records(&entries);
    // Emulate interruption after text has been replaced and the image table has
    // not yet been written. The durable file marker keeps both kinds invisible.
    store
        .write(|state| {
            let directories = state.directories.ensure(&file.relative_path)?;
            let mut unfinished = file.clone();
            unfinished.index_status = FileIndexStatus::NotIndexed;
            state.files.put(&unfinished, &directories)?;
            state.files.flush()?;
            state.fragments.delete_file(file.id)?;
            let mut prepared = Fragments::prepare(&file, &entities, &projections, &directories)?;
            prepared.remove(&ContentKind::Image);
            state.fragments.write(&prepared)
        })
        .expect("interrupted replacement");
    store.close().expect("persist partial write");
    let reader = open(home.path(), true);
    let hits = reader
        .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 1, None)
        .expect("filter before top-k");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, healthy_file.id);
    assert!(
        reader
            .search_vector(ContentKind::Image, &[0.0, 1.0], 1, None)
            .expect("hidden image")
            .is_empty()
    );
    assert!(
        reader
            .read_entity(&entities[0].id)
            .expect("hidden snapshot")
            .is_none()
    );
    assert_eq!(
        reader.entity_counts().expect("committed counts"),
        BTreeMap::from([(ContentKind::Text, 1), (ContentKind::Image, 0)])
    );
    reader.close().expect("close reader");
    let writer = open_multi_model(home.path());
    writer
        .replace_fixture_file(&file, &entries)
        .expect("retry complete file");
    writer.close().expect("persist recovered file");
    let reader = open(home.path(), true);
    for entry in entries {
        let hits = reader
            .search_vector(entry.kind, &entry.vector, 1, None)
            .expect("restored kind");
        assert_eq!(hits[0].entity_id, entry.entity.id);
    }
}

#[test]
fn vector_scores_follow_relevance_for_cosine_dot_product_and_euclidean() {
    for metric in [
        EmbeddingMetric::Cosine,
        EmbeddingMetric::DotProduct,
        EmbeddingMetric::Euclidean,
    ] {
        let home = tempfile::tempdir().expect("workspace");
        let mut model = schema();
        model.metric = metric;
        let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.path().to_owned(),
            tables: vec![text_table(model)],
        })
        .expect("metric storage");
        let mut expected = Vec::new();
        for (name, vector) in [
            ("self", vec![1.0, 0.0, 0.0]),
            ("related", vec![0.5, 0.5, 0.0]),
            ("orthogonal", vec![0.0, 1.0, 0.0]),
            ("opposite", vec![-1.0, 0.0, 0.0]),
        ] {
            let (file, entry) = fixture(Some(&store), name, name, vector);
            expected.push(file.id);
            store
                .replace_fixture_file(&file, &[entry])
                .expect("metric document");
        }
        let hits = store
            .search_vector(ContentKind::Text, &[1.0, 0.0, 0.0], 4, None)
            .expect("metric query");
        assert_eq!(
            hits.iter().map(|hit| hit.file_id).collect::<Vec<_>>(),
            expected,
            "{metric:?}"
        );
        assert!(
            hits.windows(2).all(|pair| pair[0].score > pair[1].score),
            "{metric:?}: {hits:?}"
        );
        let self_score = if metric == EmbeddingMetric::DotProduct {
            1.0
        } else {
            0.0
        };
        assert!(
            (hits[0].score - self_score).abs() < 1e-6,
            "{metric:?}: {hits:?}"
        );
    }
}
