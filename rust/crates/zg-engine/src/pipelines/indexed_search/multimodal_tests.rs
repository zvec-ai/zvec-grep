use std::{collections::BTreeMap, io::Cursor, sync::Mutex};

use async_trait::async_trait;
use image::{DynamicImage, ImageFormat, RgbImage};

use super::{
    context::{context_from_index, normalize_context_request},
    pipeline::SearchEmbeddingRuntime,
};
use crate::{
    EngineResult,
    api::context::{
        ContextOptions, ContextResult,
        options::{ContextRoute, ContextRouteMode, QueryFilter, QueryImage},
        result::{IndexScoring, IndexTargetStatus},
    },
    domain::{
        Content, ContentKind, Entity, EntityFragment, EntityId, FileFormat, FileIndexStatus,
        FileRecord, FileSnapshot, FragmentId, ImageContent, IndexDescriptor, IndexState, Range,
        SourcePath, Workspace,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    },
    models::{ModelError, QueryEncoding},
    storage::{
        IndexStore,
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
};

struct Runtime {
    info: EmbeddingModelInfo,
    calls: Mutex<Vec<(ContentKind, QueryEncoding)>>,
    code_query: bool,
    pinned: bool,
    fail: bool,
}

impl Runtime {
    fn new(name: &str, dimension: usize, multimodal: bool) -> Self {
        let kinds = if multimodal {
            vec![ContentKind::Text, ContentKind::Code, ContentKind::Image]
        } else {
            vec![ContentKind::Text, ContentKind::Code]
        };
        Self {
            info: EmbeddingModelInfo::new(
                ModelInfo::new("test", name, kinds, 32).expect("model"),
                dimension,
                EmbeddingMetric::Cosine,
                None,
                None,
            )
            .expect("embedding model"),
            calls: Mutex::new(Vec::new()),
            code_query: false,
            pinned: true,
            fail: false,
        }
    }

    fn calls(&self) -> Vec<(ContentKind, QueryEncoding)> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl SearchEmbeddingRuntime for Runtime {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    fn query_encoding(
        &self,
        input: ContentKind,
        target: ContentKind,
    ) -> Result<QueryEncoding, ModelError> {
        Ok(
            if self.code_query && input != ContentKind::Image && target == ContentKind::Code {
                QueryEncoding::TextPrefix("task: code retrieval | query: ")
            } else {
                QueryEncoding::Default
            },
        )
    }

    fn has_pinned_weights(&self) -> bool {
        self.pinned
    }

    async fn embed_queries(
        &self,
        queries: &[Content],
        encoding: QueryEncoding,
    ) -> Result<Vec<Vec<f32>>, ModelError> {
        self.calls
            .lock()
            .expect("calls")
            .extend(queries.iter().map(|query| (query.kind(), encoding)));
        if self.fail {
            return Err(ModelError::internal("intentional inference failure"));
        }
        Ok(queries
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; self.info.dimension()];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }
}

struct Fixture {
    store: IndexStore,
    workspace: Workspace,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new(default: &Runtime, routes: &[(ContentKind, &Runtime)]) -> Self {
        let directory = tempfile::tempdir().expect("index directory");
        let descriptor = IndexDescriptor::new(
            default.info.clone(),
            routes
                .iter()
                .map(|(kind, model)| (*kind, model.info.clone()))
                .collect(),
            crate::domain::FTS_CONFIG,
        )
        .expect("index descriptor");
        let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: directory.path().join("storage"),
            tables: descriptor.content_indexes(),
        })
        .expect("open index");
        let workspace = Workspace {
            name: "multimodal-test".into(),
            root: directory.path().to_path_buf(),
            scan: crate::domain::ScanRules::default(),
            index: IndexState::Enabled(Box::new(descriptor)),
            created_epoch_ms: 0,
            updated_epoch_ms: 0,
        };
        Self {
            store,
            workspace,
            directory,
        }
    }

    fn add(&self, path: &str, content: Content, similarity: f32) -> EntityId {
        self.add_fragments(path, content, None, &[similarity])
    }

    fn add_fragments(
        &self,
        path: &str,
        content: Content,
        metadata: Option<crate::domain::EntityMetadata>,
        similarities: &[f32],
    ) -> EntityId {
        let relative_path = SourcePath::new(path).expect("path");
        let file_id = self
            .store
            .resolve_file_ids(&[relative_path.to_path_buf()])
            .expect("file id")[0];
        let id = EntityId::new(file_id, &content, Range::Full).expect("entity id");
        let fragments = similarities
            .iter()
            .enumerate()
            .map(|(ordinal, _)| EntityFragment {
                id: FragmentId::new(&id, u32::try_from(ordinal).expect("small fixture")),
                range: Range::Full,
            })
            .collect::<Vec<_>>();
        let kind = content.kind();
        let text = match &content {
            Content::Text(text) | Content::Code(text) => text.clone(),
            Content::Image(_) => String::new(),
        };
        let bytes = match &content {
            Content::Text(text) | Content::Code(text) => text.as_bytes(),
            Content::Image(image) => image.data(),
        };
        std::fs::write(self.directory.path().join(path), bytes).expect("source file");
        let snapshot = FileSnapshot {
            size_bytes: u64::try_from(bytes.len()).expect("small fixture"),
            modified_epoch_ms: None,
            content_hash: Some(crate::utils::sha256_hex(bytes)),
        };
        let dimension = self
            .workspace
            .index
            .descriptor()
            .expect("descriptor")
            .model_for(kind)
            .expect("enabled")
            .dimension();
        let projections = fragments
            .iter()
            .zip(similarities)
            .map(|(fragment, similarity)| {
                let mut vector = vec![0.0; dimension];
                vector[0] = *similarity;
                vector[1] = (1.0 - similarity * similarity).sqrt();
                IndexedFragment {
                    entity_id: id.clone(),
                    fragment_id: fragment.id.clone(),
                    kind,
                    vector,
                    fts_text: text.clone(),
                }
            })
            .collect::<Vec<_>>();
        let entity = Entity {
            id: id.clone(),
            file_id,
            source_range: Range::Full,
            content,
            metadata,
            fragments,
        };
        let file = FileRecord {
            id: file_id,
            relative_path,
            snapshot,
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 0,
                entity_count: 1,
            },
        };
        self.store
            .replace_file(&file, &[entity], &projections)
            .expect("write file");
        id
    }

    fn add_kinds(&self) {
        self.add(
            "guide.txt",
            Content::Text("orchard apples guide".into()),
            0.8,
        );
        self.add(
            "harvest.rs",
            Content::Code("fn orchard() { harvest_apples(); }".into()),
            0.6,
        );
        self.add("apple.png", image(), 1.0);
    }

    async fn search(
        &self,
        models: &[&dyn SearchEmbeddingRuntime],
        options: ContextOptions,
    ) -> EngineResult<ContextResult> {
        let request = normalize_context_request(&options)?;
        context_from_index(
            self.directory.path(),
            &self.workspace,
            self.directory.path(),
            &self.store,
            models,
            &options,
            &request,
        )
        .await
    }
}

fn image_bytes() -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::new(2, 2))
        .write_to(&mut output, ImageFormat::Png)
        .expect("PNG");
    output.into_inner()
}

fn image() -> Content {
    Content::Image(ImageContent::new(image_bytes(), FileFormat::Png).expect("image"))
}

fn vector_options() -> ContextOptions {
    ContextOptions {
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Vector,
            query: "orchard".into(),
        }],
        prefer_symbol: false,
        limit: Some(10),
        ..ContextOptions::default()
    }
}

#[tokio::test]
async fn pooled_vector_ranks_fragments_globally_before_promoting_a_definition() {
    let model = Runtime::new("shared", 2, false);
    let fixture = Fixture::new(&model, &[]);
    fixture.add_fragments(
        "guide.txt",
        Content::Text("how ParseConfig is used".into()),
        None,
        &[0.99, 0.95],
    );
    fixture.add_fragments(
        "parser.rs",
        Content::Code("struct ParseConfig {}".into()),
        Some(crate::domain::EntityMetadata::Code(
            crate::domain::CodeMetadata {
                symbol_type: Some(crate::domain::SymbolType::Class),
                symbol_name: Some("ParseConfig".into()),
                scope: None,
                signature: None,
                documentation: None,
            },
        )),
        &[0.5],
    );
    let result = fixture
        .search(
            &[&model],
            ContextOptions {
                routes: vec![ContextRoute {
                    mode: ContextRouteMode::Vector,
                    query: "ParseConfig".into(),
                }],
                trace: true,
                limit: Some(1),
                ..ContextOptions::default()
            },
        )
        .await
        .expect("pooled rules");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path.to_str(), Some("parser.rs"));
    let trace = result.items[0].trace.as_ref().expect("trace");
    assert_eq!(
        trace.recall[0].rank,
        Some(3),
        "both text fragments precede the definition"
    );
    assert_eq!(trace.fusion.rank, 2);
    assert!((trace.fusion.score - 1.0 / 63.0).abs() < 1e-10);
    assert!(trace.ranking.as_ref().expect("rules").definition_bonus > 0.0);
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kinds, vec![ContentKind::Text, ContentKind::Code]);
    assert!(groups[0].rule_ranking);
    assert_eq!(model.calls().len(), 1);
}

#[tokio::test]
async fn fts_pools_different_models_using_native_scores_before_rules() {
    let text = Runtime::new("text", 2, false);
    let code = Runtime::new("code", 3, false);
    let fixture = Fixture::new(&text, &[(ContentKind::Code, &code)]);
    let text_id = fixture.add(
        "guide.txt",
        Content::Text("orchard orchard orchard".into()),
        0.9,
    );
    let code_id = fixture.add(
        "fruit.rs",
        Content::Code("orchard banana pear grape".into()),
        0.5,
    );
    let mut expected = [ContentKind::Text, ContentKind::Code]
        .into_iter()
        .flat_map(|kind| {
            fixture
                .store
                .search_fts(kind, "orchard", 10, None)
                .expect("native recall")
        })
        .collect::<Vec<_>>();
    expected.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.document_id.cmp(&b.document_id))
    });
    assert_eq!(expected.len(), 2);
    let result = fixture
        .search(
            &[],
            ContextOptions {
                routes: vec![ContextRoute {
                    mode: ContextRouteMode::Fts,
                    query: "orchard".into(),
                }],
                trace: true,
                ..ContextOptions::default()
            },
        )
        .await
        .expect("FTS does not need either model runtime");
    for item in &result.items {
        let trace = item.trace.as_ref().expect("trace");
        let id = if item.preview.kind() == ContentKind::Text {
            &text_id
        } else {
            &code_id
        };
        let rank = expected
            .iter()
            .position(|hit| &hit.entity_id == id)
            .expect("recalled entity")
            + 1;
        assert_eq!(trace.recall[0].rank, Some(rank));
        assert!(trace.ranking.is_some());
    }
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kinds, vec![ContentKind::Text, ContentKind::Code]);
    assert_eq!(groups[0].model_refs, vec!["test/text", "test/code"]);
    assert_eq!(groups[0].scoring, IndexScoring::FullText);
    assert!(groups[0].rule_ranking);
    assert!(text.calls().is_empty() && code.calls().is_empty());
}

#[tokio::test]
async fn pooled_mixed_routes_preserve_fused_and_independent_query_groups() {
    let model = Runtime::new("shared", 2, false);
    let fixture = Fixture::new(&model, &[]);
    fixture.add("guide.txt", Content::Text("orchard guide".into()), 0.6);
    fixture.add("fruit.rs", Content::Code("fn harvest() {}".into()), 0.9);
    for fuse in [false, true] {
        let result = fixture
            .search(
                &[&model],
                ContextOptions {
                    routes: vec![
                        ContextRoute {
                            mode: ContextRouteMode::Fts,
                            query: "orchard".into(),
                        },
                        ContextRoute {
                            mode: ContextRouteMode::Vector,
                            query: "harvest".into(),
                        },
                    ],
                    fuse,
                    trace: true,
                    ..ContextOptions::default()
                },
            )
            .await
            .expect("mixed query");
        let index = result.diagnostics.index.expect("diagnostics");
        assert_eq!(index.result_groups.len(), 1);
        assert_eq!(
            index.result_groups[0].kinds,
            vec![ContentKind::Text, ContentKind::Code]
        );
        assert_eq!(
            index.result_groups[0].scoring,
            if fuse {
                IndexScoring::Hybrid
            } else {
                IndexScoring::MultiQuery
            }
        );
        assert_eq!(index.query_groups.len(), if fuse { 1 } else { 2 });
        for item in &result.items {
            assert!(item.trace.as_ref().expect("trace").ranking.is_some());
        }
        let code = result
            .items
            .iter()
            .find(|item| item.preview.kind() == ContentKind::Code)
            .expect("vector-only code candidate");
        assert_eq!(code.query_groups.len(), 1);
        assert_eq!(code.query_groups[0].id, if fuse { "Q1" } else { "Q2" });
        let text = result
            .items
            .iter()
            .find(|item| item.preview.kind() == ContentKind::Text)
            .expect("text candidate");
        assert_eq!(text.query_groups.len(), if fuse { 1 } else { 2 });
    }
}

#[tokio::test]
async fn image_query_display_path_does_not_become_rule_ranking_evidence() {
    let model = Runtime::new("shared", 2, true);
    let fixture = Fixture::new(&model, &[]);
    fixture.add("orchard.txt", Content::Text("a guide".into()), 0.8);
    fixture.add("banana.txt", Content::Text("a different guide".into()), 0.7);
    let image_path = fixture.directory.path().join("banana.png");
    std::fs::write(&image_path, image_bytes()).expect("query image");
    let mut observed = Vec::new();
    for query_image in [
        QueryImage::Path { path: image_path },
        QueryImage::Bytes {
            format: FileFormat::Png,
            data: image_bytes(),
        },
    ] {
        let result = fixture
            .search(
                &[&model],
                ContextOptions {
                    query_image: Some(query_image),
                    target_kind: Some(ContentKind::Text),
                    trace: true,
                    ..ContextOptions::default()
                },
            )
            .await
            .expect("image query against text");
        observed.push(
            result
                .items
                .iter()
                .map(|item| {
                    let ranking = item
                        .trace
                        .as_ref()
                        .expect("trace")
                        .ranking
                        .as_ref()
                        .expect("rules");
                    assert!(ranking.path_bonus.abs() < f64::EPSILON);
                    (item.relative_path.clone(), item.score)
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(observed[0], observed[1]);
}

#[tokio::test]
async fn shared_model_pools_text_and_code_before_rules_and_keeps_images_separate() {
    let mut model = Runtime::new("shared", 2, true);
    let fixture = Fixture::new(&model, &[]);
    fixture.add_kinds();
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("default search");
    assert_eq!(
        model.calls(),
        vec![(ContentKind::Text, QueryEncoding::Default)]
    );
    let diagnostics = result.diagnostics.index.expect("diagnostics");
    assert_eq!(diagnostics.targets.len(), 3);
    assert_eq!(diagnostics.result_groups.len(), 2);
    assert_eq!(
        diagnostics.result_groups[0].kinds,
        vec![ContentKind::Text, ContentKind::Code]
    );
    assert!(diagnostics.result_groups[0].rule_ranking);
    assert!(!diagnostics.result_groups[1].rule_ranking);
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.preview.kind())
            .collect::<Vec<_>>(),
        vec![ContentKind::Text, ContentKind::Code, ContentKind::Image]
    );
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.rank)
            .collect::<Vec<_>>(),
        vec![1, 2, 1]
    );
    for kind in [ContentKind::Code, ContentKind::Image] {
        let options = ContextOptions {
            target_kind: Some(kind),
            ..vector_options()
        };
        let result = fixture
            .search(&[&model], options)
            .await
            .expect("explicit target");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].preview.kind(), kind);
        assert_eq!(
            model.calls().last(),
            Some(&(ContentKind::Text, QueryEncoding::Default))
        );
    }
    let calls = model.calls().len();
    let result = fixture
        .search(
            &[&model],
            ContextOptions {
                query_image: Some(QueryImage::Bytes {
                    format: FileFormat::Png,
                    data: image_bytes(),
                }),
                prefer_symbol: false,
                ..ContextOptions::default()
            },
        )
        .await
        .expect("image to all supported targets");
    assert_eq!(result.items.len(), 3);
    assert_eq!(model.calls().len(), calls + 1);
    assert_eq!(
        model.calls().last(),
        Some(&(ContentKind::Image, QueryEncoding::Default))
    );

    model.pinned = false;
    let calls = model.calls().len();
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("mutable remote model");
    assert_eq!(model.calls().len(), calls + 1);
    assert_eq!(
        result
            .diagnostics
            .index
            .expect("diagnostics")
            .result_groups
            .len(),
        3,
        "query reuse does not imply cross-table score comparability"
    );
}

#[tokio::test]
async fn query_instructions_separate_vector_and_hybrid_groups_without_rebuilding() {
    let mut model = Runtime::new("task-aware", 2, true);
    let fixture = Fixture::new(&model, &[]);
    fixture.add_kinds();
    model.code_query = true;
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("task queries");
    assert_eq!(
        model.calls(),
        vec![
            (ContentKind::Text, QueryEncoding::Default),
            (
                ContentKind::Text,
                QueryEncoding::TextPrefix("task: code retrieval | query: ")
            )
        ]
    );
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].kinds, vec![ContentKind::Text]);
    assert_eq!(groups[1].kinds, vec![ContentKind::Code]);
    assert_eq!(groups[2].kinds, vec![ContentKind::Image]);

    let options = ContextOptions {
        query: Some("orchard".into()),
        prefer_symbol: true,
        ..ContextOptions::default()
    };
    let result = fixture
        .search(&[&model], options)
        .await
        .expect("hybrid search");
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].scoring, IndexScoring::Hybrid);
    assert_eq!(groups[1].scoring, IndexScoring::Hybrid);
    assert_eq!(groups[2].scoring, IndexScoring::Vector);

    model.code_query = false;
    let calls = model.calls().len();
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("query instruction changes do not require rebuilding");
    assert_eq!(model.calls().len(), calls + 1);
    assert_eq!(
        result
            .diagnostics
            .index
            .expect("diagnostics")
            .result_groups
            .len(),
        2
    );
}

#[tokio::test]
async fn different_models_and_dimensions_group_with_total_limit_and_path_filter() {
    let text = Runtime::new("text-2d", 2, false);
    let code = Runtime::new("code-3d", 3, false);
    let vision = Runtime::new("vision-4d", 4, true);
    let fixture = Fixture::new(
        &text,
        &[(ContentKind::Code, &code), (ContentKind::Image, &vision)],
    );
    fixture.add_kinds();
    let models: [&dyn SearchEmbeddingRuntime; 3] = [&text, &code, &vision];
    let result = fixture
        .search(
            &models,
            ContextOptions {
                limit: Some(2),
                ..vector_options()
            },
        )
        .await
        .expect("mixed dimensions");
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.items[0].preview.kind(), ContentKind::Text);
    assert_eq!(result.items[1].preview.kind(), ContentKind::Code);
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(
        groups
            .iter()
            .map(|group| group.item_count)
            .collect::<Vec<_>>(),
        vec![1, 1, 0]
    );
    assert_eq!(text.calls().len(), 1);
    assert_eq!(code.calls().len(), 1);
    assert_eq!(vision.calls().len(), 1);

    fixture.add(
        "keep.txt",
        Content::Text("less similar but allowed".into()),
        0.1,
    );
    let result = fixture
        .search(
            &models,
            ContextOptions {
                target_kind: Some(ContentKind::Text),
                limit: Some(1),
                filter: QueryFilter {
                    globs: vec!["keep.txt".into()],
                    ..QueryFilter::default()
                },
                ..vector_options()
            },
        )
        .await
        .expect("filtered top result");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path.to_str(), Some("keep.txt"));
    for limit in [0, 2_001, usize::MAX] {
        let error = fixture
            .search(
                &models,
                ContextOptions {
                    limit: Some(limit),
                    ..vector_options()
                },
            )
            .await
            .expect_err("invalid total cap");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
    }
}

#[tokio::test]
async fn image_query_skips_unsupported_default_paths_but_explicit_target_errors() {
    let text = Runtime::new("text", 2, false);
    let vision = Runtime::new("vision", 3, true);
    let fixture = Fixture::new(&text, &[(ContentKind::Image, &vision)]);
    fixture.add_kinds();
    let options = ContextOptions {
        query_image: Some(QueryImage::Bytes {
            format: FileFormat::Png,
            data: image_bytes(),
        }),
        prefer_symbol: false,
        ..ContextOptions::default()
    };
    let result = fixture
        .search(&[&text, &vision], options.clone())
        .await
        .expect("image search");
    assert!(text.calls().is_empty());
    assert_eq!(
        vision.calls(),
        vec![(ContentKind::Image, QueryEncoding::Default)]
    );
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].preview.kind(), ContentKind::Image);
    let statuses = result
        .diagnostics
        .index
        .expect("diagnostics")
        .targets
        .into_iter()
        .map(|target| (target.kind, target.status))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(statuses[&ContentKind::Text], IndexTargetStatus::Skipped);
    assert_eq!(statuses[&ContentKind::Code], IndexTargetStatus::Skipped);
    let error = fixture
        .search(
            &[&text, &vision],
            ContextOptions {
                target_kind: Some(ContentKind::Text),
                ..options
            },
        )
        .await
        .expect_err("unsupported explicit path");
    assert!(
        error
            .message()
            .contains("does not support image query input for the text table")
    );

    let text_only = Fixture::new(&text, &[]);
    let error = text_only
        .search(
            &[&text],
            ContextOptions {
                target_kind: Some(ContentKind::Image),
                ..vector_options()
            },
        )
        .await
        .expect_err("disabled image target");
    assert!(error.message().contains("not enabled"));
    let empty = text_only
        .search(&[&text], vector_options())
        .await
        .expect("empty index");
    assert_eq!(
        empty.diagnostics.empty_reason,
        Some(crate::api::context::result::EmptyReason::NoMatches)
    );
    assert!(empty.items.is_empty());
    assert_eq!(
        text.calls(),
        vec![(ContentKind::Text, QueryEncoding::Default)],
        "empty tables follow the normal search path and share query encoding"
    );
    assert!(
        empty
            .diagnostics
            .index
            .expect("diagnostics")
            .targets
            .iter()
            .all(|target| target.status == IndexTargetStatus::Searched)
    );
}

#[tokio::test]
async fn failed_target_is_incomplete_in_default_results_and_fails_explicit_requests() {
    let text = Runtime::new("text", 2, false);
    let mut vision = Runtime::new("failing-vision", 3, true);
    vision.fail = true;
    let fixture = Fixture::new(&text, &[(ContentKind::Image, &vision)]);
    fixture.add_kinds();
    let result = fixture
        .search(&[&text, &vision], vector_options())
        .await
        .expect("partial results");
    assert_eq!(result.items.len(), 2);
    let diagnostics = result.diagnostics.index.expect("diagnostics");
    assert!(diagnostics.incomplete);
    let failed = diagnostics
        .targets
        .iter()
        .find(|target| target.kind == ContentKind::Image)
        .expect("image target");
    assert_eq!(failed.status, IndexTargetStatus::Failed);
    assert!(
        failed
            .reason
            .as_deref()
            .expect("failure reason")
            .contains("intentional inference failure")
    );
    let error = fixture
        .search(
            &[&text, &vision],
            ContextOptions {
                target_kind: Some(ContentKind::Image),
                ..vector_options()
            },
        )
        .await
        .expect_err("explicit inference failure");
    assert!(error.message().contains("intentional inference failure"));
}

struct CodeVectorFailure<'a>(&'a IndexStore);

impl super::storage::SearchStorage for CodeVectorFailure<'_> {
    fn list_file_paths(&self) -> EngineResult<Vec<(crate::domain::FileId, std::path::PathBuf)>> {
        self.0.list_file_paths()
    }
    fn list_file_attributes(
        &self,
    ) -> EngineResult<Vec<crate::storage::types::StoredFileAttributes>> {
        self.0.list_file_attributes()
    }
    fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        self.0.has_non_unicode_file_names()
    }
    fn load_search_hits(
        &self,
        hits: &[crate::storage::types::StorageSearchHit],
    ) -> EngineResult<crate::storage::types::StoredSearchData> {
        self.0.load_search_hits(hits)
    }
    fn search_fts(
        &self,
        kind: ContentKind,
        query: &str,
        limit: usize,
        filter: Option<&crate::storage::types::StorageSearchFilter>,
    ) -> EngineResult<Vec<crate::storage::types::StorageSearchHit>> {
        self.0.search_fts(kind, query, limit, filter)
    }
    fn search_vector(
        &self,
        kind: ContentKind,
        vector: &[f32],
        limit: usize,
        filter: Option<&crate::storage::types::StorageSearchFilter>,
    ) -> EngineResult<Vec<crate::storage::types::StorageSearchHit>> {
        if kind == ContentKind::Code {
            return Err(crate::EngineError::storage_failure(
                "intentional code recall failure",
            ));
        }
        self.0.search_vector(kind, vector, limit, filter)
    }
}

#[tokio::test]
async fn failed_table_does_not_leak_earlier_routes_into_its_compatible_pool() {
    let model = Runtime::new("shared", 2, false);
    let fixture = Fixture::new(&model, &[]);
    fixture.add("guide.txt", Content::Text("orchard guide".into()), 0.8);
    fixture.add("fruit.rs", Content::Code("fn orchard() {}".into()), 0.9);
    let options = ContextOptions {
        query: Some("orchard".into()),
        ..ContextOptions::default()
    };
    let request = normalize_context_request(&options).expect("request");
    let storage = CodeVectorFailure(&fixture.store);
    let result = context_from_index(
        fixture.directory.path(),
        &fixture.workspace,
        fixture.directory.path(),
        &storage,
        &[&model],
        &options,
        &request,
    )
    .await
    .expect("partial text result");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].preview.kind(), ContentKind::Text);
    let index = result.diagnostics.index.expect("diagnostics");
    assert!(index.incomplete);
    assert_eq!(index.result_groups.len(), 1);
    assert_eq!(index.result_groups[0].kinds, vec![ContentKind::Text]);
    assert_eq!(index.targets[1].status, IndexTargetStatus::Failed);
    assert!(
        index.targets[1]
            .reason
            .as_deref()
            .expect("reason")
            .contains("intentional code recall failure")
    );
    let error = context_from_index(
        fixture.directory.path(),
        &fixture.workspace,
        fixture.directory.path(),
        &storage,
        &[&model],
        &ContextOptions {
            target_kind: Some(ContentKind::Code),
            ..options
        },
        &request,
    )
    .await
    .expect_err("explicit table fails");
    assert!(error.message().contains("intentional code recall failure"));
}
