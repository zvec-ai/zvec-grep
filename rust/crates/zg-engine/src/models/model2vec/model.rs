use crate::domain::Content;
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(windows)]
use std::ffi::OsString;

use async_trait::async_trait;
use serde_json::Value;
use tokenizers::Tokenizer;
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::utils::atomic_write;
use crate::{
    domain::model::{
        EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult, ModelConfig, ModelInfo,
        ModelProgress,
    },
    models::{
        artifact_downloader::{ArtifactSource, ResolveArtifacts, resolve_model_artifacts},
        catalog::Model2VecConfig,
        compute::ModelComputeRuntime,
        download_progress::ModelDownloadProgressReporter,
        spi::{
            EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions, ModelError, input_text,
            validate_inputs, validate_result,
        },
    },
};

use super::safetensors::{StaticEmbeddingTable, load_static_embedding_table};

pub(crate) struct Model2VecEmbeddingModel {
    entry: Model2VecConfig,
    info: EmbeddingModelInfo,
    model_cache_dir: PathBuf,
    compute_runtime: ModelComputeRuntime,
    dependencies: Arc<dyn Model2VecDependencies>,
    state: Mutex<ModelState>,
}

#[derive(Default)]
struct ModelState {
    loaded: Option<Arc<LoadedModel>>,
}

struct LoadedModel {
    tokenizer: Arc<dyn TokenizerRuntime>,
    table: StaticEmbeddingTable,
}

impl Model2VecEmbeddingModel {
    pub(crate) fn new(
        entry: Model2VecConfig,
        options: ModelConfig,
        compute_runtime: ModelComputeRuntime,
    ) -> Self {
        Self::with_dependencies(
            entry,
            options,
            Arc::new(DefaultModel2VecDependencies::new()),
            compute_runtime,
        )
    }

    fn with_dependencies(
        entry: Model2VecConfig,
        options: ModelConfig,
        dependencies: Arc<dyn Model2VecDependencies>,
        compute_runtime: ModelComputeRuntime,
    ) -> Self {
        let model_cache_dir = options
            .cache_dir
            .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(default_model_cache_dir);
        Self {
            entry,
            info: EmbeddingModelInfo {
                model: ModelInfo {
                    provider: entry.provider.to_owned(),
                    name: entry.model.to_owned(),
                    endpoint: None,
                },
                dimension: entry.dimension,
                metric: entry.metric,
                max_batch_size: entry.max_batch_size,
                max_input_tokens: Some(entry.max_input_tokens),
                max_image_bytes: None,
            },
            model_cache_dir,
            compute_runtime,
            dependencies,
            state: Mutex::new(ModelState::default()),
        }
    }

    async fn ensure_loaded(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<Arc<LoadedModel>, ModelError> {
        let mut state = self.state.lock().await;
        if let Some(loaded) = &state.loaded {
            return Ok(Arc::clone(loaded));
        }
        let loaded = Arc::new(self.load_model(on_progress, signal).await?);
        state.loaded = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    async fn load_model(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<LoadedModel, ModelError> {
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            on_progress,
            self.entry
                .download
                .artifacts
                .iter()
                .map(|artifact| artifact.path.to_owned()),
        );
        reporter.start();
        let resolved = self
            .dependencies
            .resolve_artifacts(self.entry, &self.model_cache_dir, &reporter, signal)
            .await?;
        let model_path = resolved.model_path;
        let tokenizer_source = resolved
            .tokenizer_path
            .parent()
            .ok_or_else(|| ModelError::storage_failure("Tokenizer path has no parent"))?
            .to_path_buf();
        let config_path = tokenizer_source.join("tokenizer_config.json");
        if !is_usable_model_file(&config_path).await {
            tokio::task::spawn_blocking(move || {
                atomic_write(
                    &config_path,
                    b"{\"tokenizer_class\":\"PreTrainedTokenizer\"}\n",
                )
            })
            .await
            .map_err(|error| {
                ModelError::storage_failure("Unable to complete tokenizer config write")
                    .with_cause(error)
            })?
            .map_err(|error| {
                ModelError::storage_failure("Unable to write tokenizer config").with_cause(error)
            })?;
        }
        let table = self
            .dependencies
            .load_safetensors(
                &model_path,
                self.entry.embedding_tensor,
                self.entry.dimension,
            )
            .await?;
        let tokenizer = self.dependencies.load_tokenizer(&tokenizer_source).await?;
        reporter.finish();
        Ok(LoadedModel { tokenizer, table })
    }
}

#[async_trait]
impl EmbeddingModel for Model2VecEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        EmbeddingConcurrencyDefaults {
            initial: self.entry.default_concurrency.max(1),
            maximum: self.entry.default_concurrency.max(1),
        }
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs, |content| {
            matches!(content, Content::Text(_))
        })?;
        let loaded = self
            .ensure_loaded(options.on_progress, options.signal.as_ref())
            .await?;
        let purpose = options.purpose;
        let prefix = match purpose {
            EmbeddingPurpose::Document => self.entry.document_prefix,
            EmbeddingPurpose::Query => self.entry.query_prefix,
        };
        let texts = inputs
            .iter()
            .map(|input| {
                let text = input_text(input)?;
                Ok(match prefix {
                    Some(prefix) => format!("{prefix}{text}"),
                    None => text.into_owned(),
                })
            })
            .collect::<Result<Vec<_>, ModelError>>()?;
        let signal = options.signal;
        let entry = self.entry;
        let computation = self
            .compute_runtime
            .run(move || {
                embed_model2vec_texts(
                    &texts,
                    loaded.tokenizer.as_ref(),
                    &loaded.table,
                    entry.max_input_tokens,
                    entry.normalize,
                    signal.as_ref(),
                )
            })
            .await?;
        let result = computation.map_err(|cause| {
            cause.wrap(
                "Model2Vec embedding failed",
                Some(format!(
                    "model={} repo={}",
                    self.entry.reference, self.entry.repo
                )),
            )
        })?;
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

#[async_trait]
trait Model2VecDependencies: Send + Sync {
    async fn resolve_artifacts(
        &self,
        entry: Model2VecConfig,
        model_cache_dir: &Path,
        reporter: &ModelDownloadProgressReporter,
        signal: Option<&CancellationToken>,
    ) -> Result<Model2VecResolvedArtifacts, ModelError>;

    async fn load_tokenizer(&self, source: &Path) -> Result<Arc<dyn TokenizerRuntime>, ModelError>;

    async fn load_safetensors(
        &self,
        path: &Path,
        tensor_name: &str,
        dimension: usize,
    ) -> Result<StaticEmbeddingTable, ModelError>;
}

struct Model2VecResolvedArtifacts {
    model_path: PathBuf,
    tokenizer_path: PathBuf,
}

trait TokenizerRuntime: Send + Sync {
    fn encode(&self, text: &str) -> Result<Vec<u32>, ModelError>;
    fn unknown_token_id(&self) -> Option<u32>;
}

struct DefaultModel2VecDependencies {
    client: reqwest::Client,
}

impl DefaultModel2VecDependencies {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Model2VecDependencies for DefaultModel2VecDependencies {
    async fn resolve_artifacts(
        &self,
        entry: Model2VecConfig,
        model_cache_dir: &Path,
        reporter: &ModelDownloadProgressReporter,
        signal: Option<&CancellationToken>,
    ) -> Result<Model2VecResolvedArtifacts, ModelError> {
        let local_model = file_name(entry.model_file)?;
        let hugging_face = model_cache_dir
            .join("model2vec")
            .join(entry.download.hugging_face.repo.replace('/', "--"))
            .join(entry.download.hugging_face.revision);
        let model_scope = model_cache_dir
            .join("modelscope")
            .join("model2vec")
            .join(entry.download.model_scope.repo.replace('/', "--"))
            .join(entry.download.model_scope.revision);
        let sources = [
            ArtifactSource::hugging_face(entry.download.hugging_face, hugging_face)
                .with_local_path(entry.model_file, local_model)
                .with_local_path(entry.tokenizer_file, "tokenizer/tokenizer.json"),
            ArtifactSource::model_scope(entry.download.model_scope, model_scope)
                .with_local_path(entry.model_file, local_model)
                .with_local_path(entry.tokenizer_file, "tokenizer/tokenizer.json"),
        ];
        let resolved = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: entry.reference,
                sources,
                artifacts: entry.download.artifacts,
                reporter,
                signal,
            },
        )
        .await?;
        Ok(Model2VecResolvedArtifacts {
            model_path: resolved
                .paths
                .get(entry.model_file)
                .cloned()
                .ok_or_else(|| {
                    ModelError::storage_failure("Resolved Model2Vec model artifact is missing")
                })?,
            tokenizer_path: resolved
                .paths
                .get(entry.tokenizer_file)
                .cloned()
                .ok_or_else(|| {
                    ModelError::storage_failure("Resolved Model2Vec tokenizer artifact is missing")
                })?,
        })
    }

    async fn load_tokenizer(&self, source: &Path) -> Result<Arc<dyn TokenizerRuntime>, ModelError> {
        let tokenizer_path = source.join("tokenizer.json");
        let tokenizer_json = fs::read(&tokenizer_path).await.map_err(|error| {
            ModelError::storage_failure(format!("Unable to read Model2Vec tokenizer: {error}"))
        })?;
        let tokenizer = Tokenizer::from_bytes(&tokenizer_json).map_err(|error| {
            ModelError::storage_failure("Unable to load Model2Vec tokenizer").with_cause(error)
        })?;
        let unknown_token_id = resolve_unknown_token_id(&tokenizer, &tokenizer_json);
        Ok(Arc::new(HuggingFaceTokenizer {
            tokenizer,
            unknown_token_id,
        }))
    }

    async fn load_safetensors(
        &self,
        path: &Path,
        tensor_name: &str,
        dimension: usize,
    ) -> Result<StaticEmbeddingTable, ModelError> {
        load_static_embedding_table(path, tensor_name, dimension).await
    }
}

struct HuggingFaceTokenizer {
    tokenizer: Tokenizer,
    unknown_token_id: Option<u32>,
}

impl TokenizerRuntime for HuggingFaceTokenizer {
    fn encode(&self, text: &str) -> Result<Vec<u32>, ModelError> {
        self.tokenizer
            .encode(text, false)
            .map(|encoding| encoding.get_ids().to_vec())
            .map_err(|error| {
                ModelError::internal("Model2Vec tokenization failed").with_cause(error)
            })
    }

    fn unknown_token_id(&self) -> Option<u32> {
        self.unknown_token_id
    }
}

fn embed_model2vec_texts(
    texts: &[String],
    tokenizer: &dyn TokenizerRuntime,
    table: &StaticEmbeddingTable,
    max_input_tokens: usize,
    normalize: bool,
    signal: Option<&CancellationToken>,
) -> Result<EmbeddingResult, ModelError> {
    let mut vectors = Vec::with_capacity(texts.len());
    let mut truncated = Vec::new();
    let unknown_token_id = tokenizer.unknown_token_id();
    for (index, text) in texts.iter().enumerate() {
        check_cancelled(signal)?;
        let encoded = tokenizer.encode(text)?;
        if encoded.len() > max_input_tokens {
            truncated.push(index);
        }
        let token_ids = encoded
            .into_iter()
            .take(max_input_tokens)
            .filter(|token_id| Some(*token_id) != unknown_token_id)
            .collect::<Vec<_>>();
        vectors.push(embed_static_token_list(&token_ids, table, normalize)?);
    }
    check_cancelled(signal)?;
    Ok(EmbeddingResult { vectors, truncated })
}

fn embed_static_token_list(
    token_ids: &[u32],
    table: &StaticEmbeddingTable,
    normalize: bool,
) -> Result<Vec<f32>, ModelError> {
    let mut vector = vec![0.0_f64; table.dimension];
    if token_ids.is_empty() {
        return Ok(vec![0.0; table.dimension]);
    }
    for &token_id in token_ids {
        let row = usize::try_from(token_id)
            .map_err(|_| out_of_range_token_error(token_id, table.rows))?;
        if row >= table.rows {
            return Err(out_of_range_token_error(token_id, table.rows));
        }
        let start = row
            .checked_mul(table.dimension)
            .ok_or_else(|| ModelError::internal("Static embedding table offset overflow"))?;
        for (column, value) in vector.iter_mut().enumerate() {
            *value += f64::from(table.values[start + column]);
        }
    }
    let divisor = f64::from(
        u32::try_from(token_ids.len())
            .map_err(|_| ModelError::internal("Model2Vec token count exceeds u32"))?,
    );
    let mut squared_norm = 0.0_f64;
    for value in &mut vector {
        *value /= divisor;
        squared_norm += *value * *value;
    }
    if normalize && squared_norm > 0.0 {
        let inverse_norm = squared_norm.sqrt().recip();
        for value in &mut vector {
            *value *= inverse_norm;
        }
    }
    Ok(vector.into_iter().map(js_number_to_float32).collect())
}

// The TypeScript worker serializes its double-precision calculation through a
// Float32Array before returning it. This narrowing is required for parity.
#[allow(clippy::cast_possible_truncation)]
fn js_number_to_float32(value: f64) -> f32 {
    value as f32
}

fn out_of_range_token_error(token_id: u32, rows: usize) -> ModelError {
    ModelError::internal(format!(
        "Tokenizer returned out-of-range token id: id={token_id} rows={rows}"
    ))
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(ModelError::cancelled("Model2Vec embedding was cancelled"));
    }
    Ok(())
}

fn resolve_unknown_token_id(tokenizer: &Tokenizer, bytes: &[u8]) -> Option<u32> {
    let json: Value = serde_json::from_slice(bytes).ok()?;
    let model = json.get("model")?;
    if let Some(id) = model
        .get("unk_id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
    {
        return Some(id);
    }
    model
        .get("unk_token")
        .and_then(Value::as_str)
        .and_then(|token| tokenizer.token_to_id(token))
}

async fn is_usable_model_file(path: &Path) -> bool {
    fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn file_name(path: &str) -> Result<&str, ModelError> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ModelError::storage_failure(format!("Model artifact has no file name: {path}"))
        })
}

fn default_model_cache_dir() -> PathBuf {
    env::var_os("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(|| user_home_dir().map(|home| home.join(".zvec-grep")))
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

#[cfg(windows)]
fn user_home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE").map(PathBuf::from).or_else(|| {
        let drive = env::var_os("HOMEDRIVE")?;
        let path = env::var_os("HOMEPATH")?;
        let mut home = OsString::from(drive);
        home.push(path);
        Some(PathBuf::from(home))
    })
}

#[cfg(not(windows))]
fn user_home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
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
            catalog::Model2VecConfig,
            download_progress::ArtifactDownloadProgress,
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
            crate::models::compute::ModelComputeRuntime::shared(),
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
}
