use super::*;
use crate::domain::{
    ByteRange, CodeMetadata, Content, EntityFragment, EntityId, EntityMetadata, FileSnapshot,
    FragmentId, Range, SourcePath, SymbolType, TextRange,
};
use crate::storage::{
    path::encode_path,
    types::StoragePathFilter,
    zvec::{doc_key, fetch_map, native, string_field, write_docs},
};
use zvec_rust::{Doc, SearchQuery};

fn model() -> EmbeddingModelInfo {
    EmbeddingModelInfo {
        model: crate::domain::model::ModelInfo {
            provider: "fixture".into(),
            name: "fixture".into(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    }
}

fn metadata_store(path: &Path) -> IndexStore {
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: path.to_owned(),
        embeddings: vec![model()],
    })
    .expect("metadata storage")
}

impl IndexStore {
    fn apply_fixture_records(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        assert_eq!(
            self.resolve_file_ids(&[file.relative_path.to_path_buf()])?,
            [file.id]
        );
        let shared = self.shared()?;
        validate_batch(file, entities, entries, &shared.schema)?;
        self.write(|state| super::replace_file(state, file, entities, entries))
    }
}

fn entity_document(store: &IndexStore, id: &str) -> Doc {
    store
        .read(|state| {
            Ok(fetch_map(state.entities.collection(), &[id.to_owned()])?
                .remove(id)
                .expect("stored entity"))
        })
        .expect("fetch entity")
}

fn fragment_document(store: &IndexStore, model: &str, id: &str) -> Doc {
    store
        .read(|state| {
            Ok(
                fetch_map(state.fragments.collection(model)?, &[id.to_owned()])?
                    .remove(id)
                    .expect("stored fragment"),
            )
        })
        .expect("fetch fragment")
}

fn file(id: u32, path: impl Into<PathBuf>) -> FileRecord {
    FileRecord {
        id: FileId::new(id),
        relative_path: crate::domain::SourcePath::new(path).expect("source path"),
        snapshot: FileSnapshot {
            size_bytes: 0,
            modified_epoch_ms: None,
            content_hash: None,
        },
        index_status: FileIndexStatus::NotIndexed,
    }
}

fn metadata_fragments(
    file_id: u32,
    label: &str,
) -> (FileRecord, Vec<Entity>, Vec<IndexedFragment>) {
    let mut source = file(file_id, "harvest.rs");
    source.snapshot.size_bytes = 100;
    source.snapshot.content_hash = Some("fixture-hash".into());
    source.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    let mut text = "Harvest outline".to_owned();
    text.push_str(&" ".repeat(20 - text.len()));
    text.push_str("orchard fruit");
    text.push_str(&" ".repeat(40 - text.len()));
    text.push_str(label);
    text.push_str(&" ".repeat(100 - text.len()));
    let content = Content::Text(text);
    let source_range =
        Range::Text(TextRange::from_coordinates(0, 100, 1, 1, 0, 100).expect("owner range"));
    let id = EntityId::new(source.id, &content, source_range).expect("entity id");
    let owner = Entity {
        id: id.clone(),
        file_id: source.id,
        source_range,
        content,
        metadata: Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("harvest 春'\\crop".into()),
            scope: Some("Garden".into()),
            signature: Some("pub async fn harvest() -> Crop".into()),
            documentation: Some("Produces the seasonal crop.".into()),
        })),
        fragments: vec![
            EntityFragment {
                id: FragmentId::new(&id, 0),
                range: Range::Byte(ByteRange::new(0, 15).expect("ordered byte offsets")),
            },
            EntityFragment {
                id: FragmentId::new(&id, 1),
                range: Range::Byte(ByteRange::new(20, 40).expect("ordered byte offsets")),
            },
        ],
    };
    let entries = owner
        .fragments
        .iter()
        .zip([
            (vec![0.0, 1.0, 0.0], "Harvest outline\n"),
            (vec![1.0, 0.0, 0.0], "orchard fruit        \n"),
        ])
        .map(|(fragment, (vector, text))| IndexedFragment {
            fts_text: format!("harvest 春'\\crop\nGarden\npub async fn harvest() -> Crop\nProduces the seasonal crop.\n{text}"),
            model: "fixture/fixture".into(),
            entity_id: owner.id.clone(),
            fragment_id: fragment.id.clone(),
            vector,
        })
        .collect();
    (source, vec![owner], entries)
}

fn assert_projection(store: &IndexStore, entry: &IndexedFragment) {
    let id = entry.fragment_id.as_str();
    let doc = fragment_document(store, "fixture/fixture", id);
    assert_eq!(
        string_field(&doc, "text").expect("prepared searchable text"),
        entry.fts_text.replace('\0', " "),
    );
    assert!(
        !doc.has_field("payload"),
        "search tables contain only projections"
    );
    {
        assert!(!doc.has_field("metadata"));
        assert_eq!(
            string_field(&doc, "symbol_name").expect("indexed owner name"),
            crate::utils::sha256_hex_parts([
                b"symbol".as_slice(),
                b"\0",
                "harvest 春'\\crop".as_bytes()
            ])
        );
        assert_eq!(
            string_field(&doc, "symbol_type").expect("indexed owner type"),
            "function"
        );
    }
}

#[test]
fn stores_shared_metadata_once_and_filters_windows_by_owner_fields() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, mut entries) = metadata_fragments(0, "owner");
    entries[0].fts_text = "prepared\0projection\n".into();
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write file");
    store.checkpoint().expect("flush storage");
    let owner = &entities[0];
    let entity_doc = entity_document(&store, owner.id.as_str());
    assert_eq!(
        Some(
            serde_json::from_str::<EntityMetadata>(
                &string_field(&entity_doc, "metadata").expect("metadata field")
            )
            .expect("metadata JSON")
        ),
        owner.metadata
    );
    assert_eq!(
        store
            .read(|state| fetch_map(
                state.entities.collection(),
                &[entities[0].fragments[1].id.as_str().to_owned()]
            ))
            .expect("window entity lookup")
            .len(),
        0
    );
    for entry in &entries {
        assert_projection(&store, entry);
    }

    let filter = StorageSearchFilter {
        symbol_names: Some(vec!["harvest 春'\\crop".into()]),
        symbol_types: Some(vec![SymbolType::Function]),
        ..StorageSearchFilter::default()
    };
    let hits = store
        .search_fts("orchard", 10, Some(&filter))
        .expect("filtered FTS");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, entities[0].fragments[1].id.as_str());
    let vectors = store
        .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter))
        .expect("filtered vector retrieval");
    assert_eq!(vectors.len(), 2);
    assert!(
        vectors
            .iter()
            .any(|hit| hit.document_id == entities[0].fragments[1].id.as_str())
    );
    let loaded = store
        .load_search_hits(&hits)
        .expect("selected window details");
    assert_eq!(loaded.entities.len(), 1);
    assert_eq!(loaded.entities[&owner.id].entity, *owner);
    assert_eq!(loaded.entities[&owner.id].file, source);
    assert_eq!(
        loaded.fragments[entities[0].fragments[1].id.as_str()],
        entities[0].fragments[1]
    );

    for rejected in [
        StorageSearchFilter {
            symbol_names: Some(vec!["harvest".into()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_types: Some(vec![SymbolType::Class]),
            ..filter
        },
    ] {
        assert!(
            store
                .search_fts("orchard", 10, Some(&rejected))
                .expect("filtered FTS")
                .is_empty()
        );
        assert!(
            store
                .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&rejected))
                .expect("filtered vector retrieval")
                .is_empty()
        );
    }
}

#[test]
fn retrieval_defers_corrupt_metadata_until_selected_details_are_loaded() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write file");
    let mut doc = entity_document(&store, entities[0].id.as_str());
    doc.add_string("metadata", "invalid metadata JSON")
        .expect("corrupt metadata");
    store
        .write(|state| state.entities.write(&[doc]))
        .expect("write corrupt metadata");
    store.checkpoint().expect("flush storage");

    for hits in [
        store
            .search_fts("orchard", 10, None)
            .expect("lightweight FTS"),
        store
            .search_vector("fixture/fixture", &entries[1].vector, 10, None)
            .expect("lightweight vectors"),
    ] {
        let window = hits
            .iter()
            .find(|hit| hit.document_id == entities[0].fragments[1].id.as_str())
            .expect("window recalled without decoding metadata");
        assert_eq!(window.entity_id, entities[0].id);
        let error = store
            .load_search_hits(std::slice::from_ref(window))
            .err()
            .expect("selected corrupt details must fail");
        assert!(error.message().contains("invalid entity metadata"));
    }
}

#[test]
fn native_search_and_loading_preserve_compact_ids_without_reencoding() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    let entity_id = &entities[0].id;
    let window_id = entities[0].fragments[1].id.as_str();
    assert_eq!(entity_id.as_str().len(), 32);
    assert_eq!(window_id.len(), 40);
    assert!(entity_id.as_str().starts_with("00000000"));
    assert_eq!(window_id, format!("{}00000001", entity_id.as_str()));
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write compact IDs");
    store.checkpoint().expect("flush storage");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![entity_id.clone()]),
        ..StorageSearchFilter::default()
    };
    for hits in [
        store
            .search_fts("orchard", 10, Some(&filter))
            .expect("ID-filtered FTS"),
        store
            .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter))
            .expect("ID-filtered vectors"),
    ] {
        assert!(hits.iter().all(|hit| &hit.entity_id == entity_id));
        let window = hits
            .iter()
            .find(|hit| hit.document_id == window_id)
            .expect("window ID preserved");
        let loaded = store
            .load_search_hits(std::slice::from_ref(window))
            .expect("load compact IDs");
        let owner = &entities[0];
        assert_eq!(loaded.entities[&owner.id].entity, *owner);
        assert_eq!(loaded.fragments[window_id], owner.fragments[1]);
    }
    let entity_doc = entity_document(&store, entity_id.as_str());
    assert_eq!(
        doc_key(&entity_doc).expect("entity primary key"),
        entity_id.as_str()
    );
    assert_eq!(
        string_field(&entity_doc, "entity_id").expect("stored owner ID"),
        entity_id.as_str()
    );
    let doc = fragment_document(&store, "fixture/fixture", window_id);
    assert_eq!(doc_key(&doc).expect("fragment primary key"), window_id);
    assert_eq!(
        string_field(&doc, "entity_id").expect("stored owner ID"),
        entity_id.as_str()
    );
    assert_eq!(
        string_field(&doc, "document_id").expect("stored window ID"),
        window_id
    );
}

#[test]
fn entity_content_is_canonical_and_fragments_have_one_model() {
    let home = tempfile::tempdir().expect("storage");
    let text = EmbeddingModelInfo {
        model: crate::domain::model::ModelInfo {
            provider: "fixture".into(),
            name: "fixture".into(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    };
    let mut other = text.clone();
    other.model.name = "other".into();
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        embeddings: vec![text, other],
    })
    .expect("two models");
    let (source, entities, mut entries) = metadata_fragments(0, "owner");
    entries[1].model = "fixture/other".into();
    assert!(
        fragments::validate_projections(&entities, &entries).is_err(),
        "one entity cannot span model tables"
    );
    assert!(store.list_files().expect("unmodified").is_empty());
    entries[1].model = "fixture/fixture".into();
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("one model owns entire entity");
    let hits = store.search_fts("orchard", 10, None).expect("window match");
    assert_eq!(hits.len(), 1);
    let loaded = store.load_search_hits(&hits).expect("canonical bundle");
    assert_eq!(
        loaded.fragments.len(),
        2,
        "one owner read loads all its fragments"
    );
    assert_eq!(
        loaded.fragments[entities[0].fragments[0].id.as_str()],
        entities[0].fragments[0]
    );
    assert_eq!(
        loaded.fragments[entities[0].fragments[1].id.as_str()],
        entities[0].fragments[1]
    );
    store
        .read(|state| {
            for model in ["fixture/fixture", "fixture/other"] {
                let schema = state.fragments.collection(model)?.schema().expect("schema");
                assert!(schema.has_index("text"));
                assert!(schema.has_index("embedding"));
                assert!(!schema.has_field("payload"));
            }
            Ok(())
        })
        .expect("search schemas");
    // Removing a derived projection does not remove or redefine the canonical fragment.
    store
        .write(|state| state.fragments.delete_file(source.id))
        .expect("remove projection");
    assert_eq!(
        store
            .load_search_hits(&hits)
            .expect("canonical data survives")
            .fragments,
        loaded.fragments
    );
}

#[test]
fn fragment_ids_cannot_be_reused_by_another_file_in_a_different_model_table() {
    let home = tempfile::tempdir().expect("storage");
    let first = EmbeddingModelInfo {
        model: crate::domain::model::ModelInfo {
            provider: "fixture".into(),
            name: "fixture".into(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    };
    let mut second = first.clone();
    second.model.name = "other".into();
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        embeddings: vec![first, second],
    })
    .expect("two models");
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("original owner");
    let mut foreign_source = source.clone();
    foreign_source.id = FileId::new(1);
    foreign_source.relative_path = SourcePath::new("other.rs").expect("path");
    let mut foreign_entities = entities.clone();
    foreign_entities[0].file_id = foreign_source.id;
    foreign_entities[0].id = EntityId::new(
        foreign_source.id,
        &foreign_entities[0].content,
        foreign_entities[0].source_range,
    )
    .expect("entity id");
    let mut foreign = entries.clone();
    for entry in &mut foreign {
        entry.model = "fixture/other".into();
        entry.entity_id = foreign_entities[0].id.clone();
    }
    assert!(
        store
            .apply_fixture_records(&foreign_source, &foreign_entities, &foreign)
            .is_err()
    );
    assert_eq!(store.list_files().expect("original file").len(), 1);
    assert_eq!(
        store
            .search_fts("orchard", 10, None)
            .expect("original fragments")
            .len(),
        1
    );
    assert!(
        store
            .search_vector("fixture/other", &[1.0, 0.0, 0.0], 10, None)
            .expect("no foreign fragments")
            .is_empty()
    );
}

#[test]
fn interrupted_replacements_remain_readable_and_retry_removes_stale_fragments() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (mut source, entities, entries) = metadata_fragments(0, "old-owner");
    source.relative_path = SourcePath::new("src/nested/harvest.rs").expect("path");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let old_hits = store.search_fts("orchard", 10, None).expect("old hits");

    // Simulate interruption after the retry marker but before replacing old rows.
    let mut unfinished = source.clone();
    unfinished.index_status = FileIndexStatus::NotIndexed;
    store
        .write(|state| {
            let directories = state.directories.ensure(&unfinished.relative_path)?;
            state.files.put(&unfinished, &directories)
        })
        .expect("retry state");
    store.checkpoint().expect("persist interrupted state");
    drop(store);

    let store = metadata_store(home.path());
    assert_eq!(
        store.list_files().expect("unchanged retry state"),
        [unfinished]
    );
    assert_eq!(
        store
            .load_search_hits(&old_hits)
            .expect("partial data remains readable")
            .entities[&entities[0].id]
            .entity,
        entities[0],
    );
    let (_, mut replacement, mut projections) = metadata_fragments(0, "new-owner");
    replacement[0].fragments.truncate(1);
    projections.truncate(1);
    store
        .apply_fixture_records(&source, &replacement, &projections)
        .expect("retry replacement");
    store
        .apply_fixture_records(&source, &replacement, &projections)
        .expect("idempotent retry");
    assert_eq!(store.list_files().expect("completed file"), [source]);
    assert!(
        store
            .search_fts("orchard", 10, None)
            .expect("stale text removed")
            .is_empty()
    );
    assert!(
        store
            .load_search_hits(&old_hits)
            .expect("missing old owners are skipped")
            .entities
            .is_empty()
    );
    let new_hits = store.search_fts("Harvest", 10, None).expect("new results");
    assert_eq!(new_hits.len(), 1);
    assert_eq!(new_hits[0].entity_id, replacement[0].id);
}

#[test]
fn interrupted_deletions_keep_identity_until_retry_finishes() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let mut deleting = source.clone();
    deleting.index_status = FileIndexStatus::Deleting;
    store
        .write(|state| state.files.mark_deleting(source.id))
        .expect("delete intent");
    // One collection can be cleared while another still contains old results.
    store
        .write(|state| state.entities.delete_file(source.id))
        .expect("delete entities");
    store.checkpoint().expect("persist interruption");
    drop(store);

    let store = metadata_store(home.path());
    assert_eq!(
        store.list_files().expect("deletion still pending"),
        [deleting]
    );
    let hits = store.search_fts("orchard", 10, None).expect("stale hit");
    assert_eq!(hits.len(), 1);
    assert!(
        store
            .load_search_hits(&hits)
            .expect("missing entities are skipped")
            .entities
            .is_empty()
    );
    store.delete_file(source.id).expect("finish deleting");
    store.delete_file(source.id).expect("repeat deletion");
    assert!(store.list_files().expect("no files").is_empty());
    assert!(
        store
            .search_fts("orchard", 10, None)
            .expect("no stale hits")
            .is_empty()
    );
}

#[test]
fn result_loading_skips_missing_files_and_canonical_fragments() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, mut entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let hits = store.search_fts("orchard", 10, None).expect("window hit");
    entities[0].fragments.pop();
    store
        .write(|state| state.entities.write(&Entities::prepare(&entities)?))
        .expect("partial replacement");
    let loaded = store
        .load_search_hits(&hits)
        .expect("missing canonical fragment is skipped");
    assert!(!loaded.fragments.contains_key(&hits[0].document_id));

    store
        .write(|state| state.files.delete(source.id))
        .expect("missing file");
    let loaded = store
        .load_search_hits(&hits)
        .expect("missing owner file is skipped");
    assert!(loaded.entities.is_empty());
    assert!(loaded.fragments.is_empty());
}

#[test]
fn maximum_u32_directory_id_survives_reopen_and_filters_both_retrieval_collections() {
    let temporary = tempfile::tempdir().expect("storage");
    metadata_store(temporary.path())
        .close()
        .expect("create tables");
    let directory_table =
        Directories::open(&temporary.path().join("storage"), false).expect("directory table");
    let mut doc = Doc::new().expect("directory doc");
    doc.set_pk(&format!("d{}", u32::MAX));
    doc.add_u32("directory_id", u32::MAX).expect("directory ID");
    doc.add_string(
        "path",
        &encode_path(&SourcePath::new("edge").expect("path")).expect("path record"),
    )
    .expect("directory path");
    write_docs(
        directory_table.collection(),
        &[doc],
        "seed maximum directory ID",
    )
    .expect("stored directory");
    directory_table.flush().expect("persist directory");
    drop(directory_table);

    let store = metadata_store(temporary.path());
    let (mut source, entities, entries) = metadata_fragments(0, "owner");
    source.relative_path = SourcePath::new("edge/harvest.rs").expect("path");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("maximum membership");
    store.checkpoint().expect("checkpoint");
    store
        .write(|state| {
            native(state.files.collection().optimize(), "optimize files")?;
            native(
                state.directories.collection().optimize(),
                "optimize directories",
            )?;
            native(
                state.fragments.collection("fixture/fixture")?.optimize(),
                "optimize membership",
            )
        })
        .expect("optimize tables");
    store.close().expect("close writer");
    let reader = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
        storage_path: temporary.path().to_owned(),
    })
    .expect("reopen");
    let filter = StorageSearchFilter {
        path: Some(StoragePathFilter::Directory(
            SourcePath::new("edge").expect("path"),
        )),
        ..StorageSearchFilter::default()
    };
    assert_eq!(
        reader
            .search_fts("orchard", 10, Some(&filter))
            .expect("FTS membership")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector("fixture/fixture", &[1.0, 0.0, 0.0], 10, Some(&filter))
            .expect("vector membership")
            .len(),
        2
    );
    let docs = reader
        .read(|state| {
            native(
                state
                    .files
                    .collection()
                    .query(&SearchQuery::scalar(1).expect("query")),
                "read source records",
            )
        })
        .expect("source records");
    assert_eq!(
        docs[0].get_u32("file_id").expect("u32 file ID"),
        Some(source.id.get())
    );
    assert_eq!(
        docs[0]
            .get_array_u32("ancestor_directory_ids")
            .expect("u32 ancestors"),
        Some(vec![u32::MAX])
    );
}
