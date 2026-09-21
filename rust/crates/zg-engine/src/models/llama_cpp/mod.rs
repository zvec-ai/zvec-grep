use crate::domain::Content;
use std::{
    env,
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use async_trait::async_trait;
use llama_cpp_2::{
    LlamaBackendDevice, LlamaBackendDeviceType,
    context::{LlamaContext, params::LlamaContextParams},
    list_llama_ggml_backend_devices,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{AddBos, LlamaModel, params::LlamaModelParams},
    token::LlamaToken,
};
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;

use super::{
    artifact_downloader::{ArtifactSource, ResolveArtifacts, resolve_model_artifacts},
    catalog::LlamaCppConfig,
    compute::ModelComputeRuntime,
    download_progress::ModelDownloadProgressReporter,
    spi::{
        EmbeddingModel, EmbeddingOptions, ModelError, input_text, validate_inputs, validate_result,
    },
};
use crate::domain::model::{
    Device, EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult, ModelConfig, ModelInfo,
    ModelProgress,
};

static LLAMA_BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();

pub(crate) struct LlamaCppEmbeddingModel {
    entry: LlamaCppConfig,
    info: EmbeddingModelInfo,
    model_cache_dir: PathBuf,
    device: Option<Device>,
    compute_runtime: ModelComputeRuntime,
    client: reqwest::Client,
    state: Mutex<Option<Arc<LoadedLlamaModel>>>,
}

struct LoadedLlamaModel {
    contexts: LlamaContextPool,
    model: Arc<LlamaModel>,
    model_path: PathBuf,
    gpu: bool,
}

struct LlamaContextPool {
    model: Arc<LlamaModel>,
    state: StdMutex<LlamaContextPoolState>,
    changed: Condvar,
}

#[derive(Default)]
struct LlamaContextPoolState {
    workers: Vec<Arc<LlamaContextWorker>>,
    next_worker_id: usize,
}

struct LlamaContextWorker {
    busy: AtomicBool,
    sender: mpsc::Sender<LlamaWorkerMessage>,
    join: StdMutex<Option<JoinHandle<()>>>,
}

enum LlamaWorkerMessage {
    Embed(LlamaWorkerJob),
    Shutdown,
}

type IndexedEmbedding = (usize, Vec<f32>);
type LlamaWorkerResponse = Result<Vec<IndexedEmbedding>, String>;

struct LlamaWorkerJob {
    inputs: Vec<PreparedLlamaInput>,
    context_size: u32,
    batch_capacity: u32,
    threads: i32,
    signal: Option<CancellationToken>,
    response: mpsc::SyncSender<LlamaWorkerResponse>,
}

#[derive(Clone, Debug)]
struct PreparedLlamaInput {
    index: usize,
    tokens: Vec<LlamaToken>,
}

struct PreparedLlamaBatch {
    inputs: Vec<PreparedLlamaInput>,
    truncated: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LlamaContextProfile {
    context_size: u32,
    batch_capacity: u32,
    threads: i32,
}

struct LlamaWorkerLease<'a> {
    pool: &'a LlamaContextPool,
    worker: Arc<LlamaContextWorker>,
}

impl LlamaCppEmbeddingModel {
    pub(crate) fn new(
        entry: LlamaCppConfig,
        options: ModelConfig,
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
                max_input_tokens: Some(entry.context_size),
                max_image_bytes: None,
            },
            model_cache_dir,
            device: options.device,
            compute_runtime,
            client: reqwest::Client::new(),
            state: Mutex::new(None),
        }
    }

    async fn ensure_loaded(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<Arc<LoadedLlamaModel>, ModelError> {
        let mut state = self.state.lock().await;
        if let Some(model) = &*state {
            return Ok(Arc::clone(model));
        }
        let model = Arc::new(self.load_model(on_progress, signal).await?);
        *state = Some(Arc::clone(&model));
        Ok(model)
    }

    async fn load_model(
        &self,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        signal: Option<&CancellationToken>,
    ) -> Result<LoadedLlamaModel, ModelError> {
        let artifact = self
            .entry
            .download
            .artifacts
            .first()
            .ok_or_else(|| ModelError::internal("llama.cpp artifact metadata is missing"))?;
        let reporter = ModelDownloadProgressReporter::new(
            self.entry.reference,
            on_progress,
            [artifact.path.to_owned()],
        );
        reporter.start();
        let model_scope = self
            .model_cache_dir
            .join("modelscope")
            .join("llama-cpp")
            .join(self.entry.download.model_scope.repo.replace('/', "--"))
            .join(self.entry.download.model_scope.revision);
        let resolved = resolve_model_artifacts(
            &self.client,
            ResolveArtifacts {
                model: self.entry.reference,
                sources: [
                    ArtifactSource::hugging_face(
                        self.entry.download.hugging_face,
                        self.model_cache_dir.clone(),
                    )
                    .with_local_path(artifact.path, self.entry.cache_file),
                    ArtifactSource::model_scope(self.entry.download.model_scope, model_scope),
                ],
                artifacts: self.entry.download.artifacts,
                reporter: &reporter,
                signal,
            },
        )
        .await?;
        let path = resolved.paths.get(artifact.path).cloned().ok_or_else(|| {
            ModelError::storage_failure("Resolved llama.cpp model artifact is missing")
        })?;
        validate_gguf_file(&path, self.entry.uri).await?;
        let device = self.device;
        let reporter_for_load = reporter.clone();
        let model = self
            .compute_runtime
            .run(move || load_model_with_fallback(&path, device, &reporter_for_load))
            .await??;
        reporter.finish();
        Ok(model)
    }

    async fn fallback_to_cpu(
        &self,
        failed: Arc<LoadedLlamaModel>,
        on_progress: Option<Arc<dyn Fn(ModelProgress) + Send + Sync>>,
        cause: &ModelError,
    ) -> Result<Arc<LoadedLlamaModel>, ModelError> {
        let mut state = self.state.lock().await;
        if let Some(current) = &*state
            && !Arc::ptr_eq(current, &failed)
        {
            return Ok(Arc::clone(current));
        }
        let warning =
            format!("llama.cpp GPU embedding context failed ({cause}), falling back to CPU.");
        report_warning(self.entry.reference, on_progress.as_ref(), warning);
        let path = failed.model_path.clone();
        let previous = state.take();
        drop(previous);
        drop(failed);
        let loaded = Arc::new(
            self.compute_runtime
                .run(move || load_cpu_model(&path))
                .await??,
        );
        *state = Some(Arc::clone(&loaded));
        Ok(loaded)
    }
}

#[async_trait]
impl EmbeddingModel for LlamaCppEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs, |content| {
            matches!(content, Content::Text(_))
        })?;
        let on_progress = options.on_progress.clone();
        let model = self
            .ensure_loaded(options.on_progress, options.signal.as_ref())
            .await
            .map_err(|error| embed_error(self.entry, error))?;
        let purpose = options.purpose;
        let texts = inputs
            .iter()
            .map(|input| Ok(format_text(&input_text(input)?, purpose, self.entry.format)))
            .collect::<Result<Vec<_>, ModelError>>()?;
        let entry = self.entry;
        let signal = options.signal;
        let execution_concurrency = options.execution_concurrency.max(1);
        let loaded = Arc::clone(&model);
        let texts_for_first_attempt = texts.clone();
        let signal_for_first_attempt = signal.clone();
        let first = self
            .compute_runtime
            .run(move || {
                embed_texts(
                    &loaded,
                    &texts_for_first_attempt,
                    entry,
                    execution_concurrency,
                    signal_for_first_attempt.as_ref(),
                )
            })
            .await?;
        let result = match first {
            Ok(result) => result,
            Err(error) if model.gpu => {
                let cpu = self
                    .fallback_to_cpu(model, on_progress, &error)
                    .await
                    .map_err(|fallback| embed_error(entry, fallback))?;
                self.compute_runtime
                    .run(move || {
                        embed_texts(&cpu, &texts, entry, execution_concurrency, signal.as_ref())
                    })
                    .await?
                    .map_err(|error| embed_error(entry, error))?
            }
            Err(error) => return Err(embed_error(entry, error)),
        };
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

fn load_model_with_fallback(
    path: &Path,
    device: Option<Device>,
    reporter: &ModelDownloadProgressReporter,
) -> Result<LoadedLlamaModel, ModelError> {
    llama_backend()?;
    let wants_gpu = !matches!(device, None | Some(Device::Cpu));
    if wants_gpu {
        if let Some(selected) = select_gpu_device(device) {
            match load_gpu_model(path, selected.index) {
                Ok(model) => return Ok(model),
                Err(error) => {
                    let warning =
                        format!("llama.cpp GPU model load failed ({error}), falling back to CPU.");
                    if !reporter.warning(warning.clone()) {
                        tracing::warn!("{warning}");
                    }
                }
            }
        } else {
            let warning = format!(
                "llama.cpp {} GPU backend is unavailable, falling back to CPU.",
                requested_device_name(device)
            );
            if !reporter.warning(warning.clone()) {
                tracing::warn!("{warning}");
            }
        }
    }

    load_cpu_model(path)
}

fn load_gpu_model(path: &Path, device_index: usize) -> Result<LoadedLlamaModel, ModelError> {
    let params = LlamaModelParams::default()
        .with_devices(&[device_index])
        .map_err(|error| {
            ModelError::internal("Unable to select llama.cpp GPU device").with_cause(error)
        })?
        .with_n_gpu_layers(u32::MAX);
    LlamaModel::load_from_file(llama_backend()?, path, &params)
        .map(|model| loaded_llama_model(model, path, true))
        .map_err(|error| {
            ModelError::storage_failure("Unable to load llama.cpp GPU embedding model")
                .with_cause(error)
        })
}

fn load_cpu_model(path: &Path) -> Result<LoadedLlamaModel, ModelError> {
    let params = LlamaModelParams::default().with_n_gpu_layers(0);
    LlamaModel::load_from_file(llama_backend()?, path, &params)
        .map(|model| loaded_llama_model(model, path, false))
        .map_err(|error| {
            ModelError::storage_failure("Unable to load llama.cpp embedding model")
                .with_cause(error)
        })
}

fn loaded_llama_model(model: LlamaModel, path: &Path, gpu: bool) -> LoadedLlamaModel {
    let model = Arc::new(model);
    LoadedLlamaModel {
        contexts: LlamaContextPool::new(Arc::clone(&model)),
        model,
        model_path: path.to_path_buf(),
        gpu,
    }
}

fn select_gpu_device(device: Option<Device>) -> Option<LlamaBackendDevice> {
    let devices = list_llama_ggml_backend_devices();
    devices.into_iter().find(|candidate| {
        let backend = candidate.backend.to_ascii_lowercase();
        let name = candidate.name.to_ascii_lowercase();
        match device {
            Some(Device::Metal) => {
                backend == "mtl"
                    || backend.contains("metal")
                    || name.starts_with("mtl")
                    || name.contains("metal")
            }
            Some(Device::Vulkan) => backend.contains("vulkan") || name.contains("vulkan"),
            Some(Device::Cuda) => backend.contains("cuda") || name.contains("cuda"),
            Some(Device::Auto) => {
                matches!(
                    candidate.device_type,
                    LlamaBackendDeviceType::Gpu
                        | LlamaBackendDeviceType::IntegratedGpu
                        | LlamaBackendDeviceType::Accelerator
                ) || (!backend.contains("cpu") && !name.contains("cpu"))
            }
            None | Some(Device::Cpu) => false,
        }
    })
}

const fn requested_device_name(device: Option<Device>) -> &'static str {
    match device {
        Some(Device::Auto) => "auto",
        Some(Device::Metal) => "metal",
        Some(Device::Vulkan) => "vulkan",
        Some(Device::Cuda) => "cuda",
        None | Some(Device::Cpu) => "cpu",
    }
}

fn report_warning(
    model: &str,
    on_progress: Option<&Arc<dyn Fn(ModelProgress) + Send + Sync>>,
    message: String,
) {
    if let Some(on_progress) = on_progress {
        on_progress(ModelProgress::Warning {
            model: model.to_owned(),
            message,
        });
    } else {
        tracing::warn!("{message}");
    }
}

fn threads_per_context(gpu: bool, execution_concurrency: usize) -> i32 {
    if gpu || execution_concurrency <= 1 {
        return 0;
    }
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let threads = (cores / execution_concurrency).max(1);
    i32::try_from(threads).unwrap_or(i32::MAX)
}

fn embed_texts(
    loaded: &LoadedLlamaModel,
    texts: &[String],
    entry: LlamaCppConfig,
    execution_concurrency: usize,
    signal: Option<&CancellationToken>,
) -> Result<EmbeddingResult, ModelError> {
    check_cancelled(signal)?;
    let context_size = entry
        .context_size
        .min(loaded.model.n_ctx_train() as usize)
        .max(1);
    let context_size_u32 = u32::try_from(context_size).map_err(|error| {
        ModelError::internal("llama.cpp context size is too large").with_cause(error)
    })?;
    let mut truncated = Vec::new();
    let mut inputs = Vec::with_capacity(texts.len());
    for (index, text) in texts.iter().enumerate() {
        check_cancelled(signal)?;
        let mut tokens = loaded
            .model
            .str_to_token(text, AddBos::Always)
            .map_err(|error| {
                ModelError::internal("Unable to tokenize llama.cpp embedding input")
                    .with_cause(error)
            })?;
        if tokens.len() > context_size {
            let final_token = tokens.last().copied();
            tokens.truncate(context_size.saturating_sub(4).max(1));
            if let Some(final_token) = final_token.filter(|token| {
                loaded.model.is_eog_token(*token) || *token == loaded.model.token_sep()
            }) && tokens.last() != Some(&final_token)
            {
                tokens.push(final_token);
            }
            truncated.push(index);
        }
        inputs.push(PreparedLlamaInput { index, tokens });
    }
    let context_workers = physical_context_worker_limit(loaded.gpu, execution_concurrency);
    let threads = threads_per_context(loaded.gpu, context_workers);
    loaded.contexts.run(
        PreparedLlamaBatch { inputs, truncated },
        context_size_u32,
        context_workers,
        threads,
        signal,
    )
}

fn physical_context_worker_limit(gpu: bool, requested: usize) -> usize {
    if gpu {
        requested.clamp(1, 2)
    } else {
        requested.max(1)
    }
}

impl LlamaContextPool {
    fn new(model: Arc<LlamaModel>) -> Self {
        Self {
            model,
            state: StdMutex::new(LlamaContextPoolState::default()),
            changed: Condvar::new(),
        }
    }

    fn run(
        &self,
        prepared: PreparedLlamaBatch,
        context_size: u32,
        max_workers: usize,
        threads: i32,
        signal: Option<&CancellationToken>,
    ) -> Result<EmbeddingResult, ModelError> {
        check_cancelled(signal)?;
        let PreparedLlamaBatch { inputs, truncated } = prepared;
        let input_count = inputs.len();
        let desired_workers = max_workers.max(1).min(input_count.max(1));
        let workers = self.lease_workers(max_workers.max(1), desired_workers, signal)?;
        let worker_count = workers.len();
        let mut chunks = (0..worker_count).map(|_| Vec::new()).collect::<Vec<_>>();
        for (offset, input) in inputs.into_iter().enumerate() {
            chunks[offset % worker_count].push(input);
        }

        let mut pending = Vec::with_capacity(worker_count);
        let mut first_error = None;
        for (worker, chunk) in workers.into_iter().zip(chunks) {
            let batch_capacity = llama_batch_capacity(
                chunk
                    .iter()
                    .map(|input| input.tokens.len())
                    .max()
                    .unwrap_or(1),
                context_size,
            )?;
            let (response, receiver) = mpsc::sync_channel(1);
            let job = LlamaWorkerJob {
                inputs: chunk,
                context_size,
                batch_capacity,
                threads,
                signal: signal.cloned(),
                response,
            };
            if worker
                .worker
                .sender
                .send(LlamaWorkerMessage::Embed(job))
                .is_err()
            {
                first_error.get_or_insert_with(|| {
                    ModelError::internal("llama.cpp context worker stopped unexpectedly")
                });
                continue;
            }
            pending.push((receiver, worker));
        }

        let mut vectors = (0..input_count).map(|_| None).collect::<Vec<_>>();
        for (receiver, worker) in pending {
            let result = receiver.recv().map_err(|error| {
                ModelError::internal("llama.cpp context worker failed").with_cause(error)
            });
            drop(worker);
            match result {
                Ok(Ok(worker_vectors)) => {
                    for (index, vector) in worker_vectors {
                        if let Some(target) = vectors.get_mut(index) {
                            *target = Some(vector);
                        }
                    }
                }
                Ok(Err(error)) => {
                    first_error.get_or_insert_with(|| ModelError::internal(error));
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let vectors = vectors
            .into_iter()
            .enumerate()
            .map(|(index, vector)| {
                vector.ok_or_else(|| {
                    ModelError::internal(format!(
                        "llama.cpp context worker returned no vector for input {index}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EmbeddingResult { vectors, truncated })
    }

    fn lease_workers(
        &self,
        max_workers: usize,
        desired_workers: usize,
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<LlamaWorkerLease<'_>>, ModelError> {
        loop {
            check_cancelled(signal)?;
            let mut state = lock_llama_mutex(&self.state);
            let mut selected = state
                .workers
                .iter()
                .filter(|worker| {
                    worker
                        .busy
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                })
                .map(Arc::clone)
                .take(desired_workers)
                .collect::<Vec<_>>();

            let mut create_error = None;
            while selected.len() < desired_workers && state.workers.len() < max_workers {
                match self.create_worker(&mut state) {
                    Ok(worker) => {
                        worker.busy.store(true, Ordering::Release);
                        state.workers.push(Arc::clone(&worker));
                        selected.push(worker);
                    }
                    Err(error) => {
                        create_error = Some(error);
                        break;
                    }
                }
            }
            if !selected.is_empty() {
                drop(state);
                return Ok(selected
                    .into_iter()
                    .map(|worker| LlamaWorkerLease { pool: self, worker })
                    .collect());
            }
            if let Some(error) = create_error {
                return Err(error);
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(PoisonError::into_inner);
            drop(guard);
        }
    }

    fn create_worker(
        &self,
        state: &mut LlamaContextPoolState,
    ) -> Result<Arc<LlamaContextWorker>, ModelError> {
        let worker_id = state.next_worker_id;
        state.next_worker_id = state.next_worker_id.saturating_add(1);
        let model = Arc::clone(&self.model);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name(format!("zg-llama-context-{worker_id}"))
            .spawn(move || llama_context_worker(&model, &receiver))
            .map_err(|error| {
                ModelError::internal("Unable to create llama.cpp context worker").with_cause(error)
            })?;
        Ok(Arc::new(LlamaContextWorker {
            busy: AtomicBool::new(false),
            sender,
            join: StdMutex::new(Some(join)),
        }))
    }
}

impl Drop for LlamaContextPool {
    fn drop(&mut self) {
        let workers = {
            let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut state.workers)
        };
        for worker in &workers {
            let _ = worker.sender.send(LlamaWorkerMessage::Shutdown);
        }
        for worker in workers {
            if let Some(join) = lock_llama_mutex(&worker.join).take() {
                let _ = join.join();
            }
        }
    }
}

impl Drop for LlamaWorkerLease<'_> {
    fn drop(&mut self) {
        self.worker.busy.store(false, Ordering::Release);
        self.pool.changed.notify_one();
    }
}

fn llama_context_worker(model: &LlamaModel, receiver: &mpsc::Receiver<LlamaWorkerMessage>) {
    let mut context = None;
    let mut profile = None;
    while let Ok(message) = receiver.recv() {
        let LlamaWorkerMessage::Embed(job) = message else {
            break;
        };
        let desired = LlamaContextProfile {
            context_size: job.context_size,
            batch_capacity: job.batch_capacity,
            threads: job.threads,
        };
        let reusable = profile.is_some_and(|current: LlamaContextProfile| {
            current.context_size == desired.context_size
                && current.threads == desired.threads
                && current.batch_capacity >= desired.batch_capacity
        });
        if !reusable {
            context = None;
            profile = None;
            match create_llama_context(model, desired) {
                Ok(created) => {
                    context = Some(created);
                    profile = Some(desired);
                }
                Err(error) => {
                    let _ = job.response.send(Err(error.to_string()));
                    continue;
                }
            }
        }
        let result = context
            .as_mut()
            .ok_or_else(|| "llama.cpp context worker has no context".to_owned())
            .and_then(|context| {
                run_llama_worker_job(context, &job).map_err(|error| error.to_string())
            });
        let _ = job.response.send(result);
    }
}

fn create_llama_context(
    model: &LlamaModel,
    profile: LlamaContextProfile,
) -> Result<LlamaContext<'_>, ModelError> {
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(profile.context_size))
        .with_n_batch(profile.batch_capacity)
        .with_n_ubatch(profile.batch_capacity.clamp(1, 256))
        .with_n_threads(profile.threads)
        .with_n_threads_batch(profile.threads)
        .with_embeddings(true);
    model
        .new_context(llama_backend()?, params)
        .map_err(|error| {
            ModelError::internal("Unable to create llama.cpp embedding context").with_cause(error)
        })
}

fn run_llama_worker_job(
    context: &mut LlamaContext<'_>,
    job: &LlamaWorkerJob,
) -> Result<Vec<(usize, Vec<f32>)>, ModelError> {
    let mut vectors = Vec::with_capacity(job.inputs.len());
    for input in &job.inputs {
        check_cancelled(job.signal.as_ref())?;
        context.clear_kv_cache();
        let mut batch = LlamaBatch::new(input.tokens.len(), 1);
        batch
            .add_sequence(&input.tokens, 0, false)
            .map_err(|error| {
                ModelError::internal("Unable to prepare llama.cpp embedding batch")
                    .with_cause(error)
            })?;
        context.decode(&mut batch).map_err(|error| {
            ModelError::internal("llama.cpp embedding inference failed").with_cause(error)
        })?;
        let vector = context.embeddings_seq_ith(0).map_err(|error| {
            ModelError::internal("Unable to read llama.cpp embedding output").with_cause(error)
        })?;
        vectors.push((input.index, vector.to_vec()));
    }
    Ok(vectors)
}

fn llama_batch_capacity(token_count: usize, context_size: u32) -> Result<u32, ModelError> {
    let required = u32::try_from(token_count.max(1)).map_err(|error| {
        ModelError::internal("llama.cpp embedding batch is too large").with_cause(error)
    })?;
    let rounded = required.checked_next_power_of_two().unwrap_or(context_size);
    Ok(rounded.max(32.min(context_size)).min(context_size))
}

fn lock_llama_mutex<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn format_text(text: &str, purpose: EmbeddingPurpose, format: &str) -> String {
    if format == "qwen3" {
        return match purpose {
            EmbeddingPurpose::Query => {
                format!("Instruct: Retrieve relevant documents for the given query\nQuery: {text}")
            }
            EmbeddingPurpose::Document => text.to_owned(),
        };
    }
    match purpose {
        EmbeddingPurpose::Query => format!("task: search result | query: {text}"),
        EmbeddingPurpose::Document => format!("title: none | text: {text}"),
    }
}

fn embed_error(entry: LlamaCppConfig, cause: ModelError) -> ModelError {
    cause.wrap(
        "llama.cpp embedding failed",
        Some(format!("model={}", entry.reference)),
    )
}

fn llama_backend() -> Result<&'static LlamaBackend, ModelError> {
    match LLAMA_BACKEND.get_or_init(|| {
        LlamaBackend::init()
            .map(|mut backend| {
                backend.void_logs();
                backend
            })
            .map_err(|error| error.to_string())
    }) {
        Ok(backend) => Ok(backend),
        Err(error) => {
            Err(ModelError::internal("Unable to initialize llama.cpp backend").with_cause(error))
        }
    }
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ModelError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(ModelError::cancelled("llama.cpp embedding was cancelled"));
    }
    Ok(())
}

async fn validate_gguf_file(path: &Path, uri: &str) -> Result<(), ModelError> {
    use tokio::io::AsyncReadExt;

    let metadata = fs::metadata(path).await.map_err(|error| {
        ModelError::storage_failure("Unable to inspect llama.cpp model artifact").with_cause(error)
    })?;
    let mut file = fs::File::open(path).await.map_err(|error| {
        ModelError::storage_failure("Unable to open llama.cpp model artifact").with_cause(error)
    })?;
    let mut data = vec![0_u8; 512];
    let read = file.read(&mut data).await.map_err(|error| {
        ModelError::storage_failure("Unable to inspect llama.cpp model header").with_cause(error)
    })?;
    data.truncate(read);
    if data.starts_with(b"GGUF") {
        return Ok(());
    }
    let size_kb = metadata.len() / 1_024;
    let sniff = String::from_utf8_lossy(&data).to_lowercase();
    let is_html = sniff.contains("<!doctype") || sniff.contains("<html");
    let got = String::from_utf8_lossy(&data[..data.len().min(4)]);
    fs::remove_file(path).await.map_err(|error| {
        ModelError::storage_failure("Unable to remove invalid llama.cpp model artifact")
            .with_cause(error)
    })?;
    if is_html {
        return Err(ModelError::new(
            crate::EngineError::STORAGE_FAILURE,
            "Downloaded local embedding model is HTML, not GGUF",
            Some(format!(
                "model={uri} path={} sizeKB={size_kb}",
                path.display()
            )),
        ));
    }
    Err(ModelError::new(
        crate::EngineError::STORAGE_FAILURE,
        "Local embedding model is not a valid GGUF file",
        Some(format!(
            "model={uri} path={} expected=GGUF actual={got} sizeKB={size_kb}",
            path.display()
        )),
    ))
}

fn default_model_cache_dir() -> PathBuf {
    env::var_os("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(default_home)
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

fn default_home() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = env::var_os("HOME");
    home.map(PathBuf::from).map(|home| home.join(".zvec-grep"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};

    fn entry(reference: &str) -> LlamaCppConfig {
        get_embedding_model_catalog_entry(reference)
            .and_then(EmbeddingCatalogEntry::llama_cpp_config)
            .expect("llama.cpp catalog entry")
    }

    #[test]
    fn formats_embeddinggemma_and_qwen_inputs_like_main() {
        assert_eq!(
            format_text("hello", EmbeddingPurpose::Query, "embeddinggemma"),
            "task: search result | query: hello"
        );
        assert_eq!(
            format_text("hello", EmbeddingPurpose::Document, "embeddinggemma"),
            "title: none | text: hello"
        );
        assert_eq!(
            format_text("hello", EmbeddingPurpose::Query, "qwen3"),
            "Instruct: Retrieve relevant documents for the given query\nQuery: hello"
        );
        assert_eq!(
            format_text("hello", EmbeddingPurpose::Document, "qwen3"),
            "hello"
        );
    }

    #[test]
    fn pins_main_compatible_sources_and_cache_name() {
        let entry = entry("local/embeddinggemma-300m");
        assert_eq!(
            entry.download.hugging_face.revision,
            "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12"
        );
        assert_eq!(
            entry.cache_file,
            "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"
        );
    }

    #[test]
    fn user_concurrency_controls_cpu_threads_per_context() {
        assert_eq!(threads_per_context(true, 4), 0);
        assert_eq!(threads_per_context(false, 1), 0);
        let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        assert_eq!(
            threads_per_context(false, 4),
            i32::try_from((cores / 4).max(1)).unwrap_or(i32::MAX)
        );
    }

    #[test]
    fn metal_context_pool_caps_physical_workers_without_ignoring_user_limit() {
        assert_eq!(physical_context_worker_limit(true, 1), 1);
        assert_eq!(physical_context_worker_limit(true, 2), 2);
        assert_eq!(physical_context_worker_limit(true, 4), 2);
        assert_eq!(physical_context_worker_limit(false, 4), 4);
    }

    #[test]
    fn context_batch_capacity_tracks_tokens_instead_of_full_context() {
        assert_eq!(llama_batch_capacity(7, 2_048).expect("capacity"), 32);
        assert_eq!(llama_batch_capacity(65, 2_048).expect("capacity"), 128);
        assert_eq!(llama_batch_capacity(2_048, 2_048).expect("capacity"), 2_048);
    }

    #[tokio::test]
    async fn rejects_and_removes_invalid_gguf_files() {
        let directory = tempfile::tempdir().expect("temporary cache");
        let path = directory.path().join("bad.gguf");
        fs::write(&path, b"<!doctype html><title>failure</title>")
            .await
            .expect("fixture");
        let error = validate_gguf_file(&path, "hf:test/model.gguf")
            .await
            .expect_err("invalid GGUF must fail");
        assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
        assert!(!path.exists());
    }

    #[tokio::test]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the embeddinggemma GGUF"]
    async fn cached_embeddinggemma_runs_real_llama_cpp_inference() {
        let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
            .map(PathBuf::from)
            .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
        let model = LlamaCppEmbeddingModel::new(
            entry("local/embeddinggemma-300m"),
            ModelConfig {
                cache_dir: Some(cache),
                device: Some(Device::Cpu),
                ..ModelConfig::default()
            },
            crate::models::compute::ModelComputeRuntime::shared(),
        );
        let result = model
            .embed(
                &[
                    vec![Content::Text("find authentication middleware".to_owned())],
                    vec![Content::Text("parse a configuration file".to_owned())],
                ],
                EmbeddingOptions {
                    purpose: EmbeddingPurpose::Query,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("real llama.cpp embedding");
        assert_eq!(result.vectors.len(), 2);
        assert!(result.vectors.iter().all(|vector| vector.len() == 768));
        // The Rust and Node bindings track different llama.cpp revisions, so
        // compare the raw-vector scale and representative coordinates with a
        // small cross-runtime tolerance instead of requiring bit equality.
        let main_first_values = [
            [
                -163.557_48,
                2.360_403_8,
                28.599_663,
                51.072_44,
                -36.060_03,
                -59.653_484,
                -53.783_993,
                -62.748_837,
                49.552_666,
                -7.057_061,
                11.277_911,
                -19.936_779,
            ],
            [
                -171.828_66,
                -21.072_908,
                -12.103_56,
                -41.734_7,
                13.669_633,
                20.436_523,
                -33.258_686,
                13.457_932,
                16.871_897,
                1.374_025,
                -40.426_422,
                -39.468_83,
            ],
        ];
        let main_norms = [1_060.960_9_f32, 1_086.006_5_f32];
        for (index, vector) in result.vectors.iter().enumerate() {
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!(
                (norm - main_norms[index]).abs() < 2.0,
                "vector={index} norm={norm} expected={}",
                main_norms[index]
            );
            for (offset, (&actual, &expected)) in
                vector.iter().zip(&main_first_values[index]).enumerate()
            {
                assert!(
                    (actual - expected).abs() < 2.5,
                    "vector={index} offset={offset} actual={actual} expected={expected}"
                );
            }
        }
        assert!(result.truncated.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the embeddinggemma GGUF"]
    async fn cached_embeddinggemma_uses_metal_and_supports_concurrency() {
        let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
            .map(PathBuf::from)
            .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
        let model = LlamaCppEmbeddingModel::new(
            entry("local/embeddinggemma-300m"),
            ModelConfig {
                cache_dir: Some(cache),
                device: Some(Device::Metal),
                ..ModelConfig::default()
            },
            crate::models::compute::ModelComputeRuntime::shared(),
        );
        let result = model
            .embed(
                &[vec![Content::Text("find relevant code".to_owned())]],
                EmbeddingOptions {
                    execution_concurrency: 2,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("Metal llama.cpp inference");
        assert_eq!(result.vectors[0].len(), 768);
        let first_contents = [vec![Content::Text("authentication middleware".to_owned())]];
        let second_contents = [vec![Content::Text("configuration parser".to_owned())]];
        let first = model.embed(
            &first_contents,
            EmbeddingOptions {
                execution_concurrency: 2,
                ..EmbeddingOptions::default()
            },
        );
        let second = model.embed(
            &second_contents,
            EmbeddingOptions {
                execution_concurrency: 2,
                ..EmbeddingOptions::default()
            },
        );
        let (first, second) = tokio::join!(first, second);
        assert_eq!(
            first.expect("first concurrent embedding").vectors[0].len(),
            768
        );
        assert_eq!(
            second.expect("second concurrent embedding").vectors[0].len(),
            768
        );
        let loaded = model.state.lock().await;
        let using_gpu = loaded.as_ref().is_some_and(|loaded| loaded.gpu);
        assert!(using_gpu, "Metal request unexpectedly fell back to CPU");
    }

    #[tokio::test]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the Qwen3 embedding GGUF"]
    async fn cached_qwen3_runs_real_llama_cpp_inference() {
        let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
            .map(PathBuf::from)
            .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
        let model = LlamaCppEmbeddingModel::new(
            entry("local/qwen3-embedding-0.6b"),
            ModelConfig {
                cache_dir: Some(cache),
                device: Some(Device::Cpu),
                ..ModelConfig::default()
            },
            crate::models::compute::ModelComputeRuntime::shared(),
        );
        let result = model
            .embed(
                &[vec![Content::Text(
                    "find authentication middleware".to_owned(),
                )]],
                EmbeddingOptions {
                    purpose: EmbeddingPurpose::Query,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("real Qwen3 llama.cpp embedding");
        assert_eq!(result.vectors.len(), 1);
        assert_eq!(result.vectors[0].len(), 1_024);
        assert!(result.vectors[0].iter().all(|value| value.is_finite()));
        assert!(result.truncated.is_empty());
    }
}
