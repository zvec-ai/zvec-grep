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
        artifacts::{
            ArtifactSource, ModelDownloadProgressReporter, ResolveArtifacts,
            resolve_model_artifacts,
        },
        catalog::Model2VecConfig,
        runtime::ModelComputeRuntime,
        spi::{
            EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions,
            EmbeddingPrepareOptions, ModelError, input_text, validate_inputs, validate_result,
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

    async fn prepare(&self, options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
        self.ensure_loaded(options.on_progress, options.signal.as_ref())
            .await
            .map(|_| ())
            .map_err(ModelError::shared)
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
mod tests;
