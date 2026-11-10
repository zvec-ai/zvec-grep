//! Bounded input storage, parallel CPU preparation, and prioritized native work.
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
};

use super::{
    Arc, CancellationToken, Content, EmbeddingOptions, EmbeddingPurpose, EmbeddingResult,
    LoadedTransformersModel, ModelComputeRuntime, ModelError, PreparedBatch, StdMutex,
    TransformersConfig, check_cancelled, finish_output, lock_std_mutex, prepare_batch,
};

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
    _admission: Admission,
}

enum Work {
    Infer(Box<Request>),
    Empty,
    Blocked,
}

/// Retained both by the async continuation and by its native job. Cancellation
/// or runtime shutdown cannot release the request's lease/storage early.
struct RequestResources {
    request: StdMutex<Option<Request>>,
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

    fn next_eligible(&mut self, mut reserve: impl FnMut(&EmbeddingOptions) -> bool) -> Work {
        discard_cancelled(&mut self.queries);
        discard_cancelled(&mut self.documents);
        // Queries take priority. Skip operations at their execution limit so
        // other clients can use idle sessions within the shared pool capacity.
        for queue in [&mut self.queries, &mut self.documents] {
            if let Some(index) = queue.iter().position(|request| reserve(&request.options)) {
                return Work::Infer(Box::new(queue.remove(index).expect("selected request")));
            }
        }
        if self.queries.is_empty() && self.documents.is_empty() {
            Work::Empty
        } else {
            Work::Blocked
        }
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
            let work = state.next_eligible(|options| {
                reservation = loaded.sessions.try_reserve(options);
                reservation.is_some()
            });
            if matches!(work, Work::Empty) && active.is_empty() {
                state.running = false;
            }
            work
        };
        match work {
            Work::Infer(request) => {
                active.spawn(infer(
                    *request,
                    loaded,
                    entry,
                    compute.clone(),
                    reservation.expect("selected native lane"),
                ));
                continue;
            }
            Work::Empty if active.is_empty() => return,
            Work::Empty | Work::Blocked => {}
        }
        // An idle driver must not keep Sessions alive after the final caller
        // drops its model. In-flight tasks and requests retain their own leases.
        drop(loaded);
        tokio::select! {
            _ = active.join_next(), if !active.is_empty() => {},
            () = &mut changed => {},
            () = &mut available => {},
        }
    }
}

async fn infer(
    mut request: Request,
    loaded: Arc<LoadedTransformersModel>,
    entry: TransformersConfig,
    compute: ModelComputeRuntime,
    reservation: super::session::Reservation,
) {
    if request.cancelled() {
        request.complete(Err(cancelled()));
        return;
    }
    // Preserve the caller's batch exactly; no cross-request merging or copies.
    let prepared = Arc::new(request.prepared.take().expect("prepared inference"));
    let enqueued = Some(request.enqueued);
    let mut options = request.options.clone();
    let resources = Arc::new(RequestResources {
        request: StdMutex::new(Some(request)),
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
                let request = lock_std_mutex(&resources.request)
                    .take()
                    .expect("pending request");
                drop(prepared);
                drop(resources);
                drop(loaded);
                request.complete(result);
            })
            .await;
    });
}

fn cancelled() -> ModelError {
    ModelError::cancelled("Transformers embedding was cancelled")
}

#[cfg(test)]
mod tests;
