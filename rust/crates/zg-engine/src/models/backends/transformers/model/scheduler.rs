//! Runtime-scoped, bounded admission and one native worker. Waiting callers do
//! not occupy compute threads. Queries have reserved capacity and run first.
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use super::policy::BatchPolicy;
use super::{
    Arc, CancellationToken, Content, EmbeddingOptions, EmbeddingPurpose, EmbeddingResult,
    LoadedTransformersModel, ModelComputeRuntime, ModelError, PreparedBatch, StdMutex,
    TransformersConfig, TransformersExecutionProvider, check_cancelled, lock_std_mutex,
    merge_prepared_batches, prepare_batch,
};

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const QUERY_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct InferenceQueue {
    state: StdMutex<QueueState>,
    document_slots: Arc<Semaphore>,
    document_bytes: Arc<Semaphore>,
    query_slots: Arc<Semaphore>,
    query_bytes: Arc<Semaphore>,
}

#[derive(Default)]
struct QueueState {
    running: bool,
    generation: u64,
    queries: VecDeque<Request>,
    documents: VecDeque<Request>,
    policy: BatchPolicy,
    last_query: Option<Instant>,
}

pub(super) struct Admission {
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

struct Request {
    texts: Vec<String>,
    prepared: Option<PreparedBatch>,
    options: EmbeddingOptions,
    reply: oneshot::Sender<Result<EmbeddingResult, ModelError>>,
    _admission: Admission,
}

enum Work {
    Prepare(Vec<Request>),
    Infer(Vec<Request>),
}
enum Completion {
    Prepared(Vec<Request>),
    Inferred(Option<(usize, usize, Duration)>),
}

/// Tokio may drop the worker while another runtime still has callers waiting.
/// Fail queued work and permit a later caller to start a fresh worker.
struct DriverGuard {
    queue: Arc<InferenceQueue>,
    generation: u64,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        let mut state = lock_std_mutex(&self.queue.state);
        if state.generation == self.generation {
            let error = ModelError::internal("ONNX inference worker stopped").shared();
            for request in state.queries.drain(..) {
                request.complete(Err(error.clone()));
            }
            for request in state.documents.drain(..) {
                request.complete(Err(error.clone()));
            }
            state.running = false;
        }
    }
}

impl Default for InferenceQueue {
    fn default() -> Self {
        Self {
            state: StdMutex::default(),
            document_slots: Arc::new(Semaphore::new(16)),
            document_bytes: Arc::new(Semaphore::new(DOCUMENT_BYTES)),
            query_slots: Arc::new(Semaphore::new(4)),
            query_bytes: Arc::new(Semaphore::new(QUERY_BYTES)),
        }
    }
}

impl InferenceQueue {
    pub(super) async fn admit(
        &self,
        bytes: usize,
        purpose: EmbeddingPurpose,
        signal: Option<&CancellationToken>,
    ) -> Result<Admission, ModelError> {
        if bytes > MAX_REQUEST_BYTES {
            return Err(ModelError::invalid_argument(
                "ONNX embedding request exceeds the 8 MiB input budget",
            ));
        }
        let (slots, storage) = if purpose == EmbeddingPurpose::Query {
            (&self.query_slots, &self.query_bytes)
        } else {
            (&self.document_slots, &self.document_bytes)
        };
        let slot = acquire(slots, 1, signal).await?;
        let bytes = acquire(
            storage,
            u32::try_from(bytes.max(1)).expect("bounded input bytes"),
            signal,
        )
        .await?;
        Ok(Admission {
            _slot: slot,
            _bytes: bytes,
        })
    }

    pub(super) async fn submit(
        &self,
        loaded: Arc<LoadedTransformersModel>,
        entry: TransformersConfig,
        compute: ModelComputeRuntime,
        texts: Vec<String>,
        options: EmbeddingOptions,
        admission: Admission,
    ) -> Result<EmbeddingResult, ModelError> {
        check_cancelled(options.signal.as_ref())?;
        let signal = options.signal.clone();
        let (reply, receiver) = oneshot::channel();
        let start = {
            let mut state = lock_std_mutex(&self.state);
            let request = Request {
                texts,
                prepared: None,
                options,
                reply,
                _admission: admission,
            };
            state.push_back(request);
            if state.running {
                None
            } else {
                state.running = true;
                state.generation = state.generation.wrapping_add(1);
                Some(state.generation)
            }
        };
        if let Some(generation) = start {
            let guard = DriverGuard {
                queue: Arc::clone(&loaded.queue),
                generation,
            };
            tokio::spawn(drive(loaded, entry, compute, guard));
        }
        let cancellation = async {
            if let Some(signal) = &signal {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = cancellation => Err(cancelled()),
            result = receiver => result.map_err(|error| ModelError::internal("ONNX inference worker stopped").with_cause(error).shared())?,
        }
    }
}

pub(super) fn input_bytes(
    inputs: &[Vec<Content>],
    prefix: Option<&str>,
) -> Result<usize, ModelError> {
    let mut bytes = 0usize;
    for input in inputs {
        bytes = bytes
            .saturating_add(prefix.map_or(0, str::len))
            .saturating_add(input.len().saturating_sub(1));
        for part in input {
            if let Content::Text(text) | Content::Code(text) = part {
                bytes = bytes.saturating_add(text.len());
            }
        }
    }
    if bytes > MAX_REQUEST_BYTES {
        return Err(ModelError::invalid_argument(
            "ONNX embedding request exceeds the 8 MiB input budget",
        ));
    }
    Ok(bytes)
}

async fn acquire(
    semaphore: &Arc<Semaphore>,
    count: u32,
    signal: Option<&CancellationToken>,
) -> Result<OwnedSemaphorePermit, ModelError> {
    check_cancelled(signal)?;
    let acquire = Arc::clone(semaphore).acquire_many_owned(count);
    let result = if let Some(signal) = signal {
        tokio::select! { biased; () = signal.cancelled() => return Err(cancelled()), permit = acquire => permit }
    } else {
        acquire.await
    };
    result.map_err(|error| ModelError::internal("ONNX admission closed").with_cause(error))
}

impl Request {
    fn cancelled(&self) -> bool {
        self.reply.is_closed()
            || self
                .options
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
    }

    fn complete(self, result: Result<EmbeddingResult, ModelError>) {
        let result = if self.cancelled() {
            Err(cancelled())
        } else {
            result
        };
        let _ = self.reply.send(result);
    }
}

impl QueueState {
    fn queue(&mut self, purpose: EmbeddingPurpose) -> &mut VecDeque<Request> {
        if purpose == EmbeddingPurpose::Query {
            &mut self.queries
        } else {
            &mut self.documents
        }
    }

    fn push_back(&mut self, request: Request) {
        self.queue(request.options.purpose).push_back(request);
    }

    fn restore(&mut self, requests: Vec<Request>) {
        for request in requests.into_iter().rev() {
            self.queue(request.options.purpose).push_front(request);
        }
    }

    fn next(
        &mut self,
        entry: TransformersConfig,
        provider: TransformersExecutionProvider,
    ) -> Option<Work> {
        discard_cancelled(&mut self.queries);
        discard_cancelled(&mut self.documents);
        if let Some(query) = self.queries.pop_front() {
            self.last_query = Some(Instant::now());
            return Some(if query.prepared.is_some() {
                Work::Infer(vec![query])
            } else {
                Work::Prepare(vec![query])
            });
        }
        let first = self.documents.pop_front()?;
        let interactive = self
            .last_query
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1));
        let limit = if provider == TransformersExecutionProvider::Cuda {
            BatchPolicy::request_limit(entry, first.options.execution_concurrency, interactive)
        } else {
            1
        };
        let budget = first.options.execution_concurrency;
        let mut requests = vec![first];
        while requests.len() < limit
            && self
                .documents
                .front()
                .is_some_and(|next| next.options.execution_concurrency == budget)
        {
            requests.push(self.documents.pop_front().expect("front exists"));
        }
        if requests.iter().any(|request| request.prepared.is_none()) {
            return Some(Work::Prepare(requests));
        }
        let first = requests[0].prepared.as_ref().expect("prepared");
        let mut rows = first.batch_size;
        let mut width = first.sequence_length;
        let mut selected = 1;
        for request in &requests[1..] {
            let next = request.prepared.as_ref().expect("prepared");
            let next_width = width.max(next.sequence_length);
            if !self
                .policy
                .fits(entry, rows + next.batch_size, next_width, interactive)
            {
                break;
            }
            rows += next.batch_size;
            width = next_width;
            selected += 1;
        }
        self.restore(requests.split_off(selected));
        Some(Work::Infer(requests))
    }
}

fn discard_cancelled(queue: &mut VecDeque<Request>) {
    let pending = std::mem::take(queue);
    for request in pending {
        if request.cancelled() {
            request.complete(Err(cancelled()));
        } else {
            queue.push_back(request);
        }
    }
}

async fn drive(
    loaded: Arc<LoadedTransformersModel>,
    entry: TransformersConfig,
    compute: ModelComputeRuntime,
    _guard: DriverGuard,
) {
    loop {
        let work = {
            let mut state = lock_std_mutex(&loaded.queue.state);
            let work = state.next(entry, loaded.sessions.provider());
            if work.is_none() {
                state.running = false;
            }
            work
        };
        let Some(work) = work else {
            return;
        };
        let worker = Arc::clone(&loaded);
        let result = compute
            .run(move || {
                catch_unwind(AssertUnwindSafe(|| process(work, &worker, entry)))
                    .map_err(|_| ModelError::internal("ONNX inference worker panicked").shared())
            })
            .await
            .and_then(std::convert::identity);
        let mut state = lock_std_mutex(&loaded.queue.state);
        match result {
            Ok(Completion::Prepared(requests)) => state.restore(requests),
            Ok(Completion::Inferred(Some((rows, width, elapsed)))) => {
                state
                    .policy
                    .observe(rows, width, elapsed, entry.max_batch_size);
            }
            Ok(Completion::Inferred(None)) => {}
            Err(error) => {
                for request in state.queries.drain(..) {
                    request.complete(Err(error.clone()));
                }
                for request in state.documents.drain(..) {
                    request.complete(Err(error.clone()));
                }
                state.running = false;
                return;
            }
        }
    }
}

fn process(work: Work, loaded: &LoadedTransformersModel, entry: TransformersConfig) -> Completion {
    match work {
        Work::Prepare(requests) => {
            let mut prepared = Vec::new();
            for mut request in requests {
                if request.cancelled() {
                    request.complete(Err(cancelled()));
                    continue;
                }
                if request.prepared.is_none() {
                    match prepare_batch(&loaded.tokenizers, &request.texts, entry) {
                        Ok(batch) => {
                            request.prepared = Some(batch);
                            request.texts.clear();
                        }
                        Err(error) => {
                            request.complete(Err(error));
                            continue;
                        }
                    }
                }
                prepared.push(request);
            }
            Completion::Prepared(prepared)
        }
        Work::Infer(requests) => infer(requests, loaded, entry),
    }
}

fn infer(
    requests: Vec<Request>,
    loaded: &LoadedTransformersModel,
    entry: TransformersConfig,
) -> Completion {
    let mut active = Vec::new();
    for request in requests {
        if request.cancelled() {
            request.complete(Err(cancelled()));
        } else {
            active.push(request);
        }
    }
    if active.is_empty() {
        return Completion::Inferred(None);
    }
    let batches = active
        .iter()
        .map(|request| request.prepared.as_ref().expect("prepared inference"))
        .collect::<Vec<_>>();
    let prepared = match merge_prepared_batches(&batches) {
        Ok(prepared) => prepared,
        Err(error) => {
            finish(active, Err(error));
            return Completion::Inferred(None);
        }
    };
    let mut options = active[0].options.clone();
    // Cancelling one member must not terminate the other callers' inference.
    if active.len() > 1 {
        options.signal = None;
    }
    let started = Instant::now();
    let result = loaded.sessions.run(&prepared, entry, &options);
    let elapsed = started.elapsed();
    let observation = (loaded.sessions.provider() == TransformersExecutionProvider::Cuda)
        .then_some((prepared.batch_size, prepared.sequence_length, elapsed));
    finish(active, result);
    Completion::Inferred(observation)
}

fn finish(requests: Vec<Request>, result: Result<EmbeddingResult, ModelError>) {
    match result {
        Ok(result) => {
            let count = requests
                .iter()
                .map(|request| request.prepared.as_ref().expect("prepared").batch_size)
                .sum::<usize>();
            if result.vectors.len() != count {
                finish(
                    requests,
                    Err(
                        ModelError::internal("ONNX batch returned an unexpected vector count")
                            .shared(),
                    ),
                );
                return;
            }
            let mut vectors = result.vectors.into_iter();
            for request in requests {
                let prepared = request.prepared.as_ref().expect("prepared");
                let result = EmbeddingResult {
                    vectors: vectors.by_ref().take(prepared.batch_size).collect(),
                    truncated: prepared.truncated.clone(),
                };
                request.complete(Ok(result));
            }
        }
        Err(error) => {
            for request in requests {
                request.complete(Err(error.clone()));
            }
        }
    }
}

fn cancelled() -> ModelError {
    ModelError::cancelled("Transformers embedding was cancelled")
}

#[cfg(test)]
mod tests;
