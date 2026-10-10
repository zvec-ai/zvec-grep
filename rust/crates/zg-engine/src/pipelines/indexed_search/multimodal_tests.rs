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
        model::{
            EmbeddingMetric, EmbeddingModelInfo, EmbeddingRetrieval, EmbeddingSpace, ModelInfo,
        },
    },
    models::ModelError,
    storage::{
        IndexStore,
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
};

struct Runtime {
    info: EmbeddingModelInfo,
    calls: Mutex<Vec<(ContentKind, ContentKind)>>,
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
            info: EmbeddingModelInfo {
                model: ModelInfo::new("test", name, kinds).expect("model"),
                space: EmbeddingSpace::fixture(),
                retrieval: if multimodal {
                    EmbeddingRetrieval::TextImage
                } else {
                    EmbeddingRetrieval::Text
                },
                dimension,
                metric: EmbeddingMetric::Cosine,
                max_batch_size: 32,
                max_input_tokens: None,
                max_image_bytes: None,
            },
            calls: Mutex::new(Vec::new()),
            fail: false,
        }
    }

    fn calls(&self) -> Vec<(ContentKind, ContentKind)> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl SearchEmbeddingRuntime for Runtime {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn embed_queries(
        &self,
        queries: &[Content],
        target: ContentKind,
    ) -> Result<Vec<Vec<f32>>, ModelError> {
        self.calls
            .lock()
            .expect("calls")
            .extend(queries.iter().map(|query| (query.kind(), target)));
        if self.fail {
            return Err(ModelError::internal("intentional inference failure"));
        }
        Ok(queries
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; self.info.dimension];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    workspace: Workspace,
    store: IndexStore,
}

impl Fixture {
    fn new(default: &Runtime, routes: &[(ContentKind, &Runtime)]) -> Self {
        let directory = tempfile::tempdir().expect("index directory");
        let mut descriptor = IndexDescriptor::single(default.info.clone());
        descriptor.routes = routes
            .iter()
            .map(|(kind, model)| (*kind, model.info.clone()))
            .collect();
        let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: directory.path().join("storage"),
            tables: descriptor.tables().expect("tables"),
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
            directory,
            workspace,
            store,
        }
    }

    fn add(&self, path: &str, content: Content, similarity: f32) -> EntityId {
        let relative_path = SourcePath::new(path).expect("path");
        let file_id = self
            .store
            .resolve_file_ids(&[relative_path.to_path_buf()])
            .expect("file id")[0];
        let id = EntityId::new(file_id, &content, Range::Full).expect("entity id");
        let fragment = EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        };
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
            .expect("route")
            .expect("enabled")
            .dimension;
        let mut vector = vec![0.0; dimension];
        vector[0] = similarity;
        vector[1] = (1.0 - similarity * similarity).sqrt();
        let entity = Entity {
            id: id.clone(),
            file_id,
            source_range: Range::Full,
            content,
            metadata: None,
            fragments: vec![fragment.clone()],
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
            .replace_file(
                &file,
                &[entity],
                &[IndexedFragment {
                    entity_id: id.clone(),
                    fragment_id: fragment.id,
                    kind,
                    vector,
                    fts_text: text,
                }],
            )
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
            "generation-test",
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
async fn shared_model_reuses_original_query_and_merges_only_compatible_scores() {
    let model = Runtime::new("shared", 2, true);
    let fixture = Fixture::new(&model, &[]);
    fixture.add_kinds();
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("default search");
    assert_eq!(model.calls(), vec![(ContentKind::Text, ContentKind::Text)]);
    let diagnostics = result.diagnostics.index.expect("diagnostics");
    assert_eq!(diagnostics.targets.len(), 3);
    assert_eq!(diagnostics.result_groups.len(), 1);
    assert_eq!(
        diagnostics.result_groups[0].kinds,
        vec![ContentKind::Text, ContentKind::Code, ContentKind::Image]
    );
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.preview.kind())
            .collect::<Vec<_>>(),
        vec![ContentKind::Image, ContentKind::Text, ContentKind::Code]
    );
    for (rank, item) in result.items.iter().enumerate() {
        assert_eq!(item.rank, rank + 1);
        let reference = item.content_ref.as_ref().expect("snapshot reference");
        assert_eq!(reference.generation, "generation-test");
        let stored = fixture
            .store
            .read_entity(&EntityId::from_string(reference.entity_id.clone()))
            .expect("read snapshot")
            .expect("stored entity");
        assert_eq!(stored.entity.content.kind(), item.preview.kind());
        assert_eq!(stored.file.relative_path.to_path_buf(), item.relative_path);
    }
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
        assert_eq!(model.calls().last(), Some(&(ContentKind::Text, kind)));
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
        Some(&(ContentKind::Image, ContentKind::Text))
    );
}

#[tokio::test]
async fn task_instructions_separate_query_encodings_and_full_text_keeps_kind_groups() {
    let mut model = Runtime::new("task-aware", 2, true);
    model.info.space.code_query_instruction = Some("task: code retrieval | query: ".into());
    let fixture = Fixture::new(&model, &[]);
    fixture.add_kinds();
    let result = fixture
        .search(&[&model], vector_options())
        .await
        .expect("task queries");
    assert_eq!(
        model.calls(),
        vec![
            (ContentKind::Text, ContentKind::Text),
            (ContentKind::Text, ContentKind::Code)
        ]
    );
    let groups = result.diagnostics.index.expect("diagnostics").result_groups;
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].kinds, vec![ContentKind::Text, ContentKind::Image]);
    assert_eq!(groups[1].kinds, vec![ContentKind::Code]);

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
        vec![(ContentKind::Image, ContentKind::Image)]
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
    assert!(error.message().contains("does not support image to text"));

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
        Some(crate::api::context::result::EmptyReason::NoSearchableFiles)
    );
    assert!(
        empty
            .diagnostics
            .index
            .expect("diagnostics")
            .targets
            .iter()
            .all(|target| target.status == IndexTargetStatus::Empty)
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
