use std::{
    collections::{HashMap, VecDeque},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex as StdMutex, MutexGuard as StdMutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use ort::{
    session::{
        Session,
        builder::{GraphOptimizationLevel, PrepackedWeights},
    },
    value::{DynTensor, Tensor},
};
use tokenizers::{
    PaddingParams, PaddingStrategy, Tokenizer, TruncationParams,
    utils::{padding::PaddingDirection, truncation::TruncationDirection},
};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::api::index::options::Device;

use super::{
    catalog::TransformersConfig,
    compute::ModelComputeRuntime,
    download_progress::{ArtifactDownloadProgress, ModelDownloadProgressReporter},
    spi::{
        CreateEmbeddingModelOptions, EmbeddingInput, EmbeddingInputKind, EmbeddingModel,
        EmbeddingModelInfo, EmbeddingModelLimits, EmbeddingModelProgress, EmbeddingOptions,
        EmbeddingPurpose, EmbeddingResult, ModelError, validate_inputs, validate_result,
    },
};

static PARTIAL_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct TransformersEmbeddingModel {
    entry: TransformersConfig,
    info: EmbeddingModelInfo,
    model_cache_dir: PathBuf,
    device: Option<Device>,
    compute_runtime: ModelComputeRuntime,
    client: reqwest::Client,
    state: Mutex<Option<Arc<LoadedTransformersModel>>>,
}

struct LoadedTransformersModel {
    tokenizer: Tokenizer,
    sessions: SessionPool,
}

struct SessionPool {
    model_path: PathBuf,
    prepacked_weights: PrepackedWeights,
    state: StdMutex<SessionPoolState>,
    changed: Condvar,
    fallback: StdMutex<()>,
    coreml_batcher: CoreMlBatcher,
}

struct SessionPoolState {
    provider: TransformersExecutionProvider,
    generation: u64,
    creating: usize,
    sessions: Vec<Arc<SessionSlot>>,
}

struct SessionSlot {
    busy: AtomicBool,
    session: StdMutex<Session>,
}

#[derive(Default)]
struct CoreMlBatcher {
    state: StdMutex<CoreMlBatcherState>,
}

#[derive(Default)]
struct CoreMlBatcherState {
    running: bool,
    pending: VecDeque<Arc<CoreMlBatchRequest>>,
}

struct CoreMlBatchRequest {
    prepared: PreparedBatch,
    signal: Option<CancellationToken>,
    result: StdMutex<Option<Result<EmbeddingResult, String>>>,
    changed: Condvar,
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
    pub(crate) fn new(entry: TransformersConfig, options: CreateEmbeddingModelOptions) -> Self {
        let model_cache_dir = options
            .model_cache_dir
            .or_else(|| env::var_os("ZVEC_GREP_MODEL_CACHE").map(PathBuf::from))
            .unwrap_or_else(default_model_cache_dir);
        Self {
            entry,
            info: EmbeddingModelInfo {
                reference: entry.reference.to_owned(),
                provider: entry.provider.to_owned(),
                name: entry.model.to_owned(),
                dimension: entry.dimension,
                metric: entry.metric,
                endpoint: None,
                default_concurrency: None,
                input_kinds: vec![EmbeddingInputKind::Text],
                limits: EmbeddingModelLimits {
                    max_batch_size: entry.max_batch_size,
                    max_input_tokens: Some(entry.max_input_tokens),
                    max_image_bytes: None,
                },
            },
            model_cache_dir,
            device: options.device,
            compute_runtime: options.compute_runtime.unwrap_or_default(),
            client: reqwest::Client::new(),
            state: Mutex::new(None),
        }
    }

    async fn ensure_loaded(
        &self,
        on_progress: Option<Arc<dyn Fn(EmbeddingModelProgress) + Send + Sync>>,
    ) -> Result<Arc<LoadedTransformersModel>, ModelError> {
        let mut state = self.state.lock().await;
        if let Some(loaded) = &*state {
            return Ok(Arc::clone(loaded));
        }
        let loaded = Arc::new(self.load(on_progress).await?);
        *state = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    async fn load(
        &self,
        on_progress: Option<Arc<dyn Fn(EmbeddingModelProgress) + Send + Sync>>,
    ) -> Result<LoadedTransformersModel, ModelError> {
        let model_artifact = onnx_artifact(self.entry.dtype)?;
        let external_artifact = format!("{model_artifact}_data");
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            on_progress,
            [
                model_artifact.to_owned(),
                external_artifact.clone(),
                "tokenizer.json".to_owned(),
                "config.json".to_owned(),
                "tokenizer_config.json".to_owned(),
            ],
        );
        reporter.start();
        let directory = self.model_directory();
        let model_path = directory.join(model_artifact);
        let tokenizer_path = directory.join("tokenizer.json");
        let config_path = directory.join("config.json");
        let tokenizer_config_path = directory.join("tokenizer_config.json");
        let external_path = directory.join(&external_artifact);

        let (model, tokenizer, config, tokenizer_config) = tokio::join!(
            self.resolve_required(model_artifact, &model_path, &reporter),
            self.resolve_required("tokenizer.json", &tokenizer_path, &reporter),
            self.resolve_optional("config.json", &config_path, &reporter),
            self.resolve_optional("tokenizer_config.json", &tokenizer_config_path, &reporter),
        );
        let model_path = model?;
        let tokenizer_path = tokenizer?;
        config?;
        tokenizer_config?;
        self.resolve_optional(&external_artifact, &external_path, &reporter)
            .await?;

        let tokenizer = fs::read(&tokenizer_path).await.map_err(|error| {
            ModelError::storage_failure("Unable to read Transformers tokenizer").with_cause(error)
        })?;
        let tokenizer = Tokenizer::from_bytes(&tokenizer).map_err(|error| {
            ModelError::new(
                crate::EngineError::STORAGE_FAILURE,
                "Transformers.js tokenization failed",
                Some(format!(
                    "model={} repo={}",
                    self.entry.reference, self.entry.repo
                )),
            )
            .with_cause(error)
        })?;
        let device = self.device;
        let reporter_for_session = reporter.clone();
        let sessions = self
            .compute_runtime
            .run(move || SessionPool::load(model_path, device, &reporter_for_session))
            .await??;
        reporter.finish();
        Ok(LoadedTransformersModel {
            tokenizer,
            sessions,
        })
    }

    async fn resolve_required(
        &self,
        artifact: &str,
        destination: &Path,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<PathBuf, ModelError> {
        if usable_file(destination).await {
            reporter.skip(artifact);
            return Ok(destination.to_path_buf());
        }
        let url = self.artifact_url(artifact);
        self.download(&url, artifact, destination, reporter)
            .await
            .map_err(|error| {
                ModelError::new(
                    crate::EngineError::STORAGE_FAILURE,
                    "Unable to download Transformers.js model artifact",
                    Some(format!("model={} url={url}", self.entry.reference)),
                )
                .with_cause(error)
            })?;
        Ok(destination.to_path_buf())
    }

    async fn resolve_optional(
        &self,
        artifact: &str,
        destination: &Path,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<Option<PathBuf>, ModelError> {
        if usable_file(destination).await {
            reporter.skip(artifact);
            return Ok(Some(destination.to_path_buf()));
        }
        let url = self.artifact_url(artifact);
        let exists = self
            .client
            .head(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if !exists {
            reporter.skip(artifact);
            return Ok(None);
        }
        self.download(&url, artifact, destination, reporter)
            .await
            .map_err(|error| {
                ModelError::new(
                    crate::EngineError::STORAGE_FAILURE,
                    "Unable to download Transformers.js model artifact",
                    Some(format!("model={} url={url}", self.entry.reference)),
                )
                .with_cause(error)
            })?;
        Ok(Some(destination.to_path_buf()))
    }

    async fn download(
        &self,
        url: &str,
        artifact: &str,
        destination: &Path,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<(), ModelError> {
        let parent = destination
            .parent()
            .ok_or_else(|| ModelError::storage_failure("Transformers cache path has no parent"))?;
        fs::create_dir_all(parent).await.map_err(|error| {
            ModelError::storage_failure("Unable to create Transformers cache directory")
                .with_cause(error)
        })?;
        let partial = partial_path(destination);
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
        let mut file = fs::File::create(&partial).await.map_err(|error| {
            ModelError::storage_failure("Unable to create partial Transformers artifact")
                .with_cause(error)
        })?;
        let mut downloaded_bytes = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ModelError::storage_failure(error.to_string()))?;
            file.write_all(&chunk).await.map_err(|error| {
                ModelError::storage_failure("Unable to write Transformers artifact")
                    .with_cause(error)
            })?;
            downloaded_bytes = downloaded_bytes.saturating_add(chunk.len() as u64);
            reporter.report(
                artifact,
                ArtifactDownloadProgress {
                    downloaded_bytes,
                    total_bytes,
                },
            );
        }
        file.flush().await.map_err(|error| {
            ModelError::storage_failure("Unable to flush Transformers artifact").with_cause(error)
        })?;
        if !usable_file(&partial).await {
            let _ = fs::remove_file(&partial).await;
            return Err(ModelError::storage_failure(
                "Downloaded model artifact is empty",
            ));
        }
        if let Err(error) = fs::rename(&partial, destination).await {
            let _ = fs::remove_file(&partial).await;
            return Err(
                ModelError::storage_failure("Unable to publish Transformers artifact")
                    .with_cause(error),
            );
        }
        Ok(())
    }

    fn model_directory(&self) -> PathBuf {
        self.model_cache_dir
            .join(self.entry.repo)
            .join(self.entry.revision)
    }

    fn artifact_url(&self, artifact: &str) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.entry.repo, self.entry.revision, artifact
        )
    }
}

#[async_trait]
impl EmbeddingModel for TransformersEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn embed(
        &self,
        inputs: &[EmbeddingInput],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
        let loaded = self
            .ensure_loaded(options.on_progress.clone())
            .await
            .map_err(|error| {
                ModelError::new(
                    crate::EngineError::INTERNAL,
                    "Transformers.js embedding failed",
                    Some(format!(
                        "model={} repo={}",
                        self.entry.reference, self.entry.repo
                    )),
                )
                .with_cause(error)
            })?;
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
        let tokenizer = loaded.tokenizer.clone();
        let entry = self.entry;
        let signal = options.signal;
        let on_progress = options.on_progress;
        let execution_concurrency = options.execution_concurrency.max(1);
        let result = self
            .compute_runtime
            .run(move || {
                embed_batch(
                    &loaded,
                    tokenizer,
                    &texts,
                    entry,
                    execution_concurrency,
                    signal.as_ref(),
                    on_progress.as_ref(),
                )
            })
            .await??;
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

impl SessionPool {
    fn load(
        model_path: PathBuf,
        device: Option<Device>,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<Self, ModelError> {
        let prepacked_weights = PrepackedWeights::new();
        let requested = resolve_execution_provider(device);
        if cfg!(target_os = "macos")
            && matches!(device, Some(Device::Metal))
            && requested == TransformersExecutionProvider::Cpu
        {
            let warning = "Metal was requested for the Transformers ONNX model, but the available CoreML path regresses throughput and memory; using ORT CPU instead.";
            if !reporter.warning(warning) {
                tracing::warn!("{warning}");
            }
        }
        let (session, provider) = if requested == TransformersExecutionProvider::Cpu {
            (
                load_session(&model_path, requested, &prepacked_weights)?,
                TransformersExecutionProvider::Cpu,
            )
        } else {
            match load_session(&model_path, requested, &prepacked_weights) {
                Ok(session) => (session, requested),
                Err(error) => {
                    let warning = format!(
                        "Transformers.js {} embedding initialization failed ({}), falling back to CPU.",
                        requested.name(),
                        error
                    );
                    if !reporter.warning(warning.clone()) {
                        tracing::warn!("{warning}");
                    }
                    (
                        load_session(
                            &model_path,
                            TransformersExecutionProvider::Cpu,
                            &prepacked_weights,
                        )?,
                        TransformersExecutionProvider::Cpu,
                    )
                }
            }
        };
        Ok(Self {
            model_path,
            prepacked_weights,
            state: StdMutex::new(SessionPoolState {
                provider,
                generation: 0,
                creating: 0,
                sessions: vec![Arc::new(SessionSlot {
                    busy: AtomicBool::new(false),
                    session: StdMutex::new(session),
                })],
            }),
            changed: Condvar::new(),
            fallback: StdMutex::new(()),
            coreml_batcher: CoreMlBatcher::default(),
        })
    }

    fn run<T>(
        &self,
        max_sessions: usize,
        inference: impl Fn(&mut Session) -> Result<T, ModelError>,
    ) -> Result<T, ModelError> {
        let max_sessions = max_sessions.max(1);
        loop {
            let mut state = lock_std_mutex(&self.state);
            if let Some(slot) = state.sessions.iter().find_map(|slot| {
                slot.busy
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                    .then(|| Arc::clone(slot))
            }) {
                drop(state);
                let mut session = lock_session(&slot.session);
                let result = inference(&mut session);
                drop(session);
                slot.busy.store(false, Ordering::Release);
                self.changed.notify_one();
                return result;
            }

            let max_sessions = physical_session_limit(state.provider, max_sessions);
            if state.sessions.len() + state.creating < max_sessions {
                let provider = state.provider;
                let generation = state.generation;
                state.creating += 1;
                drop(state);
                let created = load_session(&self.model_path, provider, &self.prepacked_weights);
                let mut state = lock_std_mutex(&self.state);
                state.creating = state.creating.saturating_sub(1);
                match created {
                    Ok(session) if state.generation == generation && state.provider == provider => {
                        state.sessions.push(Arc::new(SessionSlot {
                            busy: AtomicBool::new(false),
                            session: StdMutex::new(session),
                        }));
                        self.changed.notify_all();
                        continue;
                    }
                    Ok(_) => {
                        self.changed.notify_all();
                        continue;
                    }
                    Err(error) => {
                        self.changed.notify_all();
                        return Err(error);
                    }
                }
            }

            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            drop(state);
        }
    }

    fn run_coreml_batched(
        &self,
        prepared: PreparedBatch,
        entry: TransformersConfig,
        execution_concurrency: usize,
        signal: Option<&CancellationToken>,
    ) -> Result<EmbeddingResult, ModelError> {
        check_cancelled(signal)?;
        let request = Arc::new(CoreMlBatchRequest {
            prepared,
            signal: signal.cloned(),
            result: StdMutex::new(None),
            changed: Condvar::new(),
        });
        let is_leader = {
            let mut state = lock_std_mutex(&self.coreml_batcher.state);
            state.pending.push_back(Arc::clone(&request));
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };

        if is_leader {
            // Give callers admitted by the same runtime wave a small window to
            // join one CoreML invocation. This keeps one compiled CoreML graph
            // while still turning user concurrency into useful device work.
            if execution_concurrency > 1 && request.prepared.batch_size < entry.max_batch_size {
                thread::sleep(Duration::from_millis(1));
            }
            self.drain_coreml_batches(entry, entry.max_batch_size);
        }

        let mut result = lock_std_mutex(&request.result);
        while result.is_none() {
            result = request
                .changed
                .wait(result)
                .unwrap_or_else(PoisonError::into_inner);
        }
        result
            .take()
            .expect("CoreML batch request result was checked")
            .map_err(ModelError::internal)
    }

    fn drain_coreml_batches(&self, entry: TransformersConfig, max_batch_size: usize) {
        loop {
            let requests = {
                let mut state = lock_std_mutex(&self.coreml_batcher.state);
                let mut requests = Vec::new();
                let mut vector_count = 0_usize;
                while let Some(request) = state.pending.front() {
                    let request_size = request.prepared.batch_size;
                    if !requests.is_empty()
                        && vector_count.saturating_add(request_size) > max_batch_size
                    {
                        break;
                    }
                    vector_count = vector_count.saturating_add(request_size);
                    requests.push(
                        state
                            .pending
                            .pop_front()
                            .expect("CoreML pending request disappeared"),
                    );
                }
                if requests.is_empty() {
                    state.running = false;
                    return;
                }
                requests
            };

            let active = requests
                .iter()
                .filter(|request| {
                    !request
                        .signal
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                })
                .cloned()
                .collect::<Vec<_>>();
            if active.is_empty() {
                for request in requests {
                    complete_coreml_request(
                        &request,
                        Err("Transformers.js embedding was cancelled".to_owned()),
                    );
                }
                continue;
            }

            let merged = merge_prepared_batches(
                &active
                    .iter()
                    .map(|request| &request.prepared)
                    .collect::<Vec<_>>(),
            );
            let result = merged.and_then(|prepared| {
                self.run(1, |session| run_session(session, &prepared, entry, None))
            });
            match result {
                Ok(result) => complete_coreml_batch(&requests, &active, &result),
                Err(error) => {
                    let message = error.to_string();
                    for request in requests {
                        let result = if request
                            .signal
                            .as_ref()
                            .is_some_and(CancellationToken::is_cancelled)
                        {
                            Err("Transformers.js embedding was cancelled".to_owned())
                        } else {
                            Err(message.clone())
                        };
                        complete_coreml_request(&request, result);
                    }
                }
            }
        }
    }

    fn provider(&self) -> TransformersExecutionProvider {
        lock_std_mutex(&self.state).provider
    }

    fn fallback_to_cpu(&self) -> Result<bool, ModelError> {
        let _fallback = lock_std_mutex(&self.fallback);
        if self.provider() == TransformersExecutionProvider::Cpu {
            return Ok(false);
        }
        let session = load_session(
            &self.model_path,
            TransformersExecutionProvider::Cpu,
            &self.prepacked_weights,
        )?;
        let mut state = lock_std_mutex(&self.state);
        state.provider = TransformersExecutionProvider::Cpu;
        state.generation = state.generation.wrapping_add(1);
        state.sessions = vec![Arc::new(SessionSlot {
            busy: AtomicBool::new(false),
            session: StdMutex::new(session),
        })];
        self.changed.notify_all();
        Ok(true)
    }
}

const fn physical_session_limit(
    provider: TransformersExecutionProvider,
    requested: usize,
) -> usize {
    match provider {
        TransformersExecutionProvider::CoreMl => 1,
        TransformersExecutionProvider::Cpu
        | TransformersExecutionProvider::WebGpu
        | TransformersExecutionProvider::Cuda
        | TransformersExecutionProvider::DirectMl => requested,
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
    match device {
        None | Some(Device::Cpu) => TransformersExecutionProvider::Cpu,
        Some(Device::Metal | Device::Auto) if cfg!(target_os = "macos") => {
            TransformersExecutionProvider::Cpu
        }
        Some(Device::Metal) => TransformersExecutionProvider::CoreMl,
        Some(Device::Cuda) => TransformersExecutionProvider::Cuda,
        // ORT's CPU/MLAS path is both faster and substantially smaller than
        // CoreML for the catalog's quantized ONNX model on Apple Silicon.
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
    prepacked_weights: &PrepackedWeights,
) -> Result<Session, ModelError> {
    let builder = Session::builder()
        .map_err(|error| {
            ModelError::internal("Unable to configure ONNX embedding model").with_cause(error)
        })?
        .with_prepacked_weights(prepacked_weights)
        .map_err(|error| {
            ModelError::internal("Unable to share ONNX prepacked weights").with_cause(error)
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

fn embed_batch(
    loaded: &LoadedTransformersModel,
    tokenizer: Tokenizer,
    texts: &[String],
    entry: TransformersConfig,
    execution_concurrency: usize,
    signal: Option<&CancellationToken>,
    on_progress: Option<&Arc<dyn Fn(EmbeddingModelProgress) + Send + Sync>>,
) -> Result<EmbeddingResult, ModelError> {
    check_cancelled(signal)?;
    let prepared = prepare_batch(tokenizer, texts, entry)?;
    check_cancelled(signal)?;
    let provider = loaded.sessions.provider();
    let first = if provider == TransformersExecutionProvider::CoreMl {
        loaded
            .sessions
            .run_coreml_batched(prepared.clone(), entry, execution_concurrency, signal)
    } else {
        loaded.sessions.run(execution_concurrency, |session| {
            run_session(session, &prepared, entry, signal)
        })
    };
    match first {
        Ok(result) => Ok(result),
        Err(error) if provider != TransformersExecutionProvider::Cpu => {
            let warning = format!(
                "Transformers.js {} embedding inference failed ({}), falling back to CPU.",
                provider.name(),
                error
            );
            if let Some(on_progress) = on_progress {
                on_progress(EmbeddingModelProgress::Warning {
                    model: entry.reference.to_owned(),
                    message: warning,
                });
            } else {
                tracing::warn!("{warning}");
            }
            loaded.sessions.fallback_to_cpu()?;
            loaded.sessions.run(execution_concurrency, |session| {
                run_session(session, &prepared, entry, signal)
            })
        }
        Err(error) => Err(error),
    }
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
            "Unable to merge an empty CoreML embedding batch",
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

fn complete_coreml_batch(
    requests: &[Arc<CoreMlBatchRequest>],
    active: &[Arc<CoreMlBatchRequest>],
    result: &EmbeddingResult,
) {
    let expected = active
        .iter()
        .map(|request| request.prepared.batch_size)
        .sum::<usize>();
    if result.vectors.len() != expected {
        let error = format!(
            "CoreML merged batch returned {} vectors for {expected} inputs",
            result.vectors.len()
        );
        for request in requests {
            complete_coreml_request(request, Err(error.clone()));
        }
        return;
    }

    let mut offset = 0_usize;
    for request in requests {
        let was_active = active
            .iter()
            .any(|active_request| Arc::ptr_eq(active_request, request));
        if !was_active {
            complete_coreml_request(
                request,
                Err("Transformers.js embedding was cancelled".to_owned()),
            );
            continue;
        }
        let end = offset + request.prepared.batch_size;
        let request_result = if request
            .signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            Err("Transformers.js embedding was cancelled".to_owned())
        } else {
            Ok(EmbeddingResult {
                vectors: result.vectors[offset..end].to_vec(),
                truncated: request.prepared.truncated.clone(),
            })
        };
        complete_coreml_request(request, request_result);
        offset = end;
    }
}

fn complete_coreml_request(request: &CoreMlBatchRequest, result: Result<EmbeddingResult, String>) {
    *lock_std_mutex(&request.result) = Some(result);
    request.changed.notify_one();
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
            "Transformers.js returned an unexpected tensor",
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

fn prepare_batch(
    mut tokenizer: Tokenizer,
    texts: &[String],
    entry: TransformersConfig,
) -> Result<PreparedBatch, ModelError> {
    tokenizer.with_padding(None);
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: entry.max_input_tokens + 1,
            direction: TruncationDirection::Right,
            ..TruncationParams::default()
        }))
        .map_err(|error| tokenization_error(entry, error))?;
    let probe = tokenizer
        .encode_batch(texts.to_vec(), true)
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

    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: entry.max_input_tokens,
            direction: TruncationDirection::Right,
            ..TruncationParams::default()
        }))
        .map_err(|error| tokenization_error(entry, error))?;
    let padding = PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        direction: PaddingDirection::Right,
        ..PaddingParams::default()
    };
    let padding_input_id = i64::from(padding.pad_id);
    tokenizer.with_padding(Some(padding));
    let encodings = tokenizer
        .encode_batch(texts.to_vec(), true)
        .map_err(|error| tokenization_error(entry, error))?;
    let sequence_length = encodings
        .first()
        .map(tokenizers::Encoding::len)
        .unwrap_or_default();
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
            "Transformers.js returned a non-finite tensor value",
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
        "Transformers.js returned an unexpected tensor",
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
        "Transformers.js tokenization failed",
        Some(format!("model={} repo={}", entry.reference, entry.repo)),
    )
    .with_cause(cause)
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(ModelError::cancelled(
            "Transformers.js embedding was cancelled",
        ));
    }
    Ok(())
}

fn lock_session(session: &StdMutex<Session>) -> StdMutexGuard<'_, Session> {
    lock_std_mutex(session)
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
            "Unsupported Transformers.js dtype: {value}"
        ))),
    }
}

async fn usable_file(path: &Path) -> bool {
    fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
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
        .or_else(user_home_dir)
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

#[cfg(windows)]
fn user_home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(windows))]
fn user_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".zvec-grep"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_devices_to_native_rust_execution_providers() {
        assert_eq!(
            resolve_execution_provider(None),
            TransformersExecutionProvider::Cpu
        );
        assert_eq!(
            resolve_execution_provider(Some(Device::Cpu)),
            TransformersExecutionProvider::Cpu
        );
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                resolve_execution_provider(Some(Device::Metal)),
                TransformersExecutionProvider::Cpu
            );
            assert_eq!(
                resolve_execution_provider(Some(Device::Auto)),
                TransformersExecutionProvider::Cpu
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            resolve_execution_provider(Some(Device::Metal)),
            TransformersExecutionProvider::CoreMl
        );
        assert_eq!(
            resolve_execution_provider(Some(Device::Vulkan)),
            TransformersExecutionProvider::WebGpu
        );
        assert_eq!(
            resolve_execution_provider(Some(Device::Cuda)),
            TransformersExecutionProvider::Cuda
        );
    }

    #[test]
    fn coreml_uses_one_physical_session_for_all_user_concurrency() {
        assert_eq!(
            physical_session_limit(TransformersExecutionProvider::CoreMl, 1),
            1
        );
        assert_eq!(
            physical_session_limit(TransformersExecutionProvider::CoreMl, 4),
            1
        );
        assert_eq!(
            physical_session_limit(TransformersExecutionProvider::Cpu, 4),
            4
        );
    }

    #[test]
    fn coreml_batch_merge_repads_rows_and_preserves_indexes() {
        let first = PreparedBatch {
            input_ids: vec![101, 102],
            attention_mask: vec![1, 1],
            token_type_ids: vec![0, 0],
            position_ids: vec![0, 1],
            padding_input_id: 0,
            batch_size: 1,
            sequence_length: 2,
            truncated: vec![0],
        };
        let second = PreparedBatch {
            input_ids: vec![201, 202, 203, 301, 302, 0],
            attention_mask: vec![1, 1, 1, 1, 1, 0],
            token_type_ids: vec![0; 6],
            position_ids: vec![0, 1, 2, 0, 1, 2],
            padding_input_id: 0,
            batch_size: 2,
            sequence_length: 3,
            truncated: vec![1],
        };
        let merged = merge_prepared_batches(&[&first, &second]).expect("merged batch");
        assert_eq!(merged.batch_size, 3);
        assert_eq!(merged.sequence_length, 3);
        assert_eq!(merged.input_ids, [101, 102, 0, 201, 202, 203, 301, 302, 0]);
        assert_eq!(merged.attention_mask, [1, 1, 0, 1, 1, 1, 1, 1, 0]);
        assert_eq!(merged.truncated, [0, 2]);
    }
    use crate::models::spi::EmbeddingMetric;

    fn entry(pooling: &'static str, normalize: bool) -> TransformersConfig {
        TransformersConfig {
            reference: "local/test-transformer",
            provider: "local",
            model: "test-transformer",
            repo: "test/model-ONNX",
            revision: "0123456789abcdef",
            dtype: "q8",
            dimension: 3,
            metric: EmbeddingMetric::Cosine,
            pooling,
            normalize,
            query_prefix: Some("query: "),
            document_prefix: Some("passage: "),
            max_input_tokens: 2,
            max_batch_size: 4,
        }
    }

    #[test]
    fn mean_pooling_and_normalization_match_transformers_pipeline() {
        let values = [
            1.0, 0.0, 0.0, // first token
            0.0, 1.0, 0.0, // second token
            9.0, 9.0, 9.0, // padding
        ];
        let vectors = pool_output(&[1, 3, 3], &values, &[1, 1, 0], 1, 3, entry("mean", true))
            .expect("pooling");
        assert!((vectors[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((vectors[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!(vectors[0][2].abs() < f32::EPSILON);
    }

    #[test]
    fn cls_pooling_and_matrix_output_are_supported() {
        let cls = pool_output(
            &[1, 2, 3],
            &[1.0, 2.0, 3.0, 8.0, 8.0, 8.0],
            &[1, 1],
            1,
            2,
            entry("cls", false),
        )
        .expect("CLS pooling");
        assert_eq!(cls, [[1.0, 2.0, 3.0]]);
        let matrix = pool_output(&[1, 3], &[1.0, 2.0, 3.0], &[1], 1, 1, entry("mean", false))
            .expect("matrix output");
        assert_eq!(matrix, [[1.0, 2.0, 3.0]]);
    }

    #[test]
    fn prefixes_and_catalog_info_match_main() {
        let model = TransformersEmbeddingModel::new(
            entry("mean", true),
            CreateEmbeddingModelOptions::default(),
        );
        assert_eq!(model.info().limits.max_input_tokens, Some(2));
        assert_eq!(model.info().input_kinds, [EmbeddingInputKind::Text]);
        assert_eq!(onnx_artifact("q4").expect("q4"), "onnx/model_q4.onnx");
        assert_eq!(
            onnx_artifact("q8").expect("q8"),
            "onnx/model_quantized.onnx"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with a pinned ONNX model"]
    async fn cached_minilm_runs_real_onnx_inference() {
        let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
            .map(PathBuf::from)
            .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
        let entry =
            crate::models::catalog::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
                .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
                .expect("catalog entry");
        let model = TransformersEmbeddingModel::new(
            entry,
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(cache),
                device: Some(Device::Cpu),
                ..CreateEmbeddingModelOptions::default()
            },
        );
        let loaded = model
            .ensure_loaded(None)
            .await
            .expect("download and load cached ONNX model");
        let tokenizer = Tokenizer::from_file(model.model_directory().join("tokenizer.json"))
            .expect("cached tokenizer");
        let prepared = prepare_batch(
            tokenizer,
            &[
                "find authentication middleware".to_owned(),
                "parse a configuration file".to_owned(),
            ],
            entry,
        )
        .expect("tokenize main oracle inputs");
        assert_eq!(
            prepared.input_ids,
            [
                101, 2424, 27280, 2690, 8059, 102, 0, 101, 11968, 3366, 1037, 9563, 5371, 102,
            ]
        );
        assert_eq!(
            prepared.attention_mask,
            [1, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1]
        );
        assert!(prepared.token_type_ids.iter().all(|&value| value == 0));
        let result = model
            .embed(
                &[
                    EmbeddingInput::text("find authentication middleware".to_owned()),
                    EmbeddingInput::text("parse a configuration file".to_owned()),
                ],
                EmbeddingOptions::default(),
            )
            .await
            .expect("real ONNX embedding");
        assert_eq!(result.vectors.len(), 2);
        assert!(result.vectors.iter().all(|vector| vector.len() == 384));
        let main_first_values = [
            [
                -0.082_118_884,
                0.026_001_254,
                -0.016_771_38,
                -0.107_571_7,
                0.079_822_97,
                -0.018_354_345,
                0.035_343_368,
                0.006_443_004_6,
                -0.010_101_517,
                0.013_123_883,
                0.013_252_53,
                -0.035_912_04,
            ],
            [
                0.027_389_433,
                0.040_037_83,
                -0.059_505_902,
                -0.038_954_25,
                -0.012_910_471,
                -0.016_948_676,
                0.027_840_037,
                0.076_388_21,
                -0.106_726_564,
                -0.027_350_506,
                0.054_113_51,
                0.034_919_977,
            ],
        ];
        for (index, vector) in result.vectors.iter().enumerate() {
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4);
            for (offset, (&actual, &expected)) in
                vector.iter().zip(&main_first_values[index]).enumerate()
            {
                assert!(
                    (actual - expected).abs() < 5e-3,
                    "vector={index} offset={offset} actual={actual} expected={expected}"
                );
            }
        }
        assert!(result.truncated.is_empty());

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_loaded = Arc::clone(&loaded);
        let first_barrier = Arc::clone(&barrier);
        let first = model.compute_runtime.run(move || {
            first_loaded.sessions.run(2, |_session| {
                first_barrier.wait();
                Ok(())
            })
        });
        let second_loaded = Arc::clone(&loaded);
        let second = model.compute_runtime.run(move || {
            second_loaded.sessions.run(2, |_session| {
                barrier.wait();
                Ok(())
            })
        });
        let (first, second) = tokio::join!(first, second);
        first
            .expect("first compute task")
            .expect("first pooled session");
        second
            .expect("second compute task")
            .expect("second pooled session");
        assert_eq!(lock_std_mutex(&loaded.sessions.state).sessions.len(), 2);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with a pinned ONNX model"]
    async fn cached_minilm_routes_metal_to_cpu_with_warning() {
        let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
            .map(PathBuf::from)
            .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
        let entry =
            crate::models::catalog::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
                .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
                .expect("catalog entry");
        let warnings = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&warnings);
        let model = TransformersEmbeddingModel::new(
            entry,
            CreateEmbeddingModelOptions {
                model_cache_dir: Some(cache),
                device: Some(Device::Metal),
                ..CreateEmbeddingModelOptions::default()
            },
        );
        let result = model
            .embed(
                &[EmbeddingInput::text("find relevant code".to_owned())],
                EmbeddingOptions {
                    on_progress: Some(Arc::new(move |progress| {
                        if let EmbeddingModelProgress::Warning { message, .. } = progress {
                            lock_std_mutex(&captured).push(message);
                        }
                    })),
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("ORT CPU inference");
        assert_eq!(result.vectors[0].len(), entry.dimension);
        let loaded = model.state.lock().await;
        let provider = loaded
            .as_ref()
            .map(|loaded| loaded.sessions.provider())
            .expect("loaded model");
        assert_eq!(provider, TransformersExecutionProvider::Cpu);
        let warnings = lock_std_mutex(&warnings);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("using ORT CPU instead"));
    }
}
