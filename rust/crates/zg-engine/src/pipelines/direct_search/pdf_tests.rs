use super::*;
use crate::{
    api::context::{options::RgOptions, result::ContentRange},
    domain::{
        Content, ContentKind, Entity, EntityFragment, EntityId, FTS_CONFIG, FileIndexStatus,
        FileRecord, FileSnapshot, FragmentId, IndexDescriptor, IndexState, PdfBlock, PdfSourceMap,
        PdfSourcePart, Range, ScanRules, SourcePath, Workspace, WorkspaceLayout,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    },
    storage::{
        IndexStore,
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
    workspace::{CURRENT_INDEX_VERSION, manifest::write_workspace_manifest},
};
use std::{collections::BTreeMap, fs, sync::Arc};

#[expect(
    clippy::too_many_lines,
    reason = "One persisted fixture includes canonical entities and intentionally unrelated FTS projections"
)]
fn fixture_with_pages(texts: &[&[(u32, &str)]]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    let root = dir.path();
    let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
    let schema = EmbeddingModelInfo::new(
        ModelInfo::new("fixture", "pdf-rg", [ContentKind::Text], 32).expect("model"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("schema");
    fs::create_dir_all(layout.index_dir()).expect("index directory");
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        graph_enabled: false,
        storage_path: layout.index_dir().into(),
        embeddings: vec![schema.clone()],
    })
    .expect("store");
    fs::write(
        root.join("manual.pdf"),
        "%PDF-1.7\n<xmp>metadata-only</xmp>",
    )
    .expect("raw PDF");
    fs::write(root.join("a.md"), "Recovery wait is 5 minutes.\n").expect("ordinary text");
    let id = store
        .resolve_file_ids(&[PathBuf::from("manual.pdf")])
        .expect("id")[0];
    let file = FileRecord::new(
        id,
        SourcePath::new("manual.pdf").expect("path"),
        FileSnapshot {
            size_bytes: 32,
            modified_epoch_ms: None,
            content_hash: Some("fixture".into()),
        },
        FileIndexStatus::NotIndexed,
    )
    .expect("file");
    let mut start = 0;
    let mut entities = Vec::new();
    let mut fragments = Vec::new();
    for (index, blocks) in texts.iter().enumerate() {
        let mut text = String::new();
        let mut parts = Vec::new();
        for (block_index, (page, block_text)) in blocks.iter().enumerate() {
            let start = text.len();
            text.push_str(block_text);
            parts.push(PdfSourcePart {
                range: start..text.trim_end_matches('\n').len(),
                block: PdfBlock {
                    page: *page,
                    id: u32::try_from(block_index + 1).expect("block ID"),
                    bounds: Some([100_000, 100_000, 900_000, 200_000]),
                },
            });
        }
        let range = Range::Pdf(Arc::new(PdfSourceMap {
            markdown_start: start,
            text_len: text.len(),
            parts,
        }));
        let text_len = text.len();
        let content = Content::Text(text);
        let entity_id = EntityId::new(id, &content, &range).expect("entity id");
        let fragment_id = FragmentId::new(&entity_id, 0);
        entities.push(
            Entity::new(
                entity_id.clone(),
                id,
                range,
                content,
                None,
                vec![EntityFragment {
                    id: fragment_id.clone(),
                    range: Range::Full,
                }],
            )
            .expect("entity"),
        );
        fragments.push(IndexedFragment {
            entity_id,
            fragment_id,
            model: schema.model().reference(),
            vector: vec![1.0, 0.0, 0.0],
            fts_text: format!("unrelated search projection {index}"),
        });
        start += text_len + 2;
    }
    store
        .replace_file(&file, &entities, &fragments)
        .expect("store entities");
    store.close().expect("close");
    let workspace = Workspace::new(
        root,
        layout.clone(),
        ScanRules::default(),
        Some(CURRENT_INDEX_VERSION),
        Some(uuid::Uuid::new_v4().to_string()),
        1,
        1,
        IndexState::Enabled(Box::new(
            IndexDescriptor::new(schema, BTreeMap::new(), BTreeMap::new(), false, FTS_CONFIG)
                .expect("descriptor"),
        )),
    )
    .expect("workspace");
    write_workspace_manifest(layout.manifest_path(), &workspace).expect("publish");
    dir
}

fn fixture() -> tempfile::TempDir {
    fixture_with_pages(&[
        &[
            (2, "# Recovery\n\nRecovery wait is 5 minutes.\n"),
            (3, "中😀 Recovery wait is 10 minutes.\n"),
            (4, "entity tail"),
        ],
        &[(2, "entity head\nRecovery restarts here.\n")],
    ])
}
fn options(root: &Path, query: &str) -> ContextOptions {
    ContextOptions {
        root: Some(root.into()),
        query: Some(query.into()),
        rg: true,
        file_types: vec!["pdf".into()],
        limit: None,
        ..ContextOptions::default()
    }
}

#[tokio::test]
async fn pdf_rg_searches_canonical_entities_and_merges_cross_page_matches() {
    let dir = fixture();
    let service = DirectSearchService::new();
    let mut request = options(dir.path(), "Recovery wait");
    request.rg_options.fixed_strings = true;
    let result = service.context(request).await.expect("PDF query");
    assert_eq!(result.items.len(), 1);
    let item = &result.items[0];
    assert_eq!(item.relative_path, Path::new("manual.pdf"));
    assert_eq!(item.pdf_boxes.len(), 3);
    let wire = serde_json::to_string(item).expect("serialize context");
    let restored: crate::api::context::result::ContextItem =
        serde_json::from_str(&wire).expect("deserialize context");
    assert_eq!(restored.pdf_boxes, item.pdf_boxes);
    assert!(matches!(&item.range, ContentRange::Pdf { pages, .. } if pages == &[2,3]));
    assert_eq!(item.match_ranges.len(), 2);
    assert!(item.content.contains("中😀 Recovery wait is 10 minutes."));
    assert!(!item.content.contains("entity tail"));
    for matched in &item.match_ranges {
        let start = matched.start_byte_offset().expect("offset")
            - item.content_range.start_byte_offset().expect("offset");
        let end = matched.end_byte_offset().expect("offset")
            - item.content_range.start_byte_offset().expect("offset");
        assert_eq!(&item.content[start..end], "Recovery wait");
    }
    let xml = service
        .context(options(dir.path(), "metadata-only"))
        .await
        .expect("XML excluded");
    assert!(xml.items.is_empty());
    let mut boundary = options(dir.path(), "entity tail\\s+entity head");
    boundary.rg_options.multiline = true;
    assert!(
        service
            .context(boundary)
            .await
            .expect("entity boundary")
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn pdf_rg_pages_exclude_context_and_gaps_between_matches() {
    let dir = fixture();
    let service = DirectSearchService::new();
    for (query, pages) in [
        ("5 minutes", vec![2]),
        ("中😀", vec![3]),
        ("entity tail", vec![4]),
        ("Recovery wait|entity tail", vec![2, 3, 4]),
        ("5 minutes|entity tail", vec![2, 4]),
    ] {
        let mut request = options(dir.path(), query);
        request.rg_options.before_context = 10;
        request.rg_options.after_context = 10;
        let result = service.context(request).await.expect("PDF query");
        assert_eq!(result.items.len(), 1, "{query}");
        let item = &result.items[0];
        assert!(
            matches!(&item.range, ContentRange::Pdf { pages: actual, .. } if actual == &pages),
            "{query}: {:?}",
            item.range
        );
        assert!(
            matches!(&item.content_range, ContentRange::Pdf { pages, .. } if pages == &[2, 3, 4])
        );
        assert!(item.content.starts_with("# Recovery"));
        assert!(item.content.ends_with("entity tail"));
    }
}

#[tokio::test]
async fn pdf_rg_maps_matches_inside_a_joined_cross_page_paragraph() {
    let dir = fixture_with_pages(&[&[
        (1, "Highprecision codes preserve recall "),
        (2, "but increase "),
        (3, "memory traffic.\n"),
    ]]);
    let service = DirectSearchService::new();
    for (query, pages) in [
        ("Highprecision codes preserve recall", vec![1]),
        ("memory traffic", vec![3]),
        ("recall but", vec![1, 2]),
        ("Highprecision|traffic", vec![1, 3]),
    ] {
        let result = service
            .context(options(dir.path(), query))
            .await
            .expect("joined paragraph");
        assert_eq!(result.items.len(), 1);
        let item = &result.items[0];
        assert!(
            matches!(&item.range, ContentRange::Pdf { pages: actual, .. } if actual == &pages),
            "{query}: {:?}",
            item.range
        );
        assert!(
            matches!(&item.content_range, ContentRange::Pdf { pages, .. } if pages == &[1, 2, 3])
        );
        assert_eq!(
            item.range.end_byte_offset(),
            item.match_ranges
                .last()
                .expect("exact match")
                .end_byte_offset()
        );
    }
    let selected = service
        .context_selected_files(
            options(dir.path(), "memory traffic"),
            vec![dir.path().join("manual.pdf")],
            1,
        )
        .await
        .expect("path lookup retains entity range");
    assert!(
        matches!(&selected.items[0].range, ContentRange::Pdf { pages, .. } if pages == &[1, 2, 3])
    );
    assert!(
        matches!(&selected.items[0].excerpt_range, Some(ContentRange::Pdf { pages, .. }) if pages == &[3])
    );
}

#[tokio::test]
async fn pdf_rg_maps_zero_width_matches_and_unmapped_separators_locally() {
    let dir = fixture_with_pages(&[&[(2, "first\n\n"), (3, "中😀 last")]]);
    let service = DirectSearchService::new();
    for (query, pages) in [
        (r"\A", vec![2]),
        (r"\z", vec![3]),
        (r"(?m)^", vec![2, 3]),
        (r"\n\n", vec![2]),
    ] {
        let mut request = options(dir.path(), query);
        request.rg_options.multiline = true;
        let result = service.context(request).await.expect("boundary search");
        assert_eq!(result.items.len(), 1, "{query}");
        assert!(
            matches!(&result.items[0].range, ContentRange::Pdf { pages: actual, .. } if actual == &pages),
            "{query}: {:?}",
            result.items[0].range
        );
    }
}

#[tokio::test]
async fn pdf_rg_inverted_search_reports_selected_pages_without_context_pages() {
    let dir = fixture_with_pages(&[&[(1, "needle\n"), (2, "keep this line\n")]]);
    let mut request = options(dir.path(), "needle");
    request.rg_options.invert_match = true;
    request.rg_options.before_context = 1;
    let result = DirectSearchService::new()
        .context(request)
        .await
        .expect("invert");
    assert_eq!(result.items.len(), 1);
    let item = &result.items[0];
    assert!(matches!(&item.range, ContentRange::Pdf { pages, .. } if pages == &[2]));
    assert!(matches!(&item.content_range, ContentRange::Pdf { pages, .. } if pages == &[1, 2]));
    assert!(item.match_ranges.is_empty());
}

#[tokio::test]
async fn pdf_rg_preserves_regex_case_context_limits_and_file_filters() {
    let dir = fixture();
    let service = DirectSearchService::new();
    let mut request = options(dir.path(), "recovery wait is (5|10)");
    assert!(
        service
            .context(request.clone())
            .await
            .expect("case sensitive")
            .items
            .is_empty()
    );
    request.rg_options.ignore_case = true;
    request.rg_options.before_context = 2;
    request.rg_options.after_context = 1;
    let result = service.context(request).await.expect("case and context");
    assert_eq!(result.items.len(), 1);
    assert!(result.items[0].content.starts_with("# Recovery"));
    assert!(result.items[0].content.ends_with("entity tail"));
    let mut request = options(dir.path(), "Recovery");
    request.rg_options.max_count = Some(1);
    let result = service.context(request).await.expect("per file max count");
    assert_eq!(result.items.len(), 1);
    assert!(matches!(&result.items[0].range, ContentRange::Pdf { pages, .. } if pages == &[2]));
    assert_eq!(result.coverage, ContextCoverage::RgTruncated);
    let mut request = options(dir.path(), "Recovery");
    request.file_types.clear();
    request.limit = Some(1);
    let result = service.context(request).await.expect("mixed files limit");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path, Path::new("a.md"));
    assert_eq!(result.coverage, ContextCoverage::RgTruncated);
    let mut request = options(dir.path(), "Recovery");
    request.globs = vec!["!*.pdf".into()];
    assert!(
        service
            .context(request)
            .await
            .expect("exclude pdf")
            .items
            .is_empty()
    );
    let mut request = options(dir.path(), "Recovery");
    request.file_types.clear();
    request.rg_paths = vec!["manual.pdf".into()];
    assert_eq!(
        service
            .context(request)
            .await
            .expect("explicit PDF")
            .items
            .len(),
        2
    );
}

#[tokio::test]
async fn pdf_rg_requires_extraction_index_without_raw_fallback() {
    let dir = tempfile::tempdir().expect("workspace");
    fs::write(dir.path().join("raw.pdf"), "%PDF-1.7\nneedle").expect("fixture");
    let error = DirectSearchService::new()
        .context(options(dir.path(), "needle"))
        .await
        .expect_err("needs index");
    assert!(error.to_string().contains("zg --index"));
}

#[tokio::test]
async fn pdf_rg_multiline_and_invert_match_use_the_embedded_matcher() {
    let dir = fixture();
    let service = DirectSearchService::new();
    let mut request = options(dir.path(), "5 minutes\\.\\n中😀 Recovery");
    request.rg_options = RgOptions {
        multiline: true,
        ..RgOptions::default()
    };
    let result = service.context(request).await.expect("multiline");
    assert_eq!(result.items.len(), 1);
    assert!(matches!(&result.items[0].range, ContentRange::Pdf { pages, .. } if pages == &[2, 3]));
    assert_eq!(result.items[0].match_ranges.len(), 1);
    let mut request = options(dir.path(), "Recovery");
    request.rg_options.invert_match = true;
    let result = service.context(request).await.expect("invert");
    assert_eq!(result.items.len(), 2);
    for item in &result.items {
        let origin = item.content_range.start_byte_offset().expect("offset");
        for window in &item.content_windows {
            let content = &item.content[window.start_byte_offset().expect("offset") - origin
                ..window.end_byte_offset().expect("offset") - origin];
            assert!(
                !content.contains("Recovery"),
                "inverted matches must omit matching lines"
            );
        }
    }
}

#[tokio::test]
async fn pdf_rg_borrows_published_writer_and_excludes_failed_files() {
    use crate::{
        pipelines::indexed_search::writer::{WriterContexts, WriterSession},
        workspace::{
            lock::{LockMode, acquire_workspace_lock},
            manifest::read_workspace_manifest,
        },
    };
    let dir = fixture();
    let layout = WorkspaceLayout::new(dir.path().join(".zvec-grep")).expect("layout");
    let workspace = read_workspace_manifest(dir.path(), &layout)
        .expect("manifest")
        .expect("workspace");
    let schema = workspace
        .index()
        .descriptor()
        .expect("descriptor")
        .default_embedding_model()
        .clone();
    let lock = Arc::new(
        acquire_workspace_lock(&layout, LockMode::Exclusive, "test writer").expect("lock"),
    );
    let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        graph_enabled: false,
        storage_path: layout.index_dir().into(),
        embeddings: vec![schema],
    })
    .expect("store");
    let writers = WriterContexts::default();
    let writer = writers.register(
        WriterSession::new(storage, Vec::new(), workspace, lock),
        true,
    );
    let service = DirectSearchService::new().with_writers(writers);
    let mut request = options(dir.path(), "Recovery");
    request.lock_timeout_ms = Some(0);
    let result = service
        .context(request.clone())
        .await
        .expect("borrow writer instead of waiting on our own lock");
    assert_eq!(result.items.len(), 2);
    let file = writer
        .session
        .storage
        .list_files()
        .expect("files")
        .pop()
        .expect("file");
    writer
        .session
        .storage
        .mark_file_failed(&file, "page 2 failed")
        .expect("failure marker");
    assert!(
        service
            .context(request.clone())
            .await
            .expect_err("old entities are not visible")
            .to_string()
            .contains("zg --index")
    );
    writer.retire().await;
    assert_eq!(
        service
            .context(request)
            .await
            .expect_err("retired writer")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    writer.session.storage.close().expect("close writer");
}
