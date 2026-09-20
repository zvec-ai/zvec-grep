use super::*;
use crate::domain::{
    CodeMetadata, Entity, FileIndexStatus, FileSnapshot, FragmentId, IndexField, MarkdownMetadata,
    SourceRange, SymbolType, TextRange, WindowFragment,
};

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

#[test]
fn source_file_projection_preserves_paths_and_directory_membership_for_all_statuses() {
    super::super::backend::initialize().expect("initialize zvec");
    assert!(!files_schema().expect("file schema").has_field("formats"));
    let directories = [DirectoryId::new(0), DirectoryId::new(1)];
    let mut source = file(12, Path::new("src").join("nested").join("name.rs"));
    source.snapshot.content_hash = Some("fixture-hash".into());
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Failed {
            error: "extractor unavailable".into(),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 0,
        },
    ] {
        source.index_status = status;
        let doc = encode_file_doc(&source, &directories).expect("encode source");
        assert_eq!(doc.get_pk(), Some("f12"));
        assert!(!doc.has_field("formats"));
        assert_eq!(
            string_field(&doc, "relative_path").expect("query path"),
            "src/nested/name.rs"
        );
        assert_eq!(
            string_field(&doc, "file_name").expect("file name"),
            "name.rs"
        );
        assert_eq!(
            doc.get_array_u32("ancestor_directory_ids")
                .expect("directory IDs"),
            Some(vec![0, 1])
        );
        assert_eq!(decode_file_doc(&doc).expect("decode source"), source);
    }
    let root_file = file(13, "main.rs");
    let root = encode_file_doc(&root_file, &[]).expect("root source");
    assert!(root.has_field("ancestor_directory_ids"));
}

#[test]
fn query_projections_read_all_paths_and_optional_times_without_decoding_payloads() {
    super::super::backend::initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let storage_path = temporary.path().join("storage");
    std::fs::create_dir(&storage_path).expect("storage directory");
    let store = NativeStore::open(
        &storage_path,
        &[EmbeddingModelInfo {
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
        }],
        false,
    )
    .expect("open storage");
    let mut expected = Vec::new();
    let mut expected_attributes = Vec::new();
    let mut docs = Vec::new();
    for index in 1..=WRITE_BATCH + 7 {
        let mut source = file(
            u32::try_from(index).expect("ID"),
            format!("file-{index}.rs"),
        );
        source.snapshot.modified_epoch_ms = match index % 3 {
            0 => None,
            1 => Some(0),
            _ => Some(u64::try_from(index).expect("modification time")),
        };
        let mut doc = encode_file_doc(&source, &[]).expect("encode source");
        doc.add_string("payload", "invalid full file payload")
            .expect("replace payload");
        docs.push(doc);
        expected_attributes.push(StoredFileAttributes::from(&source));
        expected.push((source.id, source.relative_path.into_path_buf()));
    }
    write_docs(&store.files, &docs, "write source").expect("write projections");
    let mut actual = store.list_file_paths().expect("read lightweight paths");
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    let mut attributes = store
        .list_file_attributes()
        .expect("read light file attributes");
    attributes.sort_unstable_by_key(|file| file.id);
    expected_attributes.sort_unstable_by_key(|file| file.id);
    assert_eq!(attributes, expected_attributes);
    assert!(store.list_files().is_err());
}

#[cfg(unix)]
#[test]
fn non_unicode_file_projection_keeps_native_path_without_a_lossy_query_value() {
    use std::os::unix::ffi::OsStringExt;

    super::super::backend::initialize().expect("initialize zvec");
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"src/\xff.rs".to_vec()));
    let source = file(9, path.clone());
    let doc = encode_file_doc(
        &source,
        &DirectoryIds::default()
            .resolve(&source.relative_path)
            .expect("ancestors"),
    )
    .expect("encode non-Unicode source");
    assert!(!doc.has_field("relative_path"));
    // The native getter exposes an empty STRING as None, despite the field
    // being present and non-null. It is only a disabled query projection.
    assert!(doc.has_field("file_name"));
    assert!(!doc.is_field_null("file_name"));
    assert_eq!(
        doc.get_string("file_name")
            .expect("name")
            .unwrap_or_default(),
        ""
    );
    assert_eq!(
        decode_file_path_doc(&doc).expect("native path"),
        (source.id, path)
    );
    assert_eq!(decode_file_doc(&doc).expect("full source"), source);
}

#[test]
fn full_file_decode_rejects_a_path_projection_from_another_file() {
    super::super::backend::initialize().expect("initialize zvec");
    let source = file(2, "first.rs");
    let mut doc = encode_file_doc(&source, &[]).expect("encode source");
    doc.add_string(
        "path",
        &encode_path(&crate::domain::SourcePath::new("second.rs").expect("source path"))
            .expect("path"),
    )
    .expect("replace path projection");
    assert!(decode_file_doc(&doc).is_err());
}

#[test]
fn full_range_ids_support_native_queries_membership_and_deletion() {
    super::super::backend::initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let collection = open_collection(
        &temporary.path().join("files"),
        &files_schema().expect("file schema"),
        false,
    )
    .expect("file collection");
    let ids = [0, 1, i32::MAX as u32 + 1, u32::MAX - 1, u32::MAX];
    let sources: Vec<_> = ids
        .into_iter()
        .map(|id| file(id, format!("source-{id}.rs")))
        .collect();
    let docs: Vec<_> = sources
        .iter()
        .map(|source| {
            encode_file_doc(
                source,
                &DirectoryIds::default()
                    .resolve(&source.relative_path)
                    .expect("ancestors"),
            )
            .expect("source document")
        })
        .collect();
    write_docs(&collection, &docs, "write source IDs").expect("write source IDs");
    collection.flush().expect("flush IDs");
    for (value, expected_count) in [(true, ids.len()), (false, 0)] {
        let mut query = SearchQuery::scalar(10).expect("constant query");
        query
            .set_filter(&constant_filter(value))
            .expect("constant filter");
        assert_eq!(
            collection.query(&query).expect("constant results").len(),
            expected_count
        );
    }
    for source in &sources {
        for filter in [
            format!("file_id = {}", source.id),
            format!("file_id IN ({})", source.id),
        ] {
            let mut query = SearchQuery::scalar(10).expect("ID query");
            query.set_filter(&filter).expect("ID filter");
            let docs = collection.query(&query).expect("query ID");
            assert_eq!(docs.len(), 1, "{filter}");
            assert_eq!(decode_file_doc(&docs[0]).expect("decode source"), *source);
        }
    }
    collection
        .delete_by_filter(&format!("file_id = {}", u32::MAX))
        .expect("delete maximum ID");
    let query = SearchQuery::scalar(10).expect("remaining sources");
    let mut remaining: Vec<_> = collection
        .query(&query)
        .expect("query after deletion")
        .iter()
        .map(|doc| {
            decode_file_doc(doc)
                .expect("decode remaining source")
                .id
                .get()
        })
        .collect();
    remaining.sort_unstable();
    assert_eq!(remaining, ids[..ids.len() - 1]);
}

fn metadata_store(path: &Path) -> NativeStore {
    super::super::backend::initialize().expect("initialize zvec");
    NativeStore::open(
        path,
        &[EmbeddingModelInfo {
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
        }],
        false,
    )
    .expect("metadata storage")
}

fn metadata_fragments(entity_id: &str, window_id: &str) -> (FileRecord, Vec<IndexedFragment>) {
    let mut source = file(0, "harvest.rs");
    source.snapshot.size_bytes = 100;
    source.snapshot.content_hash = Some("fixture-hash".into());
    source.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    let owner = Entity {
        id: EntityId::new(entity_id).expect("owner ID"),
        file_id: source.id,
        range: SourceRange::Text(
            TextRange::from_coordinates(0, 100, 1, 10, 0, 0).expect("owner range"),
        ),
        content: EntityContent::Outline("Harvest outline".into()),
        metadata: Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("harvest 春'\\crop".into()),
            scope: Some("Garden".into()),
            signature: Some("pub async fn harvest() -> Crop".into()),
            documentation: Some("Produces the seasonal crop.".into()),
        })),
    };
    let window = WindowFragment {
        id: FragmentId::new(window_id).expect("window ID"),
        entity_id: owner.id.clone(),
        file_id: source.id,
        range: SourceRange::Text(
            TextRange::from_coordinates(20, 40, 3, 4, 0, 5).expect("window range"),
        ),
        content: Content::Text("orchard fruit".into()),
    };
    (
        source,
        vec![
            IndexedFragment {
                model: "fixture/fixture".into(),
                fragment: EntityFragment::Representative(owner),
                vector: vec![0.0, 1.0, 0.0],
            },
            IndexedFragment {
                model: "fixture/fixture".into(),
                fragment: EntityFragment::Window(window),
                vector: vec![1.0, 0.0, 0.0],
            },
        ],
    )
}

fn fragment_document(collection: &Collection, id: &str) -> Doc {
    let key = primary_key("fragment", id);
    fetch_map(collection, std::slice::from_ref(&key))
        .expect("stored documents")
        .remove(&key)
        .expect("stored fragment")
}

#[test]
fn metadata_projection_skips_missing_and_null_fields_but_keeps_empty_strings() {
    for json in [
        serde_json::json!({}),
        serde_json::json!({"symbol_name": null, "symbol_type": null}),
    ] {
        assert!(
            encode_metadata_fields(&json)
                .expect("empty projection")
                .is_empty()
        );
    }

    assert_eq!(
        encode_metadata_fields(&serde_json::json!({
            "symbol_name": null,
            "symbol_type": "function",
        }))
        .expect("present field"),
        vec![(CodeMetadata::SYMBOL_TYPE, "function".into())]
    );
    assert_eq!(
        encode_metadata_fields(&serde_json::json!({"symbol_name": ""}))
            .expect("empty string is a value"),
        vec![(CodeMetadata::SYMBOL_NAME, primary_key("symbol", ""))]
    );
}

#[test]
fn metadata_projection_rejects_values_that_do_not_match_the_declared_type() {
    for field in EntityMetadata::index_schema() {
        for value in [
            serde_json::json!(false),
            serde_json::json!(2),
            serde_json::json!(["function"]),
            serde_json::json!({"value": "function"}),
        ] {
            let mut json = serde_json::json!({});
            json[field.name()] = value;
            assert!(
                encode_metadata_fields(&json).is_err(),
                "accepted a non-string value for {}: {json}",
                field.name()
            );
        }
    }
}

#[test]
fn metadata_projection_preserves_full_json_without_indexing_other_fields() {
    let (_, entries) = metadata_fragments("owner", "window");
    let metadata = entries[0]
        .fragment
        .as_entity()
        .expect("owner entity")
        .metadata
        .as_ref()
        .expect("owner metadata");
    let encoded = EncodedMetadata::new(metadata).expect("encode metadata");

    assert_eq!(
        serde_json::from_str::<EntityMetadata>(&encoded.json).expect("full metadata JSON"),
        *metadata
    );
    assert_eq!(
        encoded.fields,
        vec![
            (
                CodeMetadata::SYMBOL_NAME,
                primary_key("symbol", "harvest 春'\\crop"),
            ),
            (CodeMetadata::SYMBOL_TYPE, "function".into()),
        ]
    );
}

#[test]
fn stores_shared_metadata_once_and_filters_windows_by_owner_fields() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entries) = metadata_fragments("owner", "window");
    store.apply_replace(&source, &entries).expect("write file");
    store.flush().expect("flush storage");
    let owner = entries[0].fragment.as_entity().expect("owner entity");
    let entity_doc = fragment_document(&store.entities, owner.id.as_str());
    assert_eq!(
        decode_metadata(&entity_doc).expect("metadata JSON"),
        owner.metadata
    );
    assert_eq!(
        fetch_map(&store.entities, &[primary_key("fragment", "window")])
            .expect("window entity lookup")
            .len(),
        0
    );
    for entry in &entries {
        let id = entry.fragment.document_id();
        let doc = fragment_document(&store.indexes["fixture/fixture"].collection, id);
        assert!(
            !doc.has_field("payload"),
            "search tables contain only projections"
        );
        {
            assert!(!doc.has_field("metadata"));
            assert_eq!(
                string_field(&doc, "symbol_name").expect("indexed owner name"),
                primary_key("symbol", "harvest 春'\\crop")
            );
            assert_eq!(
                string_field(&doc, "symbol_type").expect("indexed owner type"),
                "function"
            );
        }
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
    assert_eq!(hits[0].document_id, "window");
    let vectors = store
        .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter))
        .expect("filtered vector retrieval");
    assert_eq!(vectors.len(), 2);
    assert!(vectors.iter().any(|hit| hit.document_id == "window"));
    let loaded = store
        .load_search_hits(&hits)
        .expect("selected window details");
    assert_eq!(loaded.entities.len(), 1);
    assert_eq!(loaded.entities[&owner.id].entity, *owner);
    assert_eq!(loaded.entities[&owner.id].file, source);
    assert_eq!(loaded.fragments["window"], entries[1].fragment);

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
fn metadata_fields_are_indexed_and_allow_entities_without_filter_values() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    {
        let collection = &store.indexes["fixture/fixture"].collection;
        let schema = collection.schema().expect("retrieval schema");
        for IndexField::String(name) in EntityMetadata::index_schema() {
            assert!(schema.has_field(name), "missing metadata field: {name}");
            assert!(schema.has_index(name), "missing metadata index: {name}");
        }
        for name in ["scope", "signature", "documentation", "heading", "level"] {
            assert!(!schema.has_field(name), "JSON-only metadata field: {name}");
        }
    }

    for metadata in [
        None,
        Some(EntityMetadata::Markdown(MarkdownMetadata {
            heading: Some("Seasonal harvest".into()),
            level: Some(2),
            scope: Some("Garden".into()),
        })),
    ] {
        let (source, mut entries) = metadata_fragments("owner", "window");
        let EntityFragment::Representative(owner) = &mut entries[0].fragment else {
            panic!("representative entity");
        };
        owner.metadata = metadata.clone();
        store
            .apply_replace(&source, &entries)
            .expect("write entities without metadata filter values");
        store.flush().expect("flush storage");
        assert_eq!(
            decode_metadata(&fragment_document(&store.entities, "owner")).expect("metadata JSON"),
            metadata
        );
        {
            let collection = &store.indexes["fixture/fixture"].collection;
            for entry in &entries {
                let doc = fragment_document(collection, entry.fragment.document_id());
                for IndexField::String(name) in EntityMetadata::index_schema() {
                    assert!(
                        doc.is_field_null(name),
                        "unexpected metadata filter value: {name}"
                    );
                }
            }
        }
    }
}

#[test]
fn symbol_filters_distinguish_classes_enums_and_unclassified_entities() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (mut source, mut entries) = metadata_fragments("class", "class-window");
    entries.extend(metadata_fragments("enum", "enum-window").1);
    entries.extend(metadata_fragments("unclassified", "unclassified-window").1);
    for (pair, symbol_type) in entries.as_chunks_mut::<2>().0.iter_mut().zip([
        Some(SymbolType::Class),
        Some(SymbolType::Enum),
        None,
    ]) {
        let EntityFragment::Representative(owner) = &mut pair[0].fragment else {
            panic!("representative entity");
        };
        let Some(EntityMetadata::Code(metadata)) = &mut owner.metadata else {
            panic!("code metadata");
        };
        metadata.symbol_type = symbol_type;
    }
    source.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 3,
    };
    store
        .apply_replace(&source, &entries)
        .expect("write classified entities");
    store.flush().expect("flush storage");

    for symbol_type in [SymbolType::Class, SymbolType::Enum] {
        let filter = StorageSearchFilter {
            symbol_types: Some(vec![symbol_type]),
            ..StorageSearchFilter::default()
        };
        for (hits, count) in [
            (store.search_fts("orchard", 10, Some(&filter)), 1),
            (
                store.search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter)),
                2,
            ),
        ] {
            let hits = hits.expect("filtered results");
            assert_eq!(hits.len(), count);
            assert!(
                hits.iter()
                    .all(|hit| hit.entity_id.as_str() == symbol_type.as_str())
            );
        }
    }

    let classified = StorageSearchFilter {
        symbol_types: Some(vec![
            SymbolType::Alias,
            SymbolType::Class,
            SymbolType::Enum,
            SymbolType::Function,
            SymbolType::Interface,
            SymbolType::Module,
            SymbolType::Value,
        ]),
        ..StorageSearchFilter::default()
    };
    for (all, filtered) in [
        (
            store.search_fts("orchard", 10, None),
            store.search_fts("orchard", 10, Some(&classified)),
        ),
        (
            store.search_vector("fixture/fixture", &entries[1].vector, 10, None),
            store.search_vector("fixture/fixture", &entries[1].vector, 10, Some(&classified)),
        ),
    ] {
        let all = all.expect("unfiltered results");
        let unclassified = all
            .iter()
            .find(|hit| hit.entity_id.as_str() == "unclassified")
            .expect("unclassified entity is searchable");
        let loaded = store
            .load_search_hits(std::slice::from_ref(unclassified))
            .expect("load unclassified entity");
        assert_eq!(
            loaded.entities[&unclassified.entity_id].entity,
            *entries[4].fragment.as_entity().expect("original entity")
        );
        assert!(
            filtered
                .expect("classified results")
                .iter()
                .all(|hit| { hit.entity_id.as_str() != "unclassified" })
        );
    }
}

#[test]
fn retrieval_defers_corrupt_metadata_until_selected_details_are_loaded() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entries) = metadata_fragments("owner", "window");
    store.apply_replace(&source, &entries).expect("write file");
    let mut doc = fragment_document(&store.entities, "owner");
    doc.add_string("metadata", "invalid metadata JSON")
        .expect("corrupt metadata");
    write_docs(&store.entities, &[doc], "replace metadata").expect("write corrupt metadata");
    store.flush().expect("flush storage");

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
            .find(|hit| hit.document_id == "window")
            .expect("window recalled without decoding metadata");
        assert_eq!(window.entity_id.as_str(), "owner");
        let error = store
            .load_search_hits(std::slice::from_ref(window))
            .err()
            .expect("selected corrupt details must fail");
        assert!(error.message().contains("invalid entity metadata"));
    }
}

#[test]
fn native_search_and_loading_restore_arbitrary_domain_ids() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let entity_id = "实体'\\\0owner";
    let window_id = "窗口'\\\0window";
    let (source, entries) = metadata_fragments(entity_id, window_id);
    store
        .apply_replace(&source, &entries)
        .expect("write arbitrary IDs");
    store.flush().expect("flush storage");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![EntityId::new(entity_id).expect("entity ID")]),
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
        assert!(hits.iter().all(|hit| hit.entity_id.as_str() == entity_id));
        let window = hits
            .iter()
            .find(|hit| hit.document_id == window_id)
            .expect("full window ID restored");
        let loaded = store
            .load_search_hits(std::slice::from_ref(window))
            .expect("load arbitrary IDs");
        let owner = entries[0].fragment.as_entity().expect("owner entity");
        assert_eq!(loaded.entities[&owner.id].entity, *owner);
        assert_eq!(loaded.fragments[window_id], entries[1].fragment);
    }
    {
        let collection = &store.indexes["fixture/fixture"].collection;
        let doc = fragment_document(collection, window_id);
        assert_eq!(
            string_field(&doc, "entity_id").expect("encoded owner ID"),
            hex::encode(entity_id)
        );
        assert_eq!(
            string_field(&doc, "document_id").expect("encoded window ID"),
            hex::encode(window_id)
        );
    }
}

#[test]
fn maximum_u32_directory_id_survives_reopen_and_filters_both_retrieval_collections() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    let (mut source, entries) = metadata_fragments("owner", "window");
    source.relative_path = SourcePath::new("edge/harvest.rs").expect("path");
    let mut directories = DirectoryIds::default();
    directories
        .add_source(&source.relative_path, &[u32::MAX])
        .expect("maximum directory ID");
    write_docs(
        &store.directories,
        &[encode_directory_doc(
            &SourcePath::new("edge").expect("directory path"),
            &directories,
        )
        .expect("directory record")],
        "seed maximum directory ID",
    )
    .expect("stored directory");
    *store.directory_ids.lock().expect("directory cache") = Some(directories);
    store
        .apply_replace(&source, &entries)
        .expect("write maximum membership");
    store.flush().expect("checkpoint");
    let schema = EmbeddingModelInfo {
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
    drop(store);
    let reader = NativeStore::open(temporary.path(), &[schema], true).expect("reopen");
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
        .files
        .query(&SearchQuery::scalar(1).expect("query"))
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

#[test]
fn filename_negation_emits_native_sql_with_literal_escaping() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    for (filter, expected) in [
        (
            StoragePathFilter::FileNamePrefix("main".into()),
            "file_name NOT LIKE 'main%'",
        ),
        (
            StoragePathFilter::FileNameSuffix(".rs".into()),
            "file_name NOT LIKE '%%.rs'",
        ),
        (
            StoragePathFilter::FileNamePrefix("under_".into()),
            r"file_name NOT LIKE 'under\_%%'",
        ),
        (
            StoragePathFilter::FileNameSuffix("%.rs".into()),
            r"file_name NOT LIKE '%%\%.rs'",
        ),
        (
            StoragePathFilter::FileNamePrefix(r"back\".into()),
            r"file_name NOT LIKE 'back\\%%'",
        ),
        (
            StoragePathFilter::FileNamePrefix("quote'".into()),
            r"file_name NOT LIKE 'quote\'%'",
        ),
        (
            StoragePathFilter::FileNameExact("main.rs".into()),
            "file_name != 'main.rs'",
        ),
    ] {
        assert_eq!(
            path_filter(&store, &StoragePathFilter::Not(Box::new(filter)), false).expect("SQL"),
            expected
        );
    }
    let filter = StoragePathFilter::Not(Box::new(StoragePathFilter::Or(vec![
        StoragePathFilter::FileNamePrefix("main".into()),
        StoragePathFilter::Not(Box::new(StoragePathFilter::FileNameSuffix(".rs".into()))),
    ])));
    assert_eq!(
        path_filter(&store, &filter, false).expect("boolean SQL"),
        "(file_name NOT LIKE 'main%' AND file_name LIKE '%%.rs')"
    );
}

#[test]
fn filename_negation_preserves_native_results_or_errors_for_both_retrieval_routes() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    let (source, entries) = metadata_fragments("owner", "window");
    store
        .apply_replace(&source, &entries)
        .expect("indexed source");
    let mut fts = Fts::new().expect("FTS request");
    fts.set_match_string("orchard").expect("query text");
    for (path, sql) in [
        (
            StoragePathFilter::FileNamePrefix("absent".into()),
            "file_name NOT LIKE 'absent%'",
        ),
        (
            StoragePathFilter::FileNameSuffix(".rs".into()),
            "file_name NOT LIKE '%%.rs'",
        ),
    ] {
        let filter = StorageSearchFilter {
            path: Some(StoragePathFilter::Not(Box::new(path))),
            ..StorageSearchFilter::default()
        };
        let queries = [
            (
                SearchQuery::fts("text", &fts, 10).expect("native FTS query"),
                &store.indexes["fixture/fixture"].collection,
                StorageSearchPath::Fts,
                "search full-text index",
            ),
            (
                SearchQuery::new("embedding", &[1.0, 0.0, 0.0], 10).expect("native vector query"),
                &store.indexes["fixture/fixture"].collection,
                StorageSearchPath::Vector,
                "search vector index",
            ),
        ];
        for (mut query, collection, route, operation) in queries {
            query.set_filter(sql).expect("native SQL");
            query
                .set_output_fields(&["document_id", "entity_id", "file_id"])
                .expect("projection");
            query.set_include_vector(false).expect("omit vectors");
            let expected = collection.query(&query);
            let actual = match route {
                StorageSearchPath::Fts => store.search_fts("orchard", 10, Some(&filter)),
                StorageSearchPath::Vector => {
                    store.search_vector("fixture/fixture", &[1.0, 0.0, 0.0], 10, Some(&filter))
                }
            };
            match expected {
                Ok(docs) => {
                    let expected = docs
                        .iter()
                        .map(|doc| decode_search_hit(doc, route).expect("native hit"))
                        .collect::<Vec<_>>();
                    assert_eq!(actual.expect("native query succeeded"), expected);
                }
                Err(error) => {
                    let actual = actual.expect_err("native query failed");
                    assert_eq!(actual.code(), EngineError::STORAGE_FAILURE);
                    assert_eq!(actual.message(), format!("zvec {operation}: {error}"));
                }
            }
        }
    }
}

#[test]
fn entity_bundle_is_authoritative_and_rejects_cross_model_fragment_ownership() {
    let home = tempfile::tempdir().expect("storage");
    super::super::backend::initialize().expect("native runtime");
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
    let store = NativeStore::open(home.path(), &[text, other], false).expect("two models");
    let (source, mut entries) = metadata_fragments("owner", "window");
    entries[1].model = "fixture/other".into();
    assert!(
        store.apply_replace(&source, &entries).is_err(),
        "one entity cannot span model tables"
    );
    assert!(store.list_files().expect("unmodified").is_empty());
    entries[1].model = "fixture/fixture".into();
    store
        .apply_replace(&source, &entries)
        .expect("one model owns entire entity");
    let hits = store.search_fts("orchard", 10, None).expect("window match");
    assert_eq!(hits.len(), 1);
    let loaded = store.load_search_hits(&hits).expect("canonical bundle");
    assert_eq!(
        loaded.fragments.len(),
        2,
        "one owner read loads all its fragments"
    );
    assert_eq!(loaded.fragments["owner"], entries[0].fragment);
    assert_eq!(loaded.fragments["window"], entries[1].fragment);
    for index in store.indexes.values() {
        let schema = index.collection.schema().expect("schema");
        assert!(schema.has_index("text"));
        assert!(schema.has_index("embedding"));
        assert!(!schema.has_field("payload"));
    }
    // Removing a derived projection does not remove or redefine the canonical fragment.
    native(
        store.indexes["fixture/fixture"]
            .collection
            .delete_by_filter("file_id = 0"),
        "remove derived rows",
    )
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
    super::super::backend::initialize().expect("native runtime");
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
    let store = NativeStore::open(home.path(), &[first, second], false).expect("two models");
    let (source, entries) = metadata_fragments("owner", "window");
    store
        .apply_replace(&source, &entries)
        .expect("original owner");
    let mut foreign_source = source.clone();
    foreign_source.id = FileId::new(1);
    foreign_source.relative_path = SourcePath::new("other.rs").expect("path");
    let mut foreign = entries.clone();
    for entry in &mut foreign {
        entry.model = "fixture/other".into();
        match &mut entry.fragment {
            EntityFragment::Representative(entity) => entity.file_id = foreign_source.id,
            EntityFragment::Window(window) => window.file_id = foreign_source.id,
            EntityFragment::Standalone(_) => unreachable!(),
        }
    }
    assert!(store.apply_replace(&foreign_source, &foreign).is_err());
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
