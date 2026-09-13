use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use tokenizers::Tokenizer;
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::models::{
    catalog::Model2VecConfig,
    compute::ModelComputeRuntime,
    download_progress::{ArtifactDownloadProgress, ModelDownloadProgressReporter},
    spi::{
        CreateEmbeddingModelOptions, EmbeddingInput, EmbeddingInputKind, EmbeddingModel,
        EmbeddingModelInfo, EmbeddingModelLimits, EmbeddingModelProgress, EmbeddingOptions,
        EmbeddingPurpose, EmbeddingResult, ModelError, validate_inputs, validate_result,
    },
};

use super::safetensors::{StaticEmbeddingTable, load_static_embedding_table};

static PARTIAL_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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
    pub(crate) fn new(entry: Model2VecConfig, options: CreateEmbeddingModelOptions) -> Self {
        Self::with_dependencies(
            entry,
            options,
            Arc::new(DefaultModel2VecDependencies::new()),
        )
    }

    fn with_dependencies(
        entry: Model2VecConfig,
        options: CreateEmbeddingModelOptions,
        dependencies: Arc<dyn Model2VecDependencies>,
    ) -> Self {
        let model_cache_dir = options
            .model_cache_dir
            .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(default_model_cache_dir);
        let compute_runtime = options.compute_runtime.unwrap_or_default();
        Self {
            entry,
            info: EmbeddingModelInfo {
                reference: entry.reference.to_owned(),
                provider: entry.provider.to_owned(),
                name: entry.model.to_owned(),
                dimension: entry.dimension,
                metric: entry.metric,
                endpoint: None,
                default_concurrency: Some(entry.default_concurrency),
                input_kinds: vec![EmbeddingInputKind::Text],
                limits: EmbeddingModelLimits {
                    max_batch_size: entry.max_batch_size,
                    max_input_tokens: Some(entry.max_input_tokens),
                    max_image_bytes: None,
                },
            },
            model_cache_dir,
            compute_runtime,
            dependencies,
            state: Mutex::new(ModelState::default()),
        }
    }

    async fn ensure_loaded(
        &self,
        on_progress: Option<Arc<dyn Fn(EmbeddingModelProgress) + Send + Sync>>,
    ) -> Result<Arc<LoadedModel>, ModelError> {
        let mut state = self.state.lock().await;
        if let Some(loaded) = &state.loaded {
            return Ok(Arc::clone(loaded));
        }
        let loaded = Arc::new(self.load_model(on_progress).await?);
        state.loaded = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    async fn load_model(
        &self,
        on_progress: Option<Arc<dyn Fn(EmbeddingModelProgress) + Send + Sync>>,
    ) -> Result<LoadedModel, ModelError> {
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            on_progress,
            [
                file_name(self.entry.model_file)?.to_owned(),
                file_name(self.entry.tokenizer_file)?.to_owned(),
            ],
        );
        reporter.start();
        self.exclude_cached_artifacts_from_progress(&reporter)
            .await?;
        let (model_path, tokenizer_source) = tokio::join!(
            self.resolve_model_path(&reporter),
            self.resolve_tokenizer_source(&reporter)
        );
        let model_path = model_path?;
        let tokenizer_source = tokenizer_source?;
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

    async fn exclude_cached_artifacts_from_progress(
        &self,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<(), ModelError> {
        let model_artifact = file_name(self.entry.model_file)?;
        if is_usable_model_file(&self.model_directory().join(model_artifact)).await {
            reporter.skip(model_artifact);
        }

        let tokenizer_artifact = file_name(self.entry.tokenizer_file)?;
        let tokenizer_path = self
            .model_directory()
            .join("tokenizer")
            .join("tokenizer.json");
        if is_usable_model_file(&tokenizer_path).await {
            reporter.skip(tokenizer_artifact);
        }
        Ok(())
    }

    async fn resolve_model_path(
        &self,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<PathBuf, ModelError> {
        let model_path = self
            .model_directory()
            .join(file_name(self.entry.model_file)?);
        self.resolve_cached_file(self.entry.model_file, &model_path, reporter)
            .await
    }

    async fn resolve_tokenizer_source(
        &self,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<PathBuf, ModelError> {
        let tokenizer_directory = self.model_directory().join("tokenizer");
        self.resolve_cached_file(
            self.entry.tokenizer_file,
            &tokenizer_directory.join("tokenizer.json"),
            reporter,
        )
        .await?;
        let config_path = tokenizer_directory.join("tokenizer_config.json");
        if !is_usable_model_file(&config_path).await {
            fs::write(
                &config_path,
                b"{\"tokenizer_class\":\"PreTrainedTokenizer\"}\n",
            )
            .await
            .map_err(|error| {
                ModelError::storage_failure(format!("Unable to write tokenizer config: {error}"))
            })?;
        }
        Ok(tokenizer_directory)
    }

    async fn resolve_cached_file(
        &self,
        remote_file: &str,
        local_path: &Path,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<PathBuf, ModelError> {
        let artifact = file_name(remote_file)?;
        if is_usable_model_file(local_path).await {
            reporter.skip(artifact);
            return Ok(local_path.to_path_buf());
        }
        let parent = local_path.parent().ok_or_else(|| {
            ModelError::storage_failure("Model2Vec cache path does not have a parent directory")
        })?;
        fs::create_dir_all(parent).await.map_err(|error| {
            ModelError::storage_failure(format!(
                "Unable to create Model2Vec cache directory: {error}"
            ))
        })?;

        let partial_path = partial_path(local_path);
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.entry.repo, self.entry.revision, remote_file
        );
        let progress_reporter = reporter.clone();
        let artifact_name = artifact.to_owned();
        let progress = Arc::new(move |event| {
            progress_reporter.report(&artifact_name, event);
        });
        let result = self
            .dependencies
            .download(&url, &partial_path, progress)
            .await;
        let result = match result {
            Ok(()) if is_usable_model_file(&partial_path).await => {
                fs::rename(&partial_path, local_path)
                    .await
                    .map_err(|error| {
                        ModelError::storage_failure(format!(
                            "Unable to publish Model2Vec artifact: {error}"
                        ))
                    })
            }
            Ok(()) => Err(ModelError::storage_failure(
                "Downloaded model file is empty",
            )),
            Err(error) => Err(error),
        };
        if let Err(cause) = result {
            let _ = fs::remove_file(&partial_path).await;
            return Err(ModelError::new(
                crate::EngineError::STORAGE_FAILURE,
                "Unable to download Model2Vec model artifact",
                Some(format!("model={} url={url}", self.entry.reference)),
            )
            .with_cause(cause));
        }
        Ok(local_path.to_path_buf())
    }

    fn model_directory(&self) -> PathBuf {
        self.model_cache_dir
            .join("model2vec")
            .join(self.entry.repo.replace('/', "--"))
            .join(self.entry.revision)
    }
}

#[async_trait]
impl EmbeddingModel for Model2VecEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn embed(
        &self,
        inputs: &[EmbeddingInput],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
        let loaded = self.ensure_loaded(options.on_progress).await?;
        let purpose = options.purpose.unwrap_or_default();
        let prefix = match purpose {
            EmbeddingPurpose::Document => self.entry.document_prefix,
            EmbeddingPurpose::Query => self.entry.query_prefix,
        };
        let texts = inputs
            .iter()
            .map(|input| {
                let text = input.to_text()?;
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
    async fn load_tokenizer(&self, source: &Path) -> Result<Arc<dyn TokenizerRuntime>, ModelError>;

    async fn load_safetensors(
        &self,
        path: &Path,
        tensor_name: &str,
        dimension: usize,
    ) -> Result<StaticEmbeddingTable, ModelError>;

    async fn download(
        &self,
        url: &str,
        destination: &Path,
        on_progress: Arc<dyn Fn(ArtifactDownloadProgress) + Send + Sync>,
    ) -> Result<(), ModelError>;
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

    async fn download(
        &self,
        url: &str,
        destination: &Path,
        on_progress: Arc<dyn Fn(ArtifactDownloadProgress) + Send + Sync>,
    ) -> Result<(), ModelError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| ModelError::storage_failure(error.to_string()))?;
        if !response.status().is_success() {
            return Err(ModelError::storage_failure(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let total_bytes = response.content_length();
        let mut file = fs::File::create(destination).await.map_err(|error| {
            ModelError::storage_failure(format!("Unable to create partial model artifact: {error}"))
        })?;
        let mut downloaded_bytes = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ModelError::storage_failure(error.to_string()))?;
            file.write_all(&chunk).await.map_err(|error| {
                ModelError::storage_failure(format!("Unable to write model artifact: {error}"))
            })?;
            downloaded_bytes = downloaded_bytes.saturating_add(chunk.len() as u64);
            on_progress(ArtifactDownloadProgress {
                downloaded_bytes,
                total_bytes,
            });
        }
        file.flush().await.map_err(|error| {
            ModelError::storage_failure(format!("Unable to flush model artifact: {error}"))
        })?;
        Ok(())
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

fn partial_path(path: &Path) -> PathBuf {
    let sequence = PARTIAL_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut partial = OsString::from(path.as_os_str());
    partial.push(format!(".part-{}-{sequence}", std::process::id()));
    PathBuf::from(partial)
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

    use crate::models::spi::{EmbeddingInput, EmbeddingMetric};
    use crate::models::{
        catalog::Model2VecConfig,
        spi::{
            CreateEmbeddingModelOptions, EmbeddingModel, EmbeddingModelProgress, EmbeddingOptions,
            EmbeddingPurpose,
        },
    };

    use super::{
        ArtifactDownloadProgress, Model2VecDependencies, Model2VecEmbeddingModel,
        StaticEmbeddingTable, TokenizerRuntime,
    };

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn matches_typescript_model2vec_oracle_and_reuses_loaded_assets() {
        let root = TempDir::new().expect("temporary directory should be created");
        let dependencies = Arc::new(FixtureDependencies::new(FixtureTokenizerMode::Oracle));
        let model = Model2VecEmbeddingModel::with_dependencies(
            fixture_entry(),
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies.clone(),
        );
        assert_eq!(model.info().default_concurrency, Some(2));

        let progress = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&progress);
        let result = model
            .embed(
                &[
                    EmbeddingInput::text("both tokens".to_owned()),
                    EmbeddingInput::text("unknown-only".to_owned()),
                    EmbeddingInput::text("third token".to_owned()),
                ],
                EmbeddingOptions {
                    purpose: Some(EmbeddingPurpose::Query),
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
                EmbeddingModelProgress::Preparing {
                    model: "local/test-potion".to_owned(),
                },
                EmbeddingModelProgress::Downloading {
                    model: "local/test-potion".to_owned(),
                    downloaded_bytes: Some(4),
                    total_bytes: None,
                },
                EmbeddingModelProgress::Downloading {
                    model: "local/test-potion".to_owned(),
                    downloaded_bytes: Some(8),
                    total_bytes: Some(16),
                },
                EmbeddingModelProgress::Ready {
                    model: "local/test-potion".to_owned(),
                },
            ]
        );
        assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
        assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
        assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);

        model
            .embed(
                &[EmbeddingInput::text("cached".to_owned())],
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
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies.clone(),
        );
        let first = [EmbeddingInput::text("first".to_owned())];
        let second = [EmbeddingInput::text("second".to_owned())];

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
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies,
        );
        let result = model
            .embed(
                &[EmbeddingInput::text("too many tokens".to_owned())],
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
                &[EmbeddingInput::text("  ".to_owned())],
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
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies.clone(),
        );
        let progress = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&progress);

        model
            .embed(
                &[EmbeddingInput::text("cached tokenizer".to_owned())],
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
                EmbeddingModelProgress::Preparing {
                    model: "local/test-potion".to_owned(),
                },
                EmbeddingModelProgress::Downloading {
                    model: "local/test-potion".to_owned(),
                    downloaded_bytes: Some(4),
                    total_bytes: Some(8),
                },
                EmbeddingModelProgress::Ready {
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
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies,
        );

        let error = model
            .embed(
                &[EmbeddingInput::text("invalid token".to_owned())],
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
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(root.path().to_path_buf()),
                ..CreateEmbeddingModelOptions::default()
            },
            dependencies.clone(),
        );
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();

        let error = model
            .embed(
                &[EmbeddingInput::text("cancelled".to_owned())],
                EmbeddingOptions {
                    signal: Some(signal),
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect_err("cancelled embedding should fail");

        assert_eq!(error.code(), crate::EngineError::CANCELLED);
        assert!(
            error
                .cause()
                .is_some_and(|cause| cause.contains("cancelled"))
        );
        model
            .embed(
                &[EmbeddingInput::text("after cancellation".to_owned())],
                EmbeddingOptions::default(),
            )
            .await
            .expect("cancellation should not poison the shared runtime");
        assert_eq!(dependencies.downloads.load(Ordering::Relaxed), 2);
        assert_eq!(dependencies.tokenizer_loads.load(Ordering::Relaxed), 1);
        assert_eq!(dependencies.table_loads.load(Ordering::Relaxed), 1);
    }

    fn fixture_entry() -> Model2VecConfig {
        Model2VecConfig {
            reference: "local/test-potion",
            provider: "local",
            model: "test-potion",
            repo: "test/potion",
            revision: "0123456789abcdef",
            model_file: "model.safetensors",
            embedding_tensor: "embeddings",
            tokenizer_file: "tokenizer.json",
            dimension: 3,
            metric: EmbeddingMetric::Cosine,
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

        async fn download(
            &self,
            _url: &str,
            destination: &Path,
            on_progress: Arc<dyn Fn(ArtifactDownloadProgress) + Send + Sync>,
        ) -> Result<(), crate::models::spi::ModelError> {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            on_progress(ArtifactDownloadProgress {
                downloaded_bytes: 4,
                total_bytes: Some(8),
            });
            tokio::fs::write(destination, b"asset")
                .await
                .map_err(|error| crate::models::spi::ModelError::internal(error.to_string()))
        }
    }
}
