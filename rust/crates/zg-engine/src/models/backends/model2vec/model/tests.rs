use std::{
    path::Path,
    sync::{
        Arc, Barrier, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use tempfile::TempDir;

use crate::domain::{Content, model::Metric};
use crate::{
    domain::model::{EmbeddingPurpose, ModelConfig, ModelProgress},
    models::{
        artifacts::ArtifactDownloadProgress,
        catalog::Model2VecConfig,
        spi::{EmbeddingModel, EmbeddingOptions},
    },
};

use super::{
    Model2VecDependencies, Model2VecEmbeddingModel, Model2VecResolvedArtifacts,
    ModelDownloadProgressReporter, StaticEmbeddingTable, TokenizerRuntime,
};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn matches_typescript_model2vec_oracle_and_reuses_loaded_assets() {
    let root = TempDir::new().expect("temporary directory should be created");
    let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::Oracle));
    let model = Model2VecEmbeddingModel::with_dependencies(
        fixture_entry(),
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies.clone(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    assert_eq!(model.concurrency_defaults().initial, 2);

    let progress = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&progress);
    let result = model
        .embed(
            &[
                vec![Content::Text("both tokens".to_owned())],
                vec![Content::Text("unknown-only".to_owned())],
                vec![Content::Text("third token".to_owned())],
            ],
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                on_progress: Some(Arc::new(move |event| {
                    captured
                        .lock()
                        .expect("progress lock should not be poisoned")
                        .push(event);
                })),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("fixture inputs should embed");

    assert!(result.truncated.is_empty());
    assert!((result.vectors[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-7);
    assert!((result.vectors[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-7);
    assert!(result.vectors[0][2].abs() <= f32::EPSILON);
    assert_eq!(result.vectors[1], [0.0, 0.0, 0.0]);
    assert_eq!(result.vectors[2], [0.0, 0.0, 1.0]);
    assert_eq!(
        dependencies
            .tokenizer
            .texts
            .lock()
            .expect("tokenizer lock should not be poisoned")
            .as_slice(),
        [
            "query: both tokens",
            "query: unknown-only",
            "query: third token",
        ]
    );
    assert_eq!(
        *progress
            .lock()
            .expect("progress lock should not be poisoned"),
        [
            ModelProgress::Preparing {
                model: "local/test-potion".to_owned(),
            },
            ModelProgress::Downloading {
                model: "local/test-potion".to_owned(),
                downloaded_bytes: Some(4),
                total_bytes: Some(16),
            },
            ModelProgress::Downloading {
                model: "local/test-potion".to_owned(),
                downloaded_bytes: Some(8),
                total_bytes: Some(16),
            },
            ModelProgress::Ready {
                model: "local/test-potion".to_owned(),
            },
        ]
    );
    assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
    assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
    assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);

    model
        .embed(
            &[vec![Content::Text("cached".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect("loaded model should be reused");
    assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
    assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
    assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);

    let loaded = {
        let state = model.state.lock().await;
        Arc::downgrade(state.loaded.as_ref().expect("loaded model"))
    };
    drop(model);
    assert!(
        loaded.upgrade().is_none(),
        "dropping the model releases its loaded resources"
    );
}

#[tokio::test]
async fn concurrent_embeddings_share_one_lazy_loaded_runtime() {
    let root = TempDir::new().expect("temporary directory should be created");
    let dependencies = Arc::new(FixtureDependencies::new_concurrent());
    let model = Model2VecEmbeddingModel::with_dependencies(
        fixture_entry(),
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies.clone(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let first = [vec![Content::Text("first".to_owned())]];
    let second = [vec![Content::Text("second".to_owned())]];

    let (first_result, second_result) = tokio::join!(
        model.embed(&first, EmbeddingOptions::default()),
        model.embed(&second, EmbeddingOptions::default()),
    );

    assert_eq!(
        first_result
            .expect("first concurrent embedding should complete")
            .vectors
            .len(),
        1
    );
    assert_eq!(
        second_result
            .expect("second concurrent embedding should complete")
            .vectors
            .len(),
        1
    );
    assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
    assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
    assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);
    assert_eq!(
        dependencies
            .tokenizer
            .maximum_active
            .load(Ordering::Acquire),
        2
    );
}

#[tokio::test]
async fn reports_truncation_and_validates_inputs_like_typescript() {
    let root = TempDir::new().expect("temporary directory should be created");
    let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::Truncated));
    let mut entry = fixture_entry();
    entry.max_input_tokens = 2;
    let model = Model2VecEmbeddingModel::with_dependencies(
        entry,
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies,
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = model
        .embed(
            &[vec![Content::Text("too many tokens".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect("long fixture should be truncated");
    assert_eq!(result.truncated, [0]);
    assert!((result.vectors[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-7);
    assert!((result.vectors[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-7);
    assert!(result.vectors[0][2].abs() <= f32::EPSILON);

    let empty = model
        .embed(&[], EmbeddingOptions::default())
        .await
        .expect_err("empty batch must fail");
    assert_eq!(empty.code(), crate::EngineError::INVALID_ARGUMENT);
    let blank = model
        .embed(
            &[vec![Content::Text("  ".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect_err("blank text must fail");
    assert_eq!(blank.code(), crate::EngineError::INVALID_ARGUMENT);
}

#[tokio::test]
async fn excludes_cached_artifacts_from_download_progress_like_typescript() {
    let root = TempDir::new().expect("temporary directory should be created");
    let tokenizer_path = root
        .path()
        .join("model2vec")
        .join("test--potion")
        .join("0123456789abcdef")
        .join("tokenizer")
        .join("tokenizer.json");
    tokio::fs::create_dir_all(
        tokenizer_path
            .parent()
            .expect("tokenizer path should have a parent"),
    )
    .await
    .expect("tokenizer cache directory should be created");
    tokio::fs::write(&tokenizer_path, b"{}")
        .await
        .expect("cached tokenizer should be written");

    let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::Oracle));
    let model = Model2VecEmbeddingModel::with_dependencies(
        fixture_entry(),
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies.clone(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let progress = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&progress);

    model
        .embed(
            &[vec![Content::Text("cached tokenizer".to_owned())]],
            EmbeddingOptions {
                on_progress: Some(Arc::new(move |event| {
                    captured
                        .lock()
                        .expect("progress lock should not be poisoned")
                        .push(event);
                })),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("cached tokenizer fixture should embed");

    assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 1);
    assert_eq!(
        *progress
            .lock()
            .expect("progress lock should not be poisoned"),
        [
            ModelProgress::Preparing {
                model: "local/test-potion".to_owned(),
            },
            ModelProgress::Downloading {
                model: "local/test-potion".to_owned(),
                downloaded_bytes: Some(4),
                total_bytes: Some(8),
            },
            ModelProgress::Ready {
                model: "local/test-potion".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn rejects_out_of_range_token_ids_like_typescript() {
    let root = TempDir::new().expect("temporary directory should be created");
    let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::OutOfRange));
    let model = Model2VecEmbeddingModel::with_dependencies(
        fixture_entry(),
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies,
        crate::models::runtime::ModelComputeRuntime::shared(),
    );

    let error = model
        .embed(
            &[vec![Content::Text("invalid token".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect_err("out-of-range token id should fail");

    assert_eq!(error.code(), crate::EngineError::INTERNAL);
    assert!(
        error
            .cause()
            .is_some_and(|cause| cause.contains("out-of-range token id"))
    );
}

#[tokio::test]
async fn rejects_cancelled_embeddings_without_corrupting_the_loaded_runtime() {
    let root = TempDir::new().expect("temporary directory should be created");
    let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::Oracle));
    let model = Model2VecEmbeddingModel::with_dependencies(
        fixture_entry(),
        ModelConfig {
            cache_dir: Some(root.path().to_path_buf()),
            ..ModelConfig::default()
        },
        dependencies.clone(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();

    let error = model
        .embed(
            &[vec![Content::Text("cancelled".to_owned())]],
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect_err("cancelled embedding should fail");

    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert!(
        error.to_string().contains("cancelled")
            || error
                .cause()
                .is_some_and(|cause| cause.contains("cancelled"))
    );
    model
        .embed(
            &[vec![Content::Text("after cancellation".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect("cancellation should not poison the shared runtime");
    assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
    assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
    assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);
}

fn fixture_entry() -> Model2VecConfig {
    let download =
        crate::models::catalog::get_embedding_model_catalog_entry("local/potion-code-16m-v2")
            .and_then(crate::models::catalog::EmbeddingCatalogEntry::model2vec_config)
            .expect("fixture download metadata")
            .download;
    Model2VecConfig {
        reference: "local/test-potion",
        provider: "local",
        model: "test-potion",
        repo: "test/potion",
        revision: "0123456789abcdef",
        download,
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 3,
        metric: Metric::Cosine,
        normalize: true,
        max_input_tokens: 512,
        max_batch_size: 32,
        default_concurrency: 2,
        query_prefix: Some("query: "),
        document_prefix: Some("passage: "),
    }
}

#[derive(Clone, Copy)]
enum FixtureTokenizerMode {
    Oracle,
    OutOfRange,
    Truncated,
}

struct FixtureTokenizer {
    mode: FixtureTokenizerMode,
    texts: StdMutex<Vec<String>>,
    barrier: Option<Barrier>,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
}

impl TokenizerRuntime for FixtureTokenizer {
    fn encode(&self, text: &str) -> Result<Vec<u32>, crate::models::spi::ModelError> {
        self.texts
            .lock()
            .expect("tokenizer lock should not be poisoned")
            .push(text.to_owned());
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.maximum_active.fetch_max(active, Ordering::AcqRel);
        if let Some(barrier) = &self.barrier {
            barrier.wait();
        }
        self.active.fetch_sub(1, Ordering::AcqRel);
        Ok(match self.mode {
            FixtureTokenizerMode::Truncated => vec![0, 1, 2],
            FixtureTokenizerMode::OutOfRange => vec![3],
            FixtureTokenizerMode::Oracle if text.contains("unknown-only") => vec![99],
            FixtureTokenizerMode::Oracle if text.contains("both") => vec![0, 1],
            FixtureTokenizerMode::Oracle => vec![2],
        })
    }

    fn unknown_token_id(&self) -> Option<u32> {
        Some(99)
    }
}

struct FixtureDependencies {
    tokenizer: Arc<FixtureTokenizer>,
    downloads: AtomicUsize,
    tokenizer_loads: AtomicUsize,
    table_loads: AtomicUsize,
}

impl FixtureDependencies {
    fn new(mode: FixtureTokenizerMode) -> Self {
        Self {
            tokenizer: Arc::new(FixtureTokenizer {
                mode,
                texts: StdMutex::new(Vec::new()),
                barrier: None,
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }),
            downloads: AtomicUsize::new(0),
            tokenizer_loads: AtomicUsize::new(0),
            table_loads: AtomicUsize::new(0),
        }
    }

    fn new_concurrent() -> Self {
        Self {
            tokenizer: Arc::new(FixtureTokenizer {
                mode: FixtureTokenizerMode::Oracle,
                texts: StdMutex::new(Vec::new()),
                barrier: Some(Barrier::new(2)),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }),
            downloads: AtomicUsize::new(0),
            tokenizer_loads: AtomicUsize::new(0),
            table_loads: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Model2VecDependencies for FixtureDependencies {
    async fn resolve_artifacts(
        &self,
        entry: Model2VecConfig,
        model_cache_dir: &Path,
        reporter: &ModelDownloadProgressReporter,
        signal: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Model2VecResolvedArtifacts, crate::models::spi::ModelError> {
        if signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return Err(crate::models::spi::ModelError::cancelled(
                "fixture download cancelled",
            ));
        }
        let directory = model_cache_dir
            .join("model2vec")
            .join(entry.repo.replace('/', "--"))
            .join(entry.revision);
        let model_path = directory.join(entry.model_file);
        let tokenizer_path = directory.join("tokenizer/tokenizer.json");
        let missing = [
            (!super::is_usable_model_file(&model_path).await)
                .then_some((entry.model_file.to_owned(), 8)),
            (!super::is_usable_model_file(&tokenizer_path).await)
                .then_some((entry.tokenizer_file.to_owned(), 8)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        reporter.set_download_plan(missing.clone());
        for (artifact, total) in missing {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            reporter.report(
                &artifact,
                ArtifactDownloadProgress {
                    downloaded_bytes: 4,
                    total_bytes: Some(total),
                },
            );
            let destination = if artifact == entry.model_file {
                &model_path
            } else {
                &tokenizer_path
            };
            tokio::fs::create_dir_all(destination.parent().expect("fixture parent"))
                .await
                .map_err(|error| crate::models::spi::ModelError::internal(error.to_string()))?;
            tokio::fs::write(destination, b"asset")
                .await
                .map_err(|error| crate::models::spi::ModelError::internal(error.to_string()))?;
        }
        Ok(Model2VecResolvedArtifacts {
            model_path,
            tokenizer_path,
        })
    }

    async fn load_tokenizer(
        &self,
        _source: &Path,
    ) -> Result<Arc<dyn TokenizerRuntime>, crate::models::spi::ModelError> {
        self.tokenizer_loads.fetch_add(1, Ordering::Relaxed);
        Ok(self.tokenizer.clone())
    }

    async fn load_safetensors(
        &self,
        _path: &Path,
        _tensor_name: &str,
        _dimension: usize,
    ) -> Result<StaticEmbeddingTable, crate::models::spi::ModelError> {
        self.table_loads.fetch_add(1, Ordering::Relaxed);
        Ok(StaticEmbeddingTable {
            values: vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 2.0],
            dimension: 3,
            rows: 3,
        })
    }
}
