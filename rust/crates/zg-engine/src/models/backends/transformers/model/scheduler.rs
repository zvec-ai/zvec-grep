//! Bounded input storage, parallel CPU preparation, and prioritized native work.
//! Coalescing depends on shape and age, never on the execution-lane budget.
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
};

use super::policy::{self, BatchPolicy};
use super::{
    Arc, CancellationToken, Content, EmbeddingOptions, EmbeddingPurpose, EmbeddingResult,
    LoadedTransformersModel, ModelComputeRuntime, ModelError, PreparedBatch, StdMutex,
    TransformersConfig, TransformersExecutionProvider, check_cancelled, finish_output,
    lock_std_mutex, merge_prepared_batches, prepare_batch,
};
use crate::models::spi::same_execution_scope;

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const QUERY_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct InferenceQueue {
    state: StdMutex<QueueState>,
    changed: Notify,
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
    enqueued: Instant,
    ready: Instant,
    _admission: Admission,
}

enum Work {
    Infer(Vec<Request>),
    Wait(Instant),
    Empty,
    Blocked,
}

/// Retained both by the async continuation and by its native job. Cancellation
/// or runtime shutdown cannot release any merged member's lease/storage early.
struct BatchResources {
    requests: StdMutex<Vec<Request>>,
}

/// A stopped Tokio runtime must fail queued work and allow later recovery.
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
        Self::new(16)
    }
}

impl InferenceQueue {
    pub(super) fn new(document_slots: usize) -> Self {
        Self {
            state: StdMutex::default(),
            changed: Notify::new(),
            document_slots: Arc::new(Semaphore::new(
                document_slots.clamp(1, Semaphore::MAX_PERMITS),
            )),
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
        let request = Request {
            texts,
            prepared: None,
            options,
            reply,
            enqueued: Instant::now(),
            ready: Instant::now(),
            _admission: admission,
        };
        // CPU preparation has an independent shared compute budget. It can run
        // while native sessions are busy, including at execution concurrency 1.
        tokio::spawn(async move {
            let worker = Arc::clone(&loaded);
            let result = compute.run(move || prepare(request, &worker, entry)).await;
            let Ok(Some(request)) = result else {
                return;
            };
            let queue = Arc::clone(&loaded.queue);
            let available = loaded.sessions.availability_handle();
            let weak = Arc::downgrade(&loaded);
            let generation = {
                let mut state = lock_std_mutex(&queue.state);
                state.push_back(request);
                if state.running {
                    None
                } else {
                    state.running = true;
                    state.generation = state.generation.wrapping_add(1);
                    Some(state.generation)
                }
            };
            drop(loaded);
            queue.changed.notify_one();
            if let Some(generation) = generation {
                let guard = DriverGuard {
                    queue: Arc::clone(&queue),
                    generation,
                };
                tokio::spawn(drive(weak, queue, available, entry, compute, guard));
            }
        });
        let cancellation = async {
            if let Some(signal) = &signal {
                signal.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! { biased;
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
        let Self {
            reply,
            options,
            prepared,
            texts,
            _admission: admission,
            ..
        } = self;
        // Completion also acknowledges ownership release. A caller may drop the
        // model immediately after its final response and then exit the process.
        drop(options);
        drop(admission);
        drop(prepared);
        drop(texts);
        let _ = reply.send(result);
    }
}

impl QueueState {
    fn push_back(&mut self, request: Request) {
        if request.options.purpose == EmbeddingPurpose::Query {
            self.queries.push_back(request);
        } else {
            self.documents.push_back(request);
        }
    }

    #[cfg(test)]
    fn next(
        &mut self,
        entry: TransformersConfig,
        provider: TransformersExecutionProvider,
        now: Instant,
    ) -> Work {
        self.next_eligible(entry, provider, now, |_| true)
    }

    fn next_eligible(
        &mut self,
        entry: TransformersConfig,
        provider: TransformersExecutionProvider,
        now: Instant,
        mut reserve: impl FnMut(&EmbeddingOptions) -> bool,
    ) -> Work {
        discard_cancelled(&mut self.queries);
        discard_cancelled(&mut self.documents);
        // Skip operations at their own C limit so other clients can use idle
        // sessions. Reserve during selection to preserve query priority.
        if let Some(index) = self
            .queries
            .iter()
            .position(|request| reserve(&request.options))
        {
            return Work::Infer(vec![self.queries.remove(index).expect("query exists")]);
        }
        let mut deadline = None;
        let mut selected = None;
        for (index, request) in self.documents.iter().enumerate() {
            let until = request.ready + policy::BATCH_WAIT;
            if provider == TransformersExecutionProvider::Cuda
                && !request.options.fixed_batch
                && now < until
                && !self.has_full_batch(entry, index)
            {
                deadline = Some(deadline.map_or(until, |old: Instant| old.min(until)));
            } else if reserve(&request.options) {
                selected = Some(index);
                break;
            }
        }
        let Some(index) = selected else {
            return if let Some(until) = deadline {
                Work::Wait(until)
            } else if self.queries.is_empty() && self.documents.is_empty() {
                Work::Empty
            } else {
                Work::Blocked
            };
        };
        let first = self.documents.remove(index).expect("selected document");
        if provider != TransformersExecutionProvider::Cuda || first.options.fixed_batch {
            return Work::Infer(vec![first]);
        }
        let batch = first.prepared.as_ref().expect("prepared");
        let mut rows = batch.batch_size;
        let mut width = batch.sequence_length;
        let bucket = BatchPolicy::length_bucket(width);
        let budget = first.options.execution_concurrency;
        let scope = first.options.execution_scope.clone();
        let mut requests = vec![first];
        let mut index = 0;
        while index < self.documents.len() && requests.len() < BatchPolicy::max_requests(entry) {
            let next = &self.documents[index];
            let batch = next.prepared.as_ref().expect("prepared");
            let next_width = width.max(batch.sequence_length);
            if !next.options.fixed_batch
                && next.options.execution_concurrency == budget
                && same_execution_scope(next.options.execution_scope.as_ref(), scope.as_ref())
                && BatchPolicy::length_bucket(batch.sequence_length) == bucket
                && BatchPolicy::fits(entry, rows + batch.batch_size, next_width)
            {
                rows += batch.batch_size;
                width = next_width;
                requests.push(self.documents.remove(index).expect("index exists"));
            } else {
                index += 1;
            }
        }
        Work::Infer(requests)
    }

    fn has_full_batch(&self, entry: TransformersConfig, first_index: usize) -> bool {
        let first = &self.documents[first_index];
        let prepared = first.prepared.as_ref().expect("prepared");
        let mut count = 1;
        let mut rows = prepared.batch_size;
        let mut width = prepared.sequence_length;
        let bucket = BatchPolicy::length_bucket(width);
        for (index, request) in self.documents.iter().enumerate() {
            if count >= BatchPolicy::max_requests(entry) || rows >= BatchPolicy::max_rows(entry) {
                return true;
            }
            let prepared = request.prepared.as_ref().expect("prepared");
            let next_width = width.max(prepared.sequence_length);
            if index != first_index
                && !request.options.fixed_batch
                && request.options.execution_concurrency == first.options.execution_concurrency
                && same_execution_scope(
                    request.options.execution_scope.as_ref(),
                    first.options.execution_scope.as_ref(),
                )
                && BatchPolicy::length_bucket(prepared.sequence_length) == bucket
                && BatchPolicy::fits(entry, rows + prepared.batch_size, next_width)
            {
                count += 1;
                rows += prepared.batch_size;
                width = next_width;
            }
        }
        count >= BatchPolicy::max_requests(entry) || rows >= BatchPolicy::max_rows(entry)
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

fn prepare(
    mut request: Request,
    loaded: &LoadedTransformersModel,
    entry: TransformersConfig,
) -> Option<Request> {
    if request.cancelled() {
        request.complete(Err(cancelled()));
        return None;
    }
    let started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| {
        prepare_batch(&loaded.tokenizers, &request.texts, entry)
    }))
    .unwrap_or_else(|_| Err(ModelError::internal("ONNX preparation worker panicked").shared()));
    match result {
        Ok(prepared) => {
            loaded.sessions.record_preparation(started.elapsed());
            request.prepared = Some(prepared);
            request.texts.clear();
            request.ready = Instant::now();
            Some(request)
        }
        Err(error) => {
            request.complete(Err(error));
            None
        }
    }
}

async fn drive(
    weak: std::sync::Weak<LoadedTransformersModel>,
    queue: Arc<InferenceQueue>,
    availability: Arc<Notify>,
    entry: TransformersConfig,
    compute: ModelComputeRuntime,
    _guard: DriverGuard,
) {
    let mut active = JoinSet::new();
    loop {
        while active.try_join_next().is_some() {}
        let changed = queue.changed.notified();
        let available = availability.notified();
        tokio::pin!(changed, available);
        changed.as_mut().enable();
        available.as_mut().enable();
        let Some(loaded) = weak.upgrade() else {
            return;
        };
        let mut reservation = None;
        let work = {
            let mut state = lock_std_mutex(&queue.state);
            let work = state.next_eligible(
                entry,
                loaded.sessions.provider(),
                Instant::now(),
                |options| {
                    reservation = loaded.sessions.try_reserve(options);
                    reservation.is_some()
                },
            );
            if matches!(work, Work::Empty) && active.is_empty() {
                state.running = false;
            }
            work
        };
        let deadline = match work {
            Work::Infer(requests) => {
                active.spawn(infer(
                    requests,
                    loaded,
                    entry,
                    compute.clone(),
                    reservation.expect("selected native lane"),
                ));
                continue;
            }
            Work::Wait(until) => Some(until),
            Work::Empty if active.is_empty() => return,
            Work::Empty | Work::Blocked => None,
        };
        // An idle driver must not keep Sessions alive after the final caller
        // drops its model. In-flight tasks and requests retain their own leases.
        drop(loaded);
        let wait = async {
            if let Some(until) = deadline {
                tokio::time::sleep_until(until.into()).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = active.join_next(), if !active.is_empty() => {},
            () = &mut changed => {},
            () = &mut available => {},
            () = wait => {},
        }
    }
}

async fn infer(
    requests: Vec<Request>,
    loaded: Arc<LoadedTransformersModel>,
    entry: TransformersConfig,
    compute: ModelComputeRuntime,
    reservation: super::session::Reservation,
) {
    let mut active = Vec::new();
    for request in requests {
        if request.cancelled() {
            request.complete(Err(cancelled()));
        } else {
            active.push(request);
        }
    }
    if active.is_empty() {
        return;
    }
    let batches = active
        .iter()
        .map(|request| request.prepared.as_ref().expect("prepared inference"))
        .collect::<Vec<_>>();
    let prepared = match merge_prepared_batches(&batches) {
        Ok(prepared) => Arc::new(prepared),
        Err(error) => {
            finish(active, Err(error));
            return;
        }
    };
    let enqueued = active.iter().map(|request| request.enqueued).collect();
    let mut options = active[0].options.clone();
    if active.len() > 1 {
        options.signal = None;
    }
    let resources = Arc::new(BatchResources {
        requests: StdMutex::new(active),
    });
    options.execution_lease = Some(resources.clone());
    let result = loaded
        .sessions
        .run(
            Arc::clone(&prepared),
            entry,
            options,
            enqueued,
            Some(reservation),
        )
        .await;
    // Pooling and normalization are CPU work, independent of the native lane.
    // The driver's active count covers native work only. Postprocessing owns
    // the request independently, so it cannot keep an idle GPU lane occupied.
    tokio::spawn(async move {
        let _ = compute
            .run(move || {
                let started = Instant::now();
                let result = result.and_then(|output| {
                    catch_unwind(AssertUnwindSafe(|| {
                        finish_output(&output, &prepared, entry)
                    }))
                    .unwrap_or_else(|_| {
                        Err(ModelError::internal("ONNX postprocessing worker panicked").shared())
                    })
                });
                loaded.sessions.record_postprocessing(started.elapsed());
                let requests = std::mem::take(&mut *lock_std_mutex(&resources.requests));
                drop(prepared);
                drop(resources);
                drop(loaded);
                finish(requests, result);
            })
            .await;
    });
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
