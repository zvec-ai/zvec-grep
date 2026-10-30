use crate::domain::Content;
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, PoisonError},
};

use async_trait::async_trait;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::{DynTensor, Tensor},
};
use tokenizers::{
    PaddingParams, Tokenizer, TruncationParams,
    utils::{padding::PaddingDirection, truncation::TruncationDirection},
};
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::domain::model::{
    Device, EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult, ModelConfig, ModelInfo,
    ModelProgress,
};
use crate::models::{
    artifacts::{
        ArtifactSource, LocalModelBackend, ModelDownloadProgressReporter, ResolveArtifacts,
        default_model_cache, resolve_model_artifacts,
    },
    catalog::TransformersConfig,
    runtime::ModelComputeRuntime,
    spi::{
        EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions, ModelError, input_text,
        validate_inputs, validate_result,
    },
};

pub(crate) struct TransformersEmbeddingModel {
    entry: TransformersConfig,
    info: EmbeddingModelInfo,
    model_cache_dir: PathBuf,
    device: Option<Device>,
    compute_runtime: ModelComputeRuntime,
    client: reqwest::Client,
    state: Arc<StdMutex<Option<Arc<LoadedTransformersModel>>>>,
    loading: Arc<Mutex<()>>,
}

struct LoadedTransformersModel {
    tokenizers: PreparedTokenizers,
    sessions: session::SessionOwner,
    queue: Arc<scheduler::InferenceQueue>,
}

struct PreparedTokenizers {
    probe: Tokenizer,
    inputs: Tokenizer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransformersExecutionProvider {
    Cpu,
    CoreMl,
    WebGpu,
    Cuda,
    DirectMl,
}

#[derive(Clone)]
struct PreparedBatch {
    input_ids: Vec<i64>,
    attention_mask: Vec<i64>,
    token_type_ids: Vec<i64>,
    position_ids: Vec<i64>,
    padding_input_id: i64,
    batch_size: usize,
    sequence_length: usize,
    truncated: Vec<usize>,
}

impl TransformersEmbeddingModel {
    pub(crate) fn new(
        entry: TransformersConfig,
        options: ModelConfig,
        compute_runtime: ModelComputeRuntime,
    ) -> Self {
        let model_cache_dir = options
            .cache_dir
            .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(|| default_model_cache(LocalModelBackend::Transformers));
        Self {
            entry,
            info: EmbeddingModelInfo::new(
                ModelInfo::new(
                    entry.provider.to_owned(),
                    entry.model.to_owned(),
                    [
                        crate::domain::ContentKind::Text,
                        crate::domain::ContentKind::Code,
                    ],
                    entry.max_batch_size,
                )
                .expect("valid model metadata"),
                entry.dimension,
                entry.metric,
                Some(entry.max_input_tokens),
                None,
            )
            .expect("valid embedding metadata"),
            model_cache_dir,
            device: options.device,
            compute_runtime,
            client: reqwest::Client::new(),
            state: Arc::new(StdMutex::new(None)),
            loading: Arc::new(Mutex::new(())),
        }
    }

    async fn ensure_loaded(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
        concurrency: usize,
        execution_lease: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Arc<LoadedTransformersModel>, ModelError> {
        check_cancelled(signal)?;
        if let Some(loaded) = &*lock_std_mutex(&self.state) {
            return Ok(Arc::clone(loaded));
        }
        let acquire = Arc::clone(&self.loading).lock_owned();
        let gate = if let Some(signal) = signal {
            tokio::select! { biased;
                () = signal.cancelled() => return Err(ModelError::cancelled("Transformers preparation was cancelled")),
                gate = acquire => gate,
            }
        } else {
            acquire.await
        };
        if let Some(loaded) = &*lock_std_mutex(&self.state) {
            return Ok(Arc::clone(loaded));
        }
        self.load(on_progress, signal, concurrency, execution_lease, gate)
            .await
    }

    async fn load(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
        concurrency: usize,
        execution_lease: Option<Arc<dyn Send + Sync>>,
        gate: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Arc<LoadedTransformersModel>, ModelError> {
        let model_artifact = onnx_artifact(self.entry.dtype)?;
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
        let hugging_face = self
            .model_cache_dir
            .join(self.entry.download.hugging_face.repo)
            .join(self.entry.download.hugging_face.revision);
        let model_scope = self
            .model_cache_dir
            .join("modelscope")
            // Keep main's cache namespace so Node and Rust reuse the same
            // verified fallback snapshot; the Rust backend itself is ORT.
            .join("transformers-js")
            .join(self.entry.download.model_scope.repo.replace('/', "--"))
            .join(self.entry.download.model_scope.revision);
        let resolved = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: self.entry.reference,
                sources: [
                    ArtifactSource::hugging_face(self.entry.download.hugging_face, hugging_face),
                    ArtifactSource::model_scope(self.entry.download.model_scope, model_scope),
                ],
                artifacts: self.entry.download.artifacts,
                reporter: &reporter,
                signal,
            },
        )
        .await?;
        let model_path = resolved.paths.get(model_artifact).cloned().ok_or_else(|| {
            ModelError::storage_failure("Resolved Transformers model artifact is missing")
        })?;
        let tokenizer_path = resolved
            .paths
            .get("tokenizer.json")
            .cloned()
            .ok_or_else(|| {
                ModelError::storage_failure("Resolved Transformers tokenizer artifact is missing")
            })?;

        let tokenizer = fs::read(&tokenizer_path).await.map_err(|error| {
            ModelError::storage_failure("Unable to read Transformers tokenizer").with_cause(error)
        })?;
        let tokenizer = Tokenizer::from_bytes(&tokenizer).map_err(|error| {
            ModelError::new(
                crate::EngineError::STORAGE_FAILURE,
                "Transformers tokenization failed",
                Some(format!(
                    "model={} repo={}",
                    self.entry.reference, self.entry.repo
                )),
            )
            .with_cause(error)
        })?;
        let tokenizers = prepare_tokenizers(tokenizer, self.entry)?;
        let device = self.device;
        let state = Arc::clone(&self.state);
        let signal = signal.cloned();
        self.compute_runtime
            .run(move || {
                // Keep initialization serialized and owned even if its awaiting
                // future is dropped while native loading is in progress.
                let _execution_lease = execution_lease;
                let _gate = gate;
                check_cancelled(signal.as_ref())?;
                let sessions =
                    session::SessionOwner::load(model_path, device, concurrency, &reporter)?;
                check_cancelled(signal.as_ref())?;
                let loaded = Arc::new(LoadedTransformersModel {
                    tokenizers,
                    sessions,
                    queue: Arc::new(scheduler::InferenceQueue::default()),
                });
                *lock_std_mutex(&state) = Some(Arc::clone(&loaded));
                reporter.finish();
                Ok(loaded)
            })
            .await?
    }
}

#[async_trait]
impl EmbeddingModel for TransformersEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    fn execution_concurrency(&self, requested: usize) -> usize {
        policy::execution_limit(
            self.entry,
            resolve_execution_provider(self.device),
            requested,
        )
    }

    async fn prepare(&self, options: EmbeddingPrepareOptions) -> Result<(), ModelError> {
        self.ensure_loaded(
            options.on_progress,
            options.signal.as_ref(),
            options.execution_concurrency,
            options.execution_lease,
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            error
                .wrap(
                    "Transformers model preparation failed",
                    Some(format!(
                        "model={} repo={}",
                        self.entry.reference, self.entry.repo
                    )),
                )
                .shared()
        })
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
        check_cancelled(options.signal.as_ref())?;
        let initial_budget = if options.purpose == EmbeddingPurpose::Document {
            options.execution_concurrency
        } else {
            1
        };
        let loaded = self
            .ensure_loaded(
                options.on_progress.clone(),
                options.signal.as_ref(),
                initial_budget,
                options.execution_lease.clone(),
            )
            .await
            .map_err(|error| {
                error.wrap(
                    "Transformers embedding failed",
                    Some(format!(
                        "model={} repo={}",
                        self.entry.reference, self.entry.repo
                    )),
                )
            })?;
        let purpose = options.purpose;
        let prefix = match purpose {
            EmbeddingPurpose::Document => self.entry.document_prefix,
            EmbeddingPurpose::Query => self.entry.query_prefix,
        };
        let bytes = scheduler::input_bytes(inputs, prefix)?;
        // Reserve bounded storage before copying caller-owned text.
        let admission = loaded
            .queue
            .admit(bytes, purpose, options.signal.as_ref())
            .await?;
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
        let result = loaded
            .queue
            .submit(
                Arc::clone(&loaded),
                self.entry,
                self.compute_runtime.clone(),
                texts,
                options,
                admission,
            )
            .await?;
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

impl TransformersExecutionProvider {
    const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::CoreMl => "coreml",
            Self::WebGpu => "webgpu",
            Self::Cuda => "cuda",
            Self::DirectMl => "dml",
        }
    }
}

fn resolve_execution_provider(device: Option<Device>) -> TransformersExecutionProvider {
    match device.or_else(crate::config::default_runtime_device) {
        None | Some(Device::Cpu) => TransformersExecutionProvider::Cpu,
        Some(Device::Metal | Device::Auto) if cfg!(target_os = "macos") => {
            TransformersExecutionProvider::Cpu
        }
        Some(Device::Metal) => TransformersExecutionProvider::CoreMl,
        Some(Device::Cuda) => TransformersExecutionProvider::Cuda,
        // ORT's CPU/MLAS path is both faster and substantially smaller than
        // CoreML for the catalog's quantized ONNX model on Apple Silicon.
        Some(Device::Auto) if cfg!(all(target_os = "windows", feature = "cuda")) => {
            // The official CUDA 12 runtime contains CPU and CUDA, not DirectML.
            TransformersExecutionProvider::Cuda
        }
        Some(Device::Auto) if cfg!(target_os = "windows") => {
            TransformersExecutionProvider::DirectMl
        }
        Some(Device::Auto) if cfg!(all(target_os = "linux", target_arch = "x86_64")) => {
            TransformersExecutionProvider::Cuda
        }
        Some(Device::Vulkan | Device::Auto) => TransformersExecutionProvider::WebGpu,
    }
}

fn load_session(
    path: &Path,
    provider: TransformersExecutionProvider,
    threads: usize,
) -> Result<Session, ModelError> {
    let builder = Session::builder(&super::native_runtime::initialize()?)
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX embedding model").with_cause(error)
        })?
        .with_intra_threads(threads)
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX intra-op threads").with_cause(error)
        })?
        .with_inter_threads(1)
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX inter-op threads").with_cause(error)
        })?
        .with_parallel_execution(false)
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX inference threads").with_cause(error)
        })?;
    let builder = configure_execution_provider(builder, provider)?;
    let mut builder = builder
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX embedding model").with_cause(error)
        })?;
    builder.commit_from_file(path).map_err(|error| {
        ModelError::storage_failure(format!(
            "Unable to load {} ONNX embedding model",
            provider.name()
        ))
        .with_cause(error)
    })
}

fn configure_execution_provider(
    builder: ort::session::builder::SessionBuilder,
    provider: TransformersExecutionProvider,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    match provider {
        TransformersExecutionProvider::Cpu => Ok(builder),
        TransformersExecutionProvider::CoreMl => configure_coreml(builder),
        TransformersExecutionProvider::WebGpu => configure_webgpu(builder),
        TransformersExecutionProvider::Cuda => configure_cuda(builder),
        TransformersExecutionProvider::DirectMl => configure_directml(builder),
    }
}

#[cfg(target_os = "macos")]
fn configure_coreml(
    builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    builder
        .with_execution_providers([ort::ep::CoreML::default().build().error_on_failure()])
        .map_err(|error| {
            ModelError::internal("Unable to configure CoreML ONNX embedding model")
                .with_cause(error)
        })
}

#[cfg(not(target_os = "macos"))]
fn configure_coreml(
    _builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    Err(ModelError::unsupported(
        "CoreML ONNX accelerator is unavailable in this build",
    ))
}

#[cfg(feature = "vulkan")]
fn configure_webgpu(
    builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    builder
        .with_execution_providers([ort::ep::WebGPU::default().build().error_on_failure()])
        .map_err(|error| {
            ModelError::internal("Unable to configure WebGPU ONNX embedding model")
                .with_cause(error)
        })
}

#[cfg(not(feature = "vulkan"))]
fn configure_webgpu(
    _builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    Err(ModelError::unsupported(
        "WebGPU ONNX accelerator is unavailable in this build",
    ))
}

#[cfg(feature = "cuda")]
fn configure_cuda(
    builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    builder
        .with_execution_providers([ort::ep::CUDA::default().build().error_on_failure()])
        .map_err(|error| {
            ModelError::internal("Unable to configure CUDA ONNX embedding model").with_cause(error)
        })
}

#[cfg(not(feature = "cuda"))]
fn configure_cuda(
    _builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    Err(ModelError::unsupported(
        "CUDA ONNX accelerator is unavailable in this build",
    ))
}

#[cfg(target_os = "windows")]
fn configure_directml(
    builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    builder
        .with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()])
        .map_err(|error| {
            ModelError::internal("Unable to configure DirectML ONNX embedding model")
                .with_cause(error)
        })
}

#[cfg(not(target_os = "windows"))]
fn configure_directml(
    _builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, ModelError> {
    Err(ModelError::unsupported(
        "DirectML ONNX accelerator is unavailable in this build",
    ))
}

fn merge_prepared_batches(batches: &[&PreparedBatch]) -> Result<PreparedBatch, ModelError> {
    let sequence_length = batches
        .iter()
        .map(|batch| batch.sequence_length)
        .max()
        .unwrap_or_default();
    let batch_size = batches.iter().map(|batch| batch.batch_size).sum::<usize>();
    if sequence_length == 0 || batch_size == 0 {
        return Err(ModelError::internal(
            "Unable to merge an empty ONNX embedding batch",
        ));
    }

    let capacity = batch_size.saturating_mul(sequence_length);
    let mut input_ids = Vec::with_capacity(capacity);
    let mut attention_mask = Vec::with_capacity(capacity);
    let mut token_type_ids = Vec::with_capacity(capacity);
    let mut position_ids = Vec::with_capacity(capacity);
    let mut truncated = Vec::new();
    let mut batch_offset = 0_usize;
    for batch in batches {
        for row in 0..batch.batch_size {
            append_padded_row(
                &mut input_ids,
                &batch.input_ids,
                row,
                batch.sequence_length,
                sequence_length,
                batch.padding_input_id,
            );
            append_padded_row(
                &mut attention_mask,
                &batch.attention_mask,
                row,
                batch.sequence_length,
                sequence_length,
                0,
            );
            append_padded_row(
                &mut token_type_ids,
                &batch.token_type_ids,
                row,
                batch.sequence_length,
                sequence_length,
                0,
            );
            append_padded_row(
                &mut position_ids,
                &batch.position_ids,
                row,
                batch.sequence_length,
                sequence_length,
                0,
            );
        }
        truncated.extend(batch.truncated.iter().map(|index| batch_offset + index));
        batch_offset += batch.batch_size;
    }

    Ok(PreparedBatch {
        input_ids,
        attention_mask,
        token_type_ids,
        position_ids,
        padding_input_id: batches[0].padding_input_id,
        batch_size,
        sequence_length,
        truncated,
    })
}

fn append_padded_row(
    destination: &mut Vec<i64>,
    source: &[i64],
    row: usize,
    source_width: usize,
    destination_width: usize,
    padding: i64,
) {
    let start = row.saturating_mul(source_width);
    let end = start.saturating_add(source_width);
    destination.extend_from_slice(&source[start..end]);
    destination.resize(
        destination.len() + destination_width - source_width,
        padding,
    );
}

fn run_session(
    session: &mut Session,
    prepared: &PreparedBatch,
    entry: TransformersConfig,
    signal: Option<&CancellationToken>,
) -> Result<EmbeddingResult, ModelError> {
    check_cancelled(signal)?;
    let mut inputs = HashMap::<String, DynTensor>::new();
    for input in session.inputs() {
        let data = match input.name() {
            "input_ids" => prepared.input_ids.clone(),
            "attention_mask" => prepared.attention_mask.clone(),
            "token_type_ids" => prepared.token_type_ids.clone(),
            "position_ids" => prepared.position_ids.clone(),
            name => {
                return Err(ModelError::internal(format!(
                    "Unsupported ONNX embedding input: {name}"
                )));
            }
        };
        let tensor = Tensor::from_array(([prepared.batch_size, prepared.sequence_length], data))
            .map_err(|error| {
                ModelError::internal("Unable to create ONNX input tensor").with_cause(error)
            })?;
        inputs.insert(input.name().to_owned(), tensor.upcast());
    }
    let outputs = session.run(inputs).map_err(|error| {
        ModelError::internal("ONNX embedding inference failed").with_cause(error)
    })?;
    let named_output = outputs
        .get("last_hidden_state")
        .or_else(|| outputs.get("token_embeddings"))
        .or_else(|| outputs.get("sentence_embedding"));
    let output = if let Some(output) = named_output {
        output
    } else if outputs.len() > 0 {
        &outputs[0]
    } else {
        return Err(ModelError::internal(
            "ONNX embedding model returned no tensor",
        ));
    };
    let (shape, data) = output.try_extract_tensor::<f32>().map_err(|error| {
        ModelError::new(
            crate::EngineError::INTERNAL,
            "Transformers returned an unexpected tensor",
            None,
        )
        .with_cause(error)
    })?;
    let shape = shape.iter().copied().collect::<Vec<_>>();
    let data = data.to_vec();
    drop(outputs);
    check_cancelled(signal)?;
    let vectors = pool_output(
        &shape,
        &data,
        &prepared.attention_mask,
        prepared.batch_size,
        prepared.sequence_length,
        entry,
    )?;
    Ok(EmbeddingResult {
        vectors,
        truncated: prepared.truncated.clone(),
    })
}

fn prepare_tokenizers(
    mut tokenizer: Tokenizer,
    entry: TransformersConfig,
) -> Result<PreparedTokenizers, ModelError> {
    // WordPiece cloning copies its vocabulary. Configure both immutable variants
    // once per loaded model so requests can share them without cloning or locks.
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: entry.max_input_tokens + 1,
            direction: TruncationDirection::Right,
            ..TruncationParams::default()
        }))
        .map_err(|error| tokenization_error(entry, error))?;
    let probe = tokenizer.clone();
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: entry.max_input_tokens,
            direction: TruncationDirection::Right,
            ..TruncationParams::default()
        }))
        .map_err(|error| tokenization_error(entry, error))?;
    Ok(PreparedTokenizers {
        probe,
        inputs: tokenizer,
    })
}

fn prepare_batch(
    tokenizers: &PreparedTokenizers,
    texts: &[String],
    entry: TransformersConfig,
) -> Result<PreparedBatch, ModelError> {
    let probe = texts
        .iter()
        .map(|text| tokenizers.probe.encode(text.as_str(), true))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| tokenization_error(entry, error))?;
    let truncated = probe
        .iter()
        .enumerate()
        .filter_map(|(index, encoding)| {
            (encoding
                .get_attention_mask()
                .iter()
                .filter(|&&value| value != 0)
                .count()
                > entry.max_input_tokens)
                .then_some(index)
        })
        .collect::<Vec<_>>();

    let padding_input_id = i64::from(PaddingParams::default().pad_id);
    let mut encodings = texts
        .iter()
        .map(|text| tokenizers.inputs.encode(text.as_str(), true))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| tokenization_error(entry, error))?;
    let sequence_length = encodings
        .iter()
        .map(tokenizers::Encoding::len)
        .max()
        .unwrap_or_default();
    let padding = PaddingParams::default();
    for encoding in &mut encodings {
        encoding.pad(
            sequence_length,
            padding.pad_id,
            padding.pad_type_id,
            &padding.pad_token,
            PaddingDirection::Right,
        );
    }
    if sequence_length == 0
        || encodings
            .iter()
            .any(|encoding| encoding.len() != sequence_length)
    {
        return Err(tokenization_error(
            entry,
            "tokenizer returned an unexpected batch shape",
        ));
    }
    let batch_size = encodings.len();
    let mut input_ids = Vec::with_capacity(batch_size * sequence_length);
    let mut attention_mask = Vec::with_capacity(batch_size * sequence_length);
    let mut token_type_ids = Vec::with_capacity(batch_size * sequence_length);
    let mut position_ids = Vec::with_capacity(batch_size * sequence_length);
    for encoding in &encodings {
        input_ids.extend(encoding.get_ids().iter().map(|&value| i64::from(value)));
        attention_mask.extend(
            encoding
                .get_attention_mask()
                .iter()
                .map(|&value| i64::from(value)),
        );
        token_type_ids.extend(
            encoding
                .get_type_ids()
                .iter()
                .map(|&value| i64::from(value)),
        );
        position_ids
            .extend((0..sequence_length).map(|value| i64::try_from(value).unwrap_or(i64::MAX)));
    }
    Ok(PreparedBatch {
        input_ids,
        attention_mask,
        token_type_ids,
        position_ids,
        padding_input_id,
        batch_size,
        sequence_length,
        truncated,
    })
}

fn pool_output(
    shape: &[i64],
    data: &[f32],
    attention_mask: &[i64],
    batch_size: usize,
    sequence_length: usize,
    entry: TransformersConfig,
) -> Result<Vec<Vec<f32>>, ModelError> {
    let expected_batch = i64::try_from(batch_size).unwrap_or(i64::MAX);
    let expected_sequence = i64::try_from(sequence_length).unwrap_or(i64::MAX);
    let expected_dimension = i64::try_from(entry.dimension).unwrap_or(i64::MAX);
    let mut vectors = if shape == [expected_batch, expected_dimension] {
        if data.len() != batch_size * entry.dimension {
            return Err(invalid_tensor(entry, shape));
        }
        data.chunks_exact(entry.dimension)
            .map(<[f32]>::to_vec)
            .collect()
    } else if shape == [expected_batch, expected_sequence, expected_dimension] {
        if data.len() != batch_size * sequence_length * entry.dimension {
            return Err(invalid_tensor(entry, shape));
        }
        let mut vectors = Vec::with_capacity(batch_size);
        for batch_index in 0..batch_size {
            let mut vector = vec![0.0_f64; entry.dimension];
            if entry.pooling == "cls" {
                let offset = batch_index * sequence_length * entry.dimension;
                for (target, &value) in vector
                    .iter_mut()
                    .zip(&data[offset..offset + entry.dimension])
                {
                    *target = f64::from(value);
                }
            } else {
                let mut count = 0_u32;
                for token_index in 0..sequence_length {
                    if attention_mask[batch_index * sequence_length + token_index] == 0 {
                        continue;
                    }
                    count = count.saturating_add(1);
                    let offset = (batch_index * sequence_length + token_index) * entry.dimension;
                    for (target, &value) in vector
                        .iter_mut()
                        .zip(&data[offset..offset + entry.dimension])
                    {
                        *target += f64::from(value);
                    }
                }
                let divisor = f64::from(count.max(1));
                for value in &mut vector {
                    *value /= divisor;
                }
            }
            vectors.push(vector.into_iter().map(narrow_float).collect());
        }
        vectors
    } else {
        return Err(invalid_tensor(entry, shape));
    };
    if entry.normalize {
        for vector in &mut vectors {
            normalize(vector);
        }
    }
    if let Some((vector_index, value_index)) = vectors.iter().enumerate().find_map(|(i, vector)| {
        vector
            .iter()
            .position(|value| !value.is_finite())
            .map(|j| (i, j))
    }) {
        return Err(ModelError::new(
            crate::EngineError::INTERNAL,
            "Transformers returned a non-finite tensor value",
            Some(format!("index={vector_index} offset={value_index}")),
        ));
    }
    Ok(vectors)
}

fn normalize(vector: &mut [f32]) {
    let squared_norm = vector
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum::<f64>();
    if squared_norm > 0.0 {
        let inverse = squared_norm.sqrt().recip();
        for value in vector {
            *value = narrow_float(f64::from(*value) * inverse);
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn narrow_float(value: f64) -> f32 {
    value as f32
}

fn invalid_tensor(entry: TransformersConfig, shape: &[i64]) -> ModelError {
    ModelError::new(
        crate::EngineError::INTERNAL,
        "Transformers returned an unexpected tensor",
        Some(format!(
            "expected=batchx{} actual={}",
            entry.dimension,
            shape
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join("x")
        )),
    )
}

fn tokenization_error(entry: TransformersConfig, cause: impl std::fmt::Display) -> ModelError {
    ModelError::new(
        crate::EngineError::INTERNAL,
        "Transformers tokenization failed",
        Some(format!("model={} repo={}", entry.reference, entry.repo)),
    )
    .with_cause(cause)
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(ModelError::cancelled(
            "Transformers embedding was cancelled",
        ));
    }
    Ok(())
}

fn lock_std_mutex<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn onnx_artifact(dtype: &str) -> Result<&'static str, ModelError> {
    match dtype {
        "fp32" => Ok("onnx/model.onnx"),
        "q8" => Ok("onnx/model_quantized.onnx"),
        "q4" => Ok("onnx/model_q4.onnx"),
        value => Err(ModelError::internal(format!(
            "Unsupported Transformers dtype: {value}"
        ))),
    }
}

mod policy;
mod scheduler;
mod session;

#[cfg(test)]
mod tests;
