use super::*;
use crate::domain::{
    ByteRange, Content, FileIndexStatus, FileSnapshot, MarkdownMetadata, Range, SymbolType,
    TextRange,
};
use std::path::PathBuf;

fn file(id: u32, path: impl Into<PathBuf>) -> FileRecord {
    FileRecord::new(
        FileId::new(id),
        crate::domain::SourcePath::new(path).expect("source path"),
        FileSnapshot {
            size_bytes: 100,
            modified_epoch_ms: None,
            content_hash: Some("fixture-hash".into()),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        },
    )
    .expect("file record")
}

fn model() -> EmbeddingModelInfo {
    EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
                crate::domain::ContentKind::Image,
            ],
            32,
        )
        .expect("valid model metadata"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata")
}

fn binary_sign_model() -> EmbeddingModelInfo {
    EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
            ],
            32,
        )
        .expect("valid model metadata"),
        64,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata")
    .with_vector_index_quantization(VectorIndexQuantization::BinarySign)
}

fn metadata_store(path: &Path) -> Fragments {
    super::super::zvec::initialize().expect("initialize zvec");
    Fragments::open(path, &[model()], false).expect("fragment storage")
}

const DIRECTORY_LOOKUP: fn(&SourcePath) -> EngineResult<Option<DirectoryId>> =
    |path| Ok((path.as_path() == Path::new("src")).then(|| DirectoryId::new(u32::MAX)));

fn write_fixture(
    store: &Fragments,
    file: &FileRecord,
    entities: &[Entity],
    entries: &[IndexedFragment],
) {
    let directories = if file.relative_path().as_path().starts_with("src") {
        vec![DirectoryId::new(u32::MAX)]
    } else {
        Vec::new()
    };
    let docs = Fragments::prepare(
        file,
        entities,
        entries,
        &directories,
        store.embedding_models(),
    )
    .expect("prepare fragments");
    store.write(&docs).expect("write fragments");
}

fn metadata_fragments(
    file_id: u32,
    label: &str,
) -> (FileRecord, Vec<Entity>, Vec<IndexedFragment>) {
    let source = file(file_id, "harvest.rs");
    let mut text = "Harvest outline".to_owned();
    text.push_str(&" ".repeat(20 - text.len()));
    text.push_str("orchard fruit");
    text.push_str(&" ".repeat(40 - text.len()));
    text.push_str(label);
    text.push_str(&" ".repeat(100 - text.len()));
    let content = Content::Text(text);
    let source_range =
        Range::Text(TextRange::from_coordinates(0, 100, 1, 1, 0, 100).expect("owner range"));
    let id = EntityId::new(source.id(), &content, source_range).expect("entity id");
    let owner = Entity::new(
        id.clone(),
        source.id(),
        source_range,
        content,
        Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("harvest 春'\\crop".into()),
            scope: Some("Garden".into()),
            signature: Some("pub async fn harvest() -> Crop".into()),
            visibility: None,
            language: None,
            documentation: Some("Produces the seasonal crop.".into()),
            arity: None,
            is_exported: false,
            is_declaration: false,
            package: None,
        })),
        vec![
            EntityFragment {
                id: FragmentId::new(&id, 0),
                range: Range::Byte(ByteRange::new(0, 15).expect("ordered byte offsets")),
            },
            EntityFragment {
                id: FragmentId::new(&id, 1),
                range: Range::Byte(ByteRange::new(20, 40).expect("ordered byte offsets")),
            },
        ],
    )
    .expect("valid entity");
    let entries = owner
        .fragments()
        .iter()
        .zip([
            (vec![0.0, 1.0, 0.0], "Harvest outline\n"),
            (vec![1.0, 0.0, 0.0], "orchard fruit        \n"),
        ])
        .map(|(fragment, (vector, text))| IndexedFragment {
            fts_text: format!("harvest 春'\\crop\nGarden\npub async fn harvest() -> Crop\nProduces the seasonal crop.\n{text}"),
            model: "fixture/fixture".into(),
            entity_id: owner.id().clone(),
            fragment_id: fragment.id.clone(),
            vector,
        })
        .collect();
    (source, vec![owner], entries)
}

fn fragment_document(collection: &Collection, id: &str) -> Doc {
    let key = id.to_owned();
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
fn metadata_fields_are_indexed_and_allow_entities_without_filter_values() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    {
        let collection = &store.indexes["fixture/fixture"];
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
        let (source, mut entities, entries) = metadata_fragments(0, "owner");
        entities[0] = Entity::new(
            entities[0].id().clone(),
            entities[0].file_id(),
            entities[0].source_range(),
            entities[0].content().clone(),
            metadata,
            entities[0].fragments().to_vec(),
        )
        .expect("valid entity with replacement metadata");
        store
            .delete_file(source.id())
            .expect("remove prior projection");
        write_fixture(&store, &source, &entities, &entries);
        store.flush().expect("flush fragments");
        {
            let collection = &store.indexes["fixture/fixture"];
            for entry in &entries {
                let doc = fragment_document(collection, entry.fragment_id.as_str());
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
    let (mut source, mut entities, mut entries) = metadata_fragments(0, "class");
    for id in ["enum", "unclassified"] {
        let (_, added_entities, added_entries) = metadata_fragments(0, id);
        entities.extend(added_entities);
        entries.extend(added_entries);
    }
    for (owner, symbol_type) in
        entities
            .iter_mut()
            .zip([Some(SymbolType::Class), Some(SymbolType::Enum), None])
    {
        let Some(EntityMetadata::Code(mut metadata)) = owner.metadata().cloned() else {
            panic!("code metadata");
        };
        metadata.symbol_type = symbol_type;
        *owner = Entity::new(
            owner.id().clone(),
            owner.file_id(),
            owner.source_range(),
            owner.content().clone(),
            Some(EntityMetadata::Code(metadata)),
            owner.fragments().to_vec(),
        )
        .expect("valid entity with replacement symbol type");
    }
    source
        .set_index_status(FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 3,
        })
        .expect("file status");
    write_fixture(&store, &source, &entities, &entries);
    store.flush().expect("flush storage");

    for (index, symbol_type) in [SymbolType::Class, SymbolType::Enum]
        .into_iter()
        .enumerate()
    {
        let filter = StorageSearchFilter {
            symbol_types: Some(vec![symbol_type]),
            ..StorageSearchFilter::default()
        };
        let filter = build_filter(Some(&filter), &DIRECTORY_LOOKUP).expect("symbol filter");
        for (hits, count) in [
            (store.search_fts("orchard", 10, filter.as_deref()), 1),
            (
                store.search_vector("fixture/fixture", &entries[1].vector, 10, filter.as_deref()),
                2,
            ),
        ] {
            let hits = hits.expect("filtered results");
            assert_eq!(hits.len(), count);
            assert!(
                hits.iter()
                    .all(|hit| &hit.entity_id == entities[index].id())
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
    let classified = build_filter(Some(&classified), &DIRECTORY_LOOKUP).expect("classified filter");
    for (all, filtered) in [
        (
            store.search_fts("orchard", 10, None),
            store.search_fts("orchard", 10, classified.as_deref()),
        ),
        (
            store.search_vector("fixture/fixture", &entries[1].vector, 10, None),
            store.search_vector(
                "fixture/fixture",
                &entries[1].vector,
                10,
                classified.as_deref(),
            ),
        ),
    ] {
        let all = all.expect("unfiltered results");
        assert!(
            all.iter().any(|hit| hit.entity_id == *entities[2].id()),
            "unclassified entity remains searchable"
        );
        assert!(
            filtered
                .expect("classified results")
                .iter()
                .all(|hit| { hit.entity_id != *entities[2].id() })
        );
    }
}

#[test]
fn filename_like_uses_single_wildcards_and_literal_escaping() {
    for (filter, expected) in [
        (
            StoragePathFilter::FileNamePrefix("main".into()),
            "file_name LIKE 'main%'",
        ),
        (
            StoragePathFilter::FileNameSuffix(".rs".into()),
            "file_name LIKE '%.rs'",
        ),
        (
            StoragePathFilter::FileNamePrefix("under_".into()),
            r"file_name LIKE 'under\_%'",
        ),
        (
            StoragePathFilter::FileNameSuffix("%.rs".into()),
            r"file_name LIKE '%\%.rs'",
        ),
        (
            StoragePathFilter::FileNamePrefix(r"back\".into()),
            r"file_name LIKE 'back\\%'",
        ),
        (
            StoragePathFilter::FileNamePrefix("quote'".into()),
            r"file_name LIKE 'quote\'%'",
        ),
        (
            StoragePathFilter::FileNamePrefix("果园".into()),
            "file_name LIKE '果园%'",
        ),
    ] {
        assert_eq!(
            path_filter(&DIRECTORY_LOOKUP, &filter, false).expect("SQL"),
            expected
        );
        let negative = StoragePathFilter::Not(Box::new(filter));
        assert_eq!(
            path_filter(&DIRECTORY_LOOKUP, &negative, false).expect("negative SQL"),
            expected.replace(" LIKE ", " NOT LIKE ")
        );
        assert_eq!(
            path_filter(
                &DIRECTORY_LOOKUP,
                &StoragePathFilter::Not(Box::new(negative)),
                false
            )
            .expect("double negation"),
            expected
        );
    }
}

#[test]
fn filename_like_negation_propagates_native_query_errors() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    write_fixture(&store, &source, &entities, &entries);
    for (positive, sql) in [
        (
            StoragePathFilter::FileNamePrefix("main".into()),
            "file_name NOT LIKE 'main%'",
        ),
        (
            StoragePathFilter::FileNameSuffix(".rs".into()),
            "file_name NOT LIKE '%.rs'",
        ),
    ] {
        let mut query = SearchQuery::scalar(10).expect("native query");
        query.set_filter(sql).expect("native filter");
        let native_error = store.indexes["fixture/fixture"]
            .query(&query)
            .err()
            .expect("the pinned zvec release does not parse NOT LIKE");
        let filter = StorageSearchFilter {
            path: Some(StoragePathFilter::Not(Box::new(positive))),
            ..StorageSearchFilter::default()
        };
        let filter = build_filter(Some(&filter), &DIRECTORY_LOOKUP).expect("negated filter");
        for (operation, result) in [
            (
                "search full-text index",
                store.search_fts("orchard", 10, filter.as_deref()),
            ),
            (
                "search vector index",
                store.search_vector("fixture/fixture", &[1.0, 0.0, 0.0], 10, filter.as_deref()),
            ),
        ] {
            let error = result.expect_err("native rejection reaches the caller");
            assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
            assert_eq!(error.message(), format!("zvec {operation}: {native_error}"));
        }
    }
    let nested = StoragePathFilter::Not(Box::new(StoragePathFilter::Or(vec![
        StoragePathFilter::FileNamePrefix("main".into()),
        StoragePathFilter::Not(Box::new(StoragePathFilter::FileNameSuffix(".rs".into()))),
    ])));
    assert_eq!(
        path_filter(&DIRECTORY_LOOKUP, &nested, false).expect("nested negation"),
        "(file_name NOT LIKE 'main%' AND file_name LIKE '%.rs')"
    );
}

fn index_named_filter_sources(store: &Fragments, paths: &[&str]) {
    for (index, path) in paths.iter().enumerate() {
        let id = u32::try_from(index + 1).expect("file ID");
        let (source, entities, entries) = metadata_fragments(id, &format!("owner-{id}"));
        let source = FileRecord::new(
            source.id(),
            SourcePath::new(*path).expect("source path"),
            source.snapshot().clone(),
            source.index_status().clone(),
        )
        .expect("file record");
        write_fixture(store, &source, &entities, &entries);
    }
}

fn assert_filter_file_ids(store: &Fragments, path: &StoragePathFilter, expected: &[u32]) {
    let filter = StorageSearchFilter {
        path: Some(path.clone()),
        ..StorageSearchFilter::default()
    };
    let filter = build_filter(Some(&filter), &DIRECTORY_LOOKUP).expect("path filter");
    for (label, result, fragments_per_file) in [
        (
            "FTS",
            store.search_fts("orchard", 100, filter.as_deref()),
            1,
        ),
        (
            "vector",
            store.search_vector("fixture/fixture", &[1.0, 0.0, 0.0], 100, filter.as_deref()),
            2,
        ),
    ] {
        let mut actual = result
            .expect("native filtered search")
            .into_iter()
            .map(|hit| hit.file_id.get())
            .collect::<Vec<_>>();
        actual.sort_unstable();
        let mut expected = expected
            .iter()
            .flat_map(|id| std::iter::repeat_n(*id, fragments_per_file))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        assert_eq!(actual, expected, "{label}: {path:?}");
    }
}

fn reopen_filter_store(path: &Path) -> Fragments {
    Fragments::open(path, &[model()], true).expect("reopen fragments")
}

#[test]
fn filename_like_matches_literals_and_case_before_and_after_reopen() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    let mut names = vec![
        "main.rs",
        "Main.rs",
        "main.RS",
        "under_.rs",
        "underX.rs",
        "literal%.rs",
        "literalX.rs",
        "quote'.rs",
        "quoteX.rs",
        "果园.rs",
        "果园.txt",
        "unrelated.txt",
    ];
    let mut suffix_matches = vec![1, 2, 4, 5, 6, 7, 8, 9, 10];
    let mut cases = vec![
        (StoragePathFilter::FileNamePrefix("main".into()), vec![1, 3]),
        (StoragePathFilter::FileNamePrefix("Main".into()), vec![2]),
        (StoragePathFilter::FileNameSuffix(".RS".into()), vec![3]),
        (StoragePathFilter::FileNamePrefix("under_".into()), vec![4]),
        (StoragePathFilter::FileNameSuffix("_.rs".into()), vec![4]),
        (
            StoragePathFilter::FileNamePrefix("literal%".into()),
            vec![6],
        ),
        (StoragePathFilter::FileNameSuffix("%.rs".into()), vec![6]),
        (StoragePathFilter::FileNamePrefix("quote'".into()), vec![8]),
        (StoragePathFilter::FileNameSuffix("'.rs".into()), vec![8]),
        (
            StoragePathFilter::FileNamePrefix("果园".into()),
            vec![10, 11],
        ),
        (StoragePathFilter::FileNameSuffix("园.rs".into()), vec![10]),
        (StoragePathFilter::FileNameSuffix(".missing".into()), vec![]),
    ];
    if cfg!(unix) {
        names.extend([r"back\.rs", "backX.rs"]);
        suffix_matches.extend([13, 14]);
        cases.extend([
            (StoragePathFilter::FileNamePrefix(r"back\".into()), vec![13]),
            (StoragePathFilter::FileNameSuffix(r"\.rs".into()), vec![13]),
        ]);
    }
    cases.push((
        StoragePathFilter::FileNameSuffix(".rs".into()),
        suffix_matches,
    ));
    index_named_filter_sources(&store, &names);
    let verify = |store: &Fragments| {
        for (path, expected) in &cases {
            assert_filter_file_ids(store, path, expected);
        }
    };
    verify(&store);
    store.flush().expect("flush filter storage");
    verify(&store);
    drop(store);
    verify(&reopen_filter_store(temporary.path()));
}

#[test]
fn negated_directory_contains_empty_root_arrays_before_and_after_reopen() {
    let temporary = tempfile::tempdir().expect("storage");
    let store = metadata_store(temporary.path());
    index_named_filter_sources(
        &store,
        &[
            "root.rs",
            "src/main.rs",
            "src/nested/deep.rs",
            "other/main.rs",
        ],
    );
    let directory = StoragePathFilter::Directory(SourcePath::new("src").expect("directory"));
    let complement = StoragePathFilter::Not(Box::new(directory.clone()));
    let id = DirectoryId::new(u32::MAX);
    assert_eq!(
        path_filter(&DIRECTORY_LOOKUP, &complement, false).expect("native complement"),
        format!("ancestor_directory_ids NOT CONTAIN_ANY ({id})")
    );
    let cases = [
        (directory, vec![2, 3]),
        (complement, vec![1, 4]),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::Directory(
                SourcePath::new("absent").expect("directory"),
            ))),
            vec![1, 2, 3, 4],
        ),
        (
            StoragePathFilter::Not(Box::new(StoragePathFilter::Or(vec![
                StoragePathFilter::Directory(SourcePath::new("src").expect("directory")),
                StoragePathFilter::FileNameExact("main.rs".into()),
            ]))),
            vec![1],
        ),
    ];
    let verify = |store: &Fragments| {
        for (path, expected) in &cases {
            assert_filter_file_ids(store, path, expected);
        }
    };
    verify(&store);
    store.flush().expect("flush empty-array storage");
    verify(&store);
    drop(store);
    verify(&reopen_filter_store(temporary.path()));
}

#[test]
fn projection_validation_keeps_one_model_and_one_row_per_fragment() {
    let (_, entities, entries) = metadata_fragments(0, "owner");
    validate_projections(&entities, &entries).expect("complete projections");
    assert!(validate_projections(&entities, &entries[..1]).is_err());
    let mut duplicate = entries.clone();
    duplicate.push(entries[0].clone());
    assert!(validate_projections(&entities, &duplicate).is_err());
    let mut mixed = entries;
    mixed[1].model = "fixture/other".into();
    assert!(validate_projections(&entities, &mixed).is_err());
}

#[test]
fn fragment_ownership_is_checked_across_model_collections() {
    super::super::zvec::initialize().expect("initialize zvec");
    let home = tempfile::tempdir().expect("storage");
    let mut other = model();
    {
        let mut metadata = serde_json::to_value(&other).expect("model metadata");
        metadata["model"]["name"] = serde_json::json!("other");
        other = serde_json::from_value(metadata).expect("valid model metadata");
    }
    let store = Fragments::open(home.path(), &[model(), other], false).expect("two models");
    let (source, entities, entries) = metadata_fragments(u32::MAX, "owner");
    write_fixture(&store, &source, &entities, &entries);
    store
        .validate_ownership(&entries, source.id())
        .expect("same file");
    let mut foreign = entries.clone();
    for entry in &mut foreign {
        entry.model = "fixture/other".into();
    }
    assert!(store.validate_ownership(&foreign, FileId::new(0)).is_err());
    let hits = store.search_fts("orchard", 10, None).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, source.id());
    assert_eq!(hits[0].document_id, entries[1].fragment_id.as_str());
    assert_eq!(hits[0].entity_id, *entities[0].id());
    store.delete_file(source.id()).expect("delete projections");
    store.delete_file(source.id()).expect("idempotent delete");
    assert!(
        store
            .search_fts("orchard", 10, None)
            .expect("search deleted")
            .is_empty()
    );
}

#[test]
fn embeddings_round_trip_as_fp16_and_remain_searchable() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, entities, entries) = metadata_fragments(1, "fp16");
    let mut entries = entries;
    entries[1].vector = vec![0.1, -0.2, 1.25];
    write_fixture(&store, &source, &entities, &entries);
    store.flush().expect("flush storage");

    let collection = store
        .collection("fixture/fixture")
        .expect("model collection");
    let stored = collection
        .fetch(&[entries[1].fragment_id.as_str()])
        .expect("fetch fragment");
    assert_eq!(stored.len(), 1);
    let decoded = zg_zvec_fp16::decode_embedding(&stored[0], "embedding")
        .expect("decode embedding")
        .expect("stored vector");
    assert_eq!(decoded.len(), entries[1].vector.len());
    for (decoded, original) in decoded.iter().zip(&entries[1].vector) {
        assert_eq!(
            decoded.to_bits(),
            half::f16::from_f32(*original).to_f32().to_bits()
        );
    }
    assert_ne!(decoded[0].to_bits(), entries[1].vector[0].to_bits());

    let hits = store
        .search_vector("fixture/fixture", &entries[1].vector, 1, None)
        .expect("vector search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, entries[1].fragment_id.as_str());
    assert_eq!(hits[0].path, StorageSearchPath::Vector);
}

#[test]
fn binary_sign_scores_float_queries_with_each_supported_metric() {
    let document = [2.0_f32, -1.0, 0.0];
    let query = [1.0_f32, 1.0, -1.0];
    let (packed, scale) = encode_sign_vector(&document).expect("encode signs");
    assert_eq!(packed.len(), 1);
    assert_eq!(scale.to_bits(), 1.0_f32.to_bits());

    let cosine =
        binary_sign_score(&query, &packed, scale, EmbeddingMetric::Cosine).expect("cosine score");
    assert!((cosine - (-1.0 / 3.0)).abs() < 1e-12);
    assert_eq!(
        binary_sign_score(&query, &packed, scale, EmbeddingMetric::DotProduct)
            .expect("dot-product score")
            .to_bits(),
        (-1.0_f64).to_bits()
    );
    let euclidean = binary_sign_score(&query, &packed, scale, EmbeddingMetric::Euclidean)
        .expect("Euclidean score");
    assert!((euclidean + 8.0_f64.sqrt()).abs() < 1e-12);
}

#[test]
fn binary_sign_search_rejects_limits_above_the_shared_top_k_bound() {
    let home = tempfile::tempdir().expect("disposable local index");
    let store = Fragments::open(home.path(), &[binary_sign_model()], false)
        .expect("create binary-sign fragment index");
    let error = store
        .search_vector("fixture/fixture", &[1.0; 64], MAX_TOP_K + 1, None)
        .expect_err("binary-sign search must use the shared top-k bound");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn binary_sign_fragments_search_with_float_queries_and_reopen_from_a_local_index() {
    let home = tempfile::tempdir().expect("disposable local index");
    let binary_model = binary_sign_model();
    let store = Fragments::open(home.path(), std::slice::from_ref(&binary_model), false)
        .expect("create binary-sign fragment index");
    let (source, entities, mut entries) = metadata_fragments(1, "binary-sign");

    let mut query = vec![0.01_f32; 64];
    query[0] = 1.0;
    let mut high_weight_match = vec![-0.01_f32; 64];
    high_weight_match[0] = 1.0;
    let mut low_weight_match = vec![0.01_f32; 64];
    low_weight_match[0] = -1.0;
    entries[0].vector = high_weight_match;
    entries[1].vector = low_weight_match;

    write_fixture(&store, &source, &entities, &entries);
    store.flush().expect("flush local index");
    let hits = store
        .search_vector("fixture/fixture", &query, 2, None)
        .expect("float query against packed signs");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].document_id, entries[0].fragment_id.as_str());

    drop(store);
    let reopened =
        Fragments::open(home.path(), &[binary_model], true).expect("reopen disposable local index");
    let hits = reopened
        .search_vector("fixture/fixture", &query, 2, None)
        .expect("query reopened binary-sign index");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].document_id, entries[0].fragment_id.as_str());
}

#[test]
fn empty_selection_lists_compile_to_false_without_native_in_syntax() {
    for filter in [
        StorageSearchFilter {
            file_ids: Some(Vec::new()),
            ..Default::default()
        },
        StorageSearchFilter {
            entity_ids: Some(Vec::new()),
            ..Default::default()
        },
        StorageSearchFilter {
            symbol_names: Some(Vec::new()),
            ..Default::default()
        },
        StorageSearchFilter {
            symbol_types: Some(Vec::new()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            build_filter(Some(&filter), &DIRECTORY_LOOKUP).expect("empty filter"),
            Some("file_id IS NULL".into())
        );
    }
}

#[test]
fn query_limits_and_nul_text_preserve_native_boundaries() {
    assert!(top_k(MAX_TOP_K).is_ok());
    assert!(top_k(MAX_TOP_K + 1).is_err());
    assert_eq!(index_text("before\0after"), "before after");
    assert!(matches!(index_text("plain"), Cow::Borrowed(_)));
}

#[test]
fn exact_symbol_lookup_collects_all_models_and_deduplicates_large_fragment_sets() {
    super::super::zvec::initialize().expect("initialize zvec");
    let root = tempfile::tempdir().expect("storage");
    let first = model();
    let second = EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            first.model().provider(),
            "second",
            first.model().content_kinds().iter().copied(),
            first.model().max_batch_size(),
        )
        .expect("second model metadata"),
        first.dimension(),
        first.metric(),
        first.max_input_tokens(),
        first.max_image_bytes(),
    )
    .expect("second model");
    let store = Fragments::open(root.path(), &[first.clone(), second.clone()], false)
        .expect("model tables");
    let mut entities = Vec::new();
    let mut entries = Vec::new();
    for i in 0..125 {
        let (_, mut owners, mut projections) = metadata_fragments(1, &format!("body-{i}"));
        let mut owner = owners.remove(0);
        if i == 0 {
            // One entity alone fills a query batch; splitting must skip its
            // remaining fragments without hiding other matching entities.
            let fragments = (0..1100)
                .map(|ordinal| EntityFragment {
                    id: FragmentId::new(owner.id(), ordinal),
                    range: Range::Full,
                })
                .collect();
            owner = Entity::new(
                owner.id().clone(),
                owner.file_id(),
                owner.source_range(),
                owner.content().clone(),
                owner.metadata().cloned(),
                fragments,
            )
            .expect("fragmented entity");
            projections = owner
                .fragments()
                .iter()
                .map(|fragment| IndexedFragment {
                    entity_id: owner.id().clone(),
                    fragment_id: fragment.id.clone(),
                    model: first.model().reference(),
                    vector: vec![1.0, 0.0, 0.0],
                    fts_text: "harvest".into(),
                })
                .collect();
        }
        if i == 124 {
            for projection in &mut projections {
                projection.model = second.model().reference();
            }
        }
        entities.push(owner);
        entries.extend(projections);
    }
    write_fixture(&store, &file(1, "harvest.rs"), &entities, &entries);
    let expected: BTreeSet<_> = entities.iter().map(|entity| entity.id().clone()).collect();
    let found = store
        .find_symbol_entity_ids("harvest 春'\\crop", None)
        .expect("all exact definitions");
    assert_eq!(found, expected.into_iter().collect::<Vec<_>>());
    assert!(
        store
            .find_symbol_entity_ids("Harvest 春'\\crop", None)
            .expect("case-sensitive lookup")
            .is_empty()
    );
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[allow(clippy::too_many_lines)] // Keep the full build, verification, filter, and reopen sequence together.
fn binary_sign_metis_shaped_vectors_round_trip_and_rank_exhaustively() {
    const DIM: usize = 1024;
    const SCALE: f32 = 0.03125;
    super::super::zvec::initialize().expect("initialize zvec");
    let model = EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
            ],
            32,
        )
        .expect("model"),
        DIM,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("embedding model")
    .with_vector_index_quantization(VectorIndexQuantization::BinarySign);
    let home = tempfile::tempdir().expect("disposable index");
    let store =
        Fragments::open(home.path(), std::slice::from_ref(&model), false).expect("create index");
    // Unequal float magnitudes make matching query signs insufficient for ranking.
    let query: Vec<f32> = (0..DIM)
        .map(|i| {
            let magnitude = if i < 80 { 0.25 } else { 0.005 };
            if i % 3 == 0 { -magnitude } else { magnitude }
        })
        .collect();
    let mut documents = Vec::new();
    for file_id in 1..=3 {
        let (source, entities, mut entries) = metadata_fragments(file_id, "metis-shaped");
        for (j, entry) in entries.iter_mut().enumerate() {
            let variant = usize::try_from((file_id - 1) * 2).expect("bounded file id") + j;
            entry.vector = (0..DIM)
                .map(|i| {
                    let sign = if (i * 13 + variant * 17) % 97 < (variant + 1) * 11 {
                        -1.0
                    } else {
                        1.0
                    };
                    if query[i] < 0.0 {
                        -SCALE * sign
                    } else {
                        SCALE * sign
                    }
                })
                .collect();
            documents.push((
                source.id(),
                entry.fragment_id.as_str().to_owned(),
                entry.vector.clone(),
            ));
        }
        write_fixture(&store, &source, &entities, &entries);
    }
    store.flush().expect("flush index");
    let verify = |store: &Fragments| {
        let collection = store.collection("fixture/fixture").expect("collection");
        let query_norm = query
            .iter()
            .map(|&x| f64::from(x).powi(2))
            .sum::<f64>()
            .sqrt();
        let mut expected = Vec::new();
        for (file_id, id, vector) in &documents {
            let stored = fragment_document(collection, id);
            let packed = stored
                .get_binary("embedding")
                .expect("packed field")
                .expect("packed vector");
            let scale = stored
                .get_f32("embedding_scale")
                .expect("scale field")
                .expect("scale");
            assert_eq!(packed.len(), DIM / 8);
            assert_eq!(scale.to_bits(), SCALE.to_bits());
            for (i, &original) in vector.iter().enumerate() {
                let decoded = if packed[i / 8] & (1 << (i % 8)) != 0 {
                    scale
                } else {
                    -scale
                };
                assert_eq!(decoded.to_bits(), original.to_bits(), "{id} dimension {i}");
            }
            let dot = query
                .iter()
                .zip(vector)
                .map(|(&q, &d)| f64::from(q) * f64::from(d))
                .sum::<f64>();
            let doc_norm = vector
                .iter()
                .map(|&d| f64::from(d).powi(2))
                .sum::<f64>()
                .sqrt();
            let cosine = dot / (query_norm * doc_norm);
            for (metric, reference) in [
                (EmbeddingMetric::DotProduct, dot),
                (EmbeddingMetric::Cosine, cosine),
            ] {
                let actual = binary_sign_score(&query, &packed, scale, metric).expect("sign score");
                assert!(
                    (actual - reference).abs() < 1e-12,
                    "{id}: {actual} != {reference}"
                );
            }
            expected.push((*file_id, id.clone(), cosine));
        }
        expected.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        for file_ids in [None, Some(vec![FileId::new(2)])] {
            let filter = StorageSearchFilter {
                file_ids: file_ids.clone(),
                ..Default::default()
            };
            let sql = build_filter(Some(&filter), &DIRECTORY_LOOKUP).expect("filter");
            let hits = store
                .search_vector("fixture/fixture", &query, 6, sql.as_deref())
                .expect("search");
            let selected: Vec<_> = expected
                .iter()
                .filter(|(id, _, _)| file_ids.as_ref().is_none_or(|ids| ids.contains(id)))
                .collect();
            assert_eq!(hits.len(), selected.len());
            for (hit, (_, id, score)) in hits.iter().zip(selected) {
                assert_eq!(&hit.document_id, id);
                assert!((hit.score - score).abs() < 1e-12);
            }
            let top = store
                .search_vector("fixture/fixture", &query, 1, sql.as_deref())
                .expect("top one");
            assert_eq!(top[0].document_id, hits[0].document_id);
        }
    };
    verify(&store);
    drop(store);
    let reopened = Fragments::open(home.path(), &[model], true).expect("reopen disposable index");
    verify(&reopened);
}
