use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, PoisonError},
};

use async_trait::async_trait;
use ort::{
    session::{RunOptions, Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use tokenizers::Tokenizer;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::processor::{MAX_PATCHES, PATCH_DIM, image_patches};
use crate::{
    domain::{
        Content,
        model::{
            Device, EmbeddingMetric, EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult,
            ModelConfig, ModelProgress,
        },
    },
    models::{
        artifacts::{
            ArtifactSource, ModelDownloadProgressReporter, ResolveArtifacts,
            resolve_model_artifacts,
        },
        catalog::{EmbeddingCatalogEntry, EmbeddingGemma2Config},
        runtime::ModelComputeRuntime,
        spi::{
            EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions, ModelError, validate_inputs,
            validate_result,
        },
    },
};

const HIDDEN_SIZE: usize = 512;
const IMAGE_TOKEN: u32 = 258_880;

pub(crate) struct EmbeddingGemma2Model {
    entry: EmbeddingGemma2Config,
    info: EmbeddingModelInfo,
    cache_directory: PathBuf,
    device: Option<Device>,
    compute: ModelComputeRuntime,
    client: reqwest::Client,
    state: Mutex<Option<Arc<LoadedModel>>>,
}

struct LoadedModel {
    tokenizer: Tokenizer,
    sessions: StdMutex<Sessions>,
}

struct Sessions {
    text: Session,
    vision: Session,
}

struct PreparedInput {
    tokens: Vec<i64>,
    image_features: Vec<f32>,
    truncated: bool,
}

impl EmbeddingGemma2Model {
    pub(crate) fn new(
        entry: EmbeddingGemma2Config,
        config: ModelConfig,
        compute: ModelComputeRuntime,
    ) -> Result<Self, ModelError> {
        let cache = config
            .cache_dir
            .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(default_cache);
        Ok(Self {
            entry,
            info: EmbeddingModelInfo {
                space: EmbeddingCatalogEntry::EmbeddingGemma2(entry).embedding_space(None),
                retrieval: EmbeddingCatalogEntry::EmbeddingGemma2(entry).retrieval(),
                model: EmbeddingCatalogEntry::EmbeddingGemma2(entry)
                    .model_info()
                    .map_err(|error| {
                        ModelError::internal("Invalid EmbeddingGemma 2 catalog metadata")
                            .with_cause(error)
                            .shared()
                    })?,
                dimension: entry.dimension,
                metric: EmbeddingMetric::Cosine,
                max_batch_size: 1,
                max_input_tokens: Some(entry.max_input_tokens),
                max_image_bytes: Some(entry.max_image_bytes),
            },
            cache_directory: cache.join(entry.source.repo).join(entry.source.revision),
            device: config.device,
            compute,
            client: reqwest::Client::new(),
            state: Mutex::new(None),
        })
    }

    async fn loaded(
        &self,
        progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<Arc<LoadedModel>, ModelError> {
        check_cancelled(signal)?;
        let mut state = tokio::select! {
            state = self.state.lock() => state,
            () = cancelled(signal) => return Err(cancelled_error()),
        };
        if let Some(loaded) = state.as_ref() {
            return Ok(Arc::clone(loaded));
        }
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            progress,
            self.entry
                .artifacts
                .iter()
                .map(|artifact| artifact.path.to_owned()),
        );
        reporter.start();
        let resolved = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: self.entry.reference,
                sources: [ArtifactSource::hugging_face(
                    self.entry.source,
                    self.cache_directory.clone(),
                )],
                artifacts: self.entry.artifacts,
                reporter: &reporter,
                signal,
            },
        )
        .await?;
        let path = |name| {
            resolved.paths.get(name).cloned().ok_or_else(|| {
                ModelError::storage_failure(format!("Missing EmbeddingGemma 2 artifact: {name}"))
                    .shared()
            })
        };
        let tokenizer_path = path("tokenizer.json")?;
        let text_path = path("onnx/model_q4.onnx")?;
        let vision_path = path("onnx/vision_encoder_q4.onnx")?;
        if self
            .device
            .is_some_and(|device| !matches!(device, Device::Cpu | Device::Auto))
        {
            let warning = "EmbeddingGemma 2 currently uses the native ONNX CPU backend; the requested GPU device is not used.";
            if !reporter.warning(warning) {
                tracing::warn!("{warning}");
            }
        }
        let load_signal = signal.cloned();
        let loaded = self
            .compute
            .run(move || {
                check_cancelled(load_signal.as_ref())?;
                let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|error| {
                    ModelError::storage_failure("Unable to load EmbeddingGemma 2 tokenizer")
                        .with_cause(error)
                        .shared()
                })?;
                let text = session(&text_path)?;
                check_cancelled(load_signal.as_ref())?;
                let vision = session(&vision_path)?;
                check_cancelled(load_signal.as_ref())?;
                Ok::<_, ModelError>(LoadedModel {
                    tokenizer,
                    sessions: StdMutex::new(Sessions { text, vision }),
                })
            })
            .await??;
        let loaded = Arc::new(loaded);
        *state = Some(Arc::clone(&loaded));
        reporter.finish();
        Ok(loaded)
    }
}

#[async_trait]
impl EmbeddingModel for EmbeddingGemma2Model {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn prepare(&self, options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
        self.loaded(options.on_progress, options.signal.as_ref())
            .await
            .map(|_| ())
            .map_err(ModelError::shared)
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
        check_cancelled(options.signal.as_ref())?;
        let loaded = self
            .loaded(options.on_progress, options.signal.as_ref())
            .await?;
        let run_options = Arc::new(RunOptions::new().map_err(inference_error)?);
        let cancellation = RunCancellation::new(options.signal.clone(), Arc::clone(&run_options));
        let inputs = inputs.to_vec();
        let input_count = inputs.len();
        let entry = self.entry;
        let result = self
            .compute
            .run(move || {
                let mut sessions = loaded
                    .sessions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut result = EmbeddingResult {
                    vectors: Vec::with_capacity(inputs.len()),
                    truncated: Vec::new(),
                };
                for (index, input) in inputs.iter().enumerate() {
                    check_cancelled(options.signal.as_ref())?;
                    let prepared = prepare_input(
                        input,
                        (options.purpose, options.query_target),
                        &loaded.tokenizer,
                        &mut sessions.vision,
                        &run_options,
                        entry.max_input_tokens,
                        options.signal.as_ref(),
                    )?;
                    let vector =
                        infer(&mut sessions.text, &prepared, &run_options, entry.dimension)?;
                    check_cancelled(options.signal.as_ref())?;
                    if prepared.truncated {
                        result.truncated.push(index);
                    }
                    result.vectors.push(vector);
                }
                Ok::<_, ModelError>(result)
            })
            .await?;
        let was_cancelled = cancellation.is_cancelled();
        drop(cancellation);
        if was_cancelled {
            return Err(cancelled_error());
        }
        let result = result?;
        validate_result(&self.info, input_count, &result)?;
        Ok(result)
    }
}

fn session(path: &Path) -> Result<Session, ModelError> {
    // A runtime lease admits one batch at a time. Two sessions share that batch;
    // neither creates an additional inference scheduler.
    Session::builder()
        .map_err(inference_error)?
        .with_intra_threads(
            std::thread::available_parallelism().map_or(1, |count| count.get().min(8)),
        )
        .map_err(inference_error)?
        .with_inter_threads(1)
        .map_err(inference_error)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(inference_error)?
        .commit_from_file(path)
        .map_err(|error| {
            ModelError::storage_failure("Unable to load EmbeddingGemma 2 ONNX model")
                .with_cause(error)
                .shared()
        })
}

fn prepare_input(
    input: &[Content],
    (purpose, query_target): (EmbeddingPurpose, Option<crate::domain::ContentKind>),
    tokenizer: &Tokenizer,
    vision: &mut Session,
    run_options: &RunOptions,
    max_tokens: usize,
    signal: Option<&CancellationToken>,
) -> Result<PreparedInput, ModelError> {
    let mut text = String::new();
    let mut features = Vec::new();
    for content in input {
        check_cancelled(signal)?;
        if !text.is_empty() {
            text.push('\n');
        }
        match content {
            Content::Text(value) | Content::Code(value) => text.push_str(&format_text(
                value,
                purpose,
                query_target.map_or_else(
                    || matches!(content, Content::Code(_)),
                    |kind| kind == crate::domain::ContentKind::Code,
                ),
            )),
            Content::Image(image) => {
                let patches = image_patches(image.data())?;
                if features.len() / HIDDEN_SIZE + patches.soft_tokens + 4 > max_tokens {
                    return Err(ModelError::invalid_argument(
                        "EmbeddingGemma 2 image input exceeds its token budget",
                    ));
                }
                let outputs = vision.run_with_options(ort::inputs! {
                    "pixel_values" => Tensor::from_array(([1, MAX_PATCHES, PATCH_DIM], patches.pixels)).map_err(inference_error)?,
                    "pixel_position_ids" => Tensor::from_array(([1, MAX_PATCHES, 2], patches.positions)).map_err(inference_error)?,
                }, run_options).map_err(inference_error)?;
                let (shape, data) = outputs
                    .get("image_features")
                    .ok_or_else(|| {
                        ModelError::internal("EmbeddingGemma 2 returned no image features")
                    })?
                    .try_extract_tensor::<f32>()
                    .map_err(inference_error)?;
                if shape.as_ref()
                    != [
                        i64::try_from(patches.soft_tokens).expect("bounded token count"),
                        512,
                    ]
                {
                    return Err(ModelError::internal(
                        "EmbeddingGemma 2 returned an unexpected vision shape",
                    )
                    .shared());
                }
                features.extend_from_slice(data);
                text.push_str(&image_placeholder(patches.soft_tokens));
            }
        }
    }
    let encoding = tokenizer.encode(text, true).map_err(|error| {
        ModelError::invalid_argument("EmbeddingGemma 2 tokenization failed").with_cause(error)
    })?;
    let image_tokens = encoding
        .get_ids()
        .iter()
        .filter(|&&id| id == IMAGE_TOKEN)
        .count();
    if image_tokens * HIDDEN_SIZE != features.len()
        || encoding
            .get_ids()
            .iter()
            .any(|&id| id == 258_881 || id == 258_884)
    {
        return Err(ModelError::invalid_argument(
            "EmbeddingGemma 2 text contains unbound media tokens",
        ));
    }
    let truncated = encoding.len() > max_tokens;
    if truncated && !features.is_empty() {
        return Err(ModelError::invalid_argument(
            "EmbeddingGemma 2 fused input exceeds its token budget",
        ));
    }
    let mut tokens = encoding
        .get_ids()
        .iter()
        .take(max_tokens)
        .map(|&id| i64::from(id))
        .collect::<Vec<_>>();
    if truncated && let Some(last) = tokens.last_mut() {
        *last = 1;
    }
    Ok(PreparedInput {
        tokens,
        image_features: features,
        truncated,
    })
}

pub(super) fn format_text(text: &str, purpose: EmbeddingPurpose, code: bool) -> String {
    match purpose {
        EmbeddingPurpose::Document => {
            format!("{}{text}", crate::models::catalog::GEMMA_DOCUMENT_PREFIX)
        }
        EmbeddingPurpose::Query if code => {
            format!("{}{text}", crate::models::catalog::GEMMA_CODE_QUERY_PREFIX)
        }
        EmbeddingPurpose::Query => format!("{}{text}", crate::models::catalog::GEMMA_QUERY_PREFIX),
    }
}

pub(super) fn image_placeholder(tokens: usize) -> String {
    format!("<|image>{}<image|>", "<|image|>".repeat(tokens))
}

fn infer(
    session: &mut Session,
    input: &PreparedInput,
    options: &RunOptions,
    dimension: usize,
) -> Result<Vec<f32>, ModelError> {
    let sequence = input.tokens.len();
    let outputs = session.run_with_options(ort::inputs! {
        "input_ids" => Tensor::from_array(([1, sequence], input.tokens.clone())).map_err(inference_error)?,
        "attention_mask" => Tensor::from_array(([1, sequence], vec![1_i64; sequence])).map_err(inference_error)?,
        "image_features" => Tensor::from_array(([input.image_features.len() / HIDDEN_SIZE, HIDDEN_SIZE], input.image_features.clone())).map_err(inference_error)?,
        "video_features" => Tensor::from_array(([0, HIDDEN_SIZE], Vec::<f32>::new())).map_err(inference_error)?,
        "audio_features" => Tensor::from_array(([0, HIDDEN_SIZE], Vec::<f32>::new())).map_err(inference_error)?,
    }, options).map_err(inference_error)?;
    // The exported graph includes 512→768 projection, masked mean pooling and
    // L2 normalization. last_hidden_state is NOT the sentence embedding.
    let (shape, values) = outputs
        .get("sentence_embedding")
        .ok_or_else(|| ModelError::internal("EmbeddingGemma 2 returned no sentence embedding"))?
        .try_extract_tensor::<f32>()
        .map_err(inference_error)?;
    if shape.as_ref()
        != [
            1,
            i64::try_from(dimension).expect("catalog dimension fits i64"),
        ]
    {
        return Err(ModelError::internal(
            "EmbeddingGemma 2 returned an unexpected embedding shape",
        )
        .shared());
    }
    Ok(values.to_vec())
}

struct RunCancellation {
    signal: Option<CancellationToken>,
    options: Arc<RunOptions>,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

impl RunCancellation {
    fn new(signal: Option<CancellationToken>, options: Arc<RunOptions>) -> Self {
        let watched_options = Arc::clone(&options);
        let watcher = signal.clone().map(|signal| {
            tokio::spawn(async move {
                signal.cancelled().await;
                let _result = watched_options.terminate();
            })
        });
        Self {
            signal,
            options,
            watcher,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

impl Drop for RunCancellation {
    fn drop(&mut self) {
        // A caller may drop the embedding future without cancelling its token.
        // Stop the native run as well as its asynchronous cancellation watcher.
        let _result = self.options.terminate();
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

async fn cancelled(signal: Option<&CancellationToken>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending().await,
    }
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

fn cancelled_error() -> ModelError {
    ModelError::cancelled("EmbeddingGemma 2 inference was cancelled")
}

fn inference_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::internal("EmbeddingGemma 2 ONNX inference failed").with_cause(error)
}

fn default_cache() -> PathBuf {
    env::var_os("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".zvec-grep"))
        })
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

#[cfg(test)]
#[path = "cancellation_tests.rs"]
mod cancellation_tests;
