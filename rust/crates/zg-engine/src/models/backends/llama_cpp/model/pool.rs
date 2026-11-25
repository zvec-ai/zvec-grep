//! Shared context ownership and dispatch of independent embedding inputs.
use std::collections::{BTreeMap, VecDeque};

use tokio::sync::{oneshot, watch};

use super::worker::Submission;

use super::{
    Arc, CancellationToken, EmbeddingResult, LlamaContextPool, LlamaContextProfile,
    LlamaContextWorker, LlamaExecutionBackend, LlamaExecutionBudget, LlamaModel, LlamaResources,
    LlamaWorkerJob, LlamaWorkerLease, LlamaWorkerResponse, ModelError, Ordering,
    PreparedLlamaBatch, PreparedLlamaInput, StdMutex, check_cancelled, llama_batch_capacity,
    llama_context_worker, llama_micro_batch_capacity, lock_llama_mutex,
};

#[derive(Default)]
pub(super) struct LlamaContextPoolState {
    pub(super) workers: Vec<Arc<LlamaContextWorker>>,
    admission: ContextAdmission,
}

#[derive(Default)]
struct ContextAdmission {
    operations: BTreeMap<usize, OperationAdmission>,
    capacity: usize,
    next_operation: usize,
    in_flight: usize,
}

struct OperationAdmission {
    scope: Option<Arc<()>>,
    registrations: usize,
    limit: usize,
    in_flight: usize,
}

impl ContextAdmission {
    fn register(&mut self, limit: usize, scope: Option<Arc<()>>) -> usize {
        if let Some(scope) = &scope
            && let Some((id, operation)) = self.operations.iter_mut().find(|(_, operation)| {
                operation
                    .scope
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, scope))
            })
        {
            debug_assert_eq!(operation.limit, limit);
            operation.registrations += 1;
            return *id;
        }
        let id = self.next_operation;
        self.next_operation = id.checked_add(1).expect("operation identity exhausted");
        self.operations.insert(
            id,
            OperationAdmission {
                scope,
                registrations: 1,
                limit,
                in_flight: 0,
            },
        );
        id
    }

    fn limit(&self) -> usize {
        self.capacity
    }

    fn can_acquire(&self, id: usize) -> bool {
        self.in_flight < self.limit()
            && self
                .operations
                .get(&id)
                .is_some_and(|operation| operation.in_flight < operation.limit)
    }

    fn try_acquire(&mut self, id: usize) -> bool {
        if !self.can_acquire(id) {
            return false;
        }
        self.operations
            .get_mut(&id)
            .expect("registered operation")
            .in_flight += 1;
        self.in_flight += 1;
        true
    }

    fn release(&mut self, id: usize) {
        let operation = self.operations.get_mut(&id).expect("registered operation");
        assert!(operation.in_flight > 0);
        operation.in_flight -= 1;
        self.in_flight -= 1;
    }

    fn unregister(&mut self, id: usize) {
        let operation = self.operations.get_mut(&id).expect("registered operation");
        operation.registrations -= 1;
        if operation.registrations == 0 {
            assert_eq!(
                operation.in_flight, 0,
                "native jobs must finish before operation release"
            );
            self.operations.remove(&id);
        }
    }
}

pub(super) struct LlamaOperationLease {
    state: Arc<StdMutex<LlamaContextPoolState>>,
    completion: Arc<CompletionSignal>,
    id: usize,
}

pub(super) struct CompletionSignal {
    generation: watch::Sender<u64>,
}

impl Default for CompletionSignal {
    fn default() -> Self {
        Self {
            generation: watch::channel(0).0,
        }
    }
}

impl CompletionSignal {
    fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    fn notify(&self) {
        self.generation
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    async fn wait(&self, observed: u64) {
        // Subscribe after the snapshot safely: watch retains intervening changes.
        let mut receiver = self.generation.subscribe();
        let _ = receiver.wait_for(|current| *current != observed).await;
    }
}

pub(super) struct LlamaResponseSender {
    sender: Option<oneshot::Sender<LlamaWorkerResponse>>,
    lease: Option<LlamaWorkerLease>,
    execution_lease: Option<Arc<dyn Send + Sync>>,
    completion: Arc<CompletionSignal>,
}

impl LlamaResponseSender {
    pub(super) fn send(mut self, result: LlamaWorkerResponse) {
        // Native execution and vector copying have finished. Release ownership
        // before publishing completion so successful callers observe an idle pool.
        drop(self.lease.take());
        drop(self.execution_lease.take());
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for LlamaResponseSender {
    fn drop(&mut self) {
        if self.sender.is_some()
            && let Some(lease) = &self.lease
        {
            lease.worker.failed.store(true, Ordering::Release);
        }
        // A panic/disconnect also releases capacity and wakes async waiters.
        drop(self.sender.take());
        drop(self.lease.take());
        drop(self.execution_lease.take());
        self.completion.notify();
    }
}

struct PendingJob {
    receiver: oneshot::Receiver<LlamaWorkerResponse>,
}

enum Dispatch {
    Submitted(PendingJob),
    Retired(PreparedLlamaInput),
}

impl PendingJob {
    fn try_receive(&mut self) -> Option<LlamaWorkerResponse> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err(ModelError::internal("llama.cpp context worker failed")))
            }
        }
    }
}

struct LlamaBatchExecution {
    backend: LlamaExecutionBackend,
    architecture: String,
    context_size: u32,
    budget: LlamaExecutionBudget,
    concurrency: usize,
}

impl LlamaBatchExecution {
    fn context_profile(
        &self,
        input: &PreparedLlamaInput,
    ) -> Result<LlamaContextProfile, ModelError> {
        if input.tokens.is_empty() || input.tokens.len() > self.context_size as usize {
            return Err(ModelError::invalid_argument(
                "llama.cpp embedding input must fit the nonempty context",
            ));
        }
        let capacity = llama_batch_capacity(input.tokens.len(), self.context_size)?;
        // Each input owns a job; another input's length must not inflate its KV.
        Ok(LlamaContextProfile {
            context_size: if self.architecture == "qwen3" {
                capacity.max(256).min(self.context_size)
            } else {
                self.context_size
            },
            batch_capacity: capacity,
            micro_batch_capacity: llama_micro_batch_capacity(&self.architecture, capacity),
            threads: self.budget.threads,
        })
    }
}

fn collect_completed(
    pending: &mut Vec<PendingJob>,
    vectors: &mut [Option<Vec<f32>>],
    failure: &mut Option<ModelError>,
) {
    let mut index = 0;
    while index < pending.len() {
        if let Some(result) = pending[index].try_receive() {
            drop(pending.swap_remove(index));
            match result {
                Ok((index, vector)) => {
                    if let Some(target) = vectors.get_mut(index) {
                        *target = Some(vector);
                    }
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        } else {
            index += 1;
        }
    }
}

impl LlamaContextPool {
    pub(super) fn new(
        model: Arc<LlamaModel>,
        backend: LlamaExecutionBackend,
        resources: Arc<LlamaResources>,
    ) -> Self {
        Self {
            model,
            backend,
            state: Arc::new(StdMutex::new(LlamaContextPoolState {
                admission: ContextAdmission {
                    capacity: resources.contexts,
                    ..ContextAdmission::default()
                },
                ..LlamaContextPoolState::default()
            })),
            resources,
            completion: Arc::new(CompletionSignal::default()),
        }
    }

    #[cfg(test)]
    pub(super) async fn run(
        &self,
        prepared: PreparedLlamaBatch,
        context_size: u32,
        execution_concurrency: usize,
        scope: Option<Arc<()>>,
        signal: Option<&CancellationToken>,
    ) -> Result<EmbeddingResult, ModelError> {
        self.run_with_lease(
            prepared,
            context_size,
            execution_concurrency,
            scope,
            signal,
            None,
        )
        .await
    }

    pub(super) async fn run_with_lease(
        &self,
        prepared: PreparedLlamaBatch,
        context_size: u32,
        execution_concurrency: usize,
        scope: Option<Arc<()>>,
        signal: Option<&CancellationToken>,
        execution_lease: Option<Arc<dyn Send + Sync>>,
    ) -> Result<EmbeddingResult, ModelError> {
        check_cancelled(signal)?;
        let architecture = self
            .model
            .meta_val_str("general.architecture")
            .map_err(|error| {
                ModelError::internal("Unable to read llama.cpp model architecture")
                    .with_cause(error)
            })?;
        let budget =
            LlamaExecutionBudget::resolve(self.backend, execution_concurrency, &self.resources);
        let execution = LlamaBatchExecution {
            backend: self.backend,
            architecture,
            context_size,
            budget,
            concurrency: execution_concurrency,
        };
        let operation = self.register_operation(budget.contexts, scope);
        run_prepared(
            prepared,
            execution,
            operation,
            signal,
            execution_lease,
            || self.create_worker(),
        )
        .await
    }

    pub(super) fn register_operation(
        &self,
        limit: usize,
        scope: Option<Arc<()>>,
    ) -> Arc<LlamaOperationLease> {
        LlamaOperationLease::register(&self.state, &self.completion, limit, scope)
    }

    #[cfg(test)]
    pub(super) async fn lease_worker(
        &self,
        operation: &Arc<LlamaOperationLease>,
        signal: Option<&CancellationToken>,
    ) -> Result<LlamaWorkerLease, ModelError> {
        loop {
            let generation = self.completion.generation();
            check_cancelled(signal)?;
            if let Some(worker) = operation.try_lease_worker(|| self.create_worker())? {
                return Ok(worker);
            }
            tokio::select! {
                () = self.completion.wait(generation) => {},
                () = cancelled(signal) => check_cancelled(signal)?,
            }
        }
    }

    fn create_worker(&self) -> Result<Arc<LlamaContextWorker>, ModelError> {
        let model = Arc::clone(&self.model);
        let resources = Arc::clone(&self.resources);
        super::worker::spawn(move |receiver| llama_context_worker(&model, receiver, &resources))
    }
}

async fn cancelled(signal: Option<&CancellationToken>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn run_prepared(
    prepared: PreparedLlamaBatch,
    execution: LlamaBatchExecution,
    operation: Arc<LlamaOperationLease>,
    signal: Option<&CancellationToken>,
    execution_lease: Option<Arc<dyn Send + Sync>>,
    mut create_worker: impl FnMut() -> Result<Arc<LlamaContextWorker>, ModelError> + Send,
) -> Result<EmbeddingResult, ModelError> {
    check_cancelled(signal)?;
    let PreparedLlamaBatch { inputs, truncated } = prepared;
    let input_count = inputs.len();
    let mut inputs = inputs
        .into_iter()
        .map(|input| {
            let profile = execution.context_profile(&input)?;
            Ok((input, profile, false))
        })
        .collect::<Result<VecDeque<_>, ModelError>>()?;
    let completion = Arc::clone(&operation.completion);
    let mut pending = Vec::new();
    let mut vectors = (0..input_count).map(|_| None).collect::<Vec<_>>();
    let mut failure = None;
    loop {
        let generation = completion.generation();
        collect_completed(&mut pending, &mut vectors, &mut failure);
        if failure.is_none()
            && let Err(error) = check_cancelled(signal)
        {
            failure = Some(error);
        }
        while failure.is_none() && !inputs.is_empty() && pending.len() < execution.budget.contexts {
            let lease = match operation.try_lease_worker(&mut create_worker) {
                Ok(Some(lease)) => lease,
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            let (input, profile, retried_retirement) = inputs.pop_front().expect("prepared input");
            match dispatch_input(
                lease,
                input,
                profile,
                &execution,
                signal,
                execution_lease.clone(),
            ) {
                Ok(Dispatch::Submitted(job)) => pending.push(job),
                Ok(Dispatch::Retired(input)) if !retried_retirement => {
                    // Retirement won before enqueue, so this input has never
                    // run. Retry once on the caller's runtime, without replaying
                    // accepted jobs or turning native failures into retries.
                    inputs.push_front((input, profile, true));
                }
                Ok(Dispatch::Retired(_)) => {
                    failure = Some(ModelError::internal(
                        "llama.cpp context worker retired before submission",
                    ));
                    break;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if pending.is_empty() {
            if let Some(error) = failure {
                return Err(error);
            }
            if inputs.is_empty() {
                break;
            }
        }
        // Both admission and completion suspend the task, never a compute thread.
        // On cancellation/failure, drain submitted jobs asynchronously. If this
        // future is dropped, the worker still owns all its permits and keepalives.
        tokio::select! {
            () = completion.wait(generation) => {},
            () = cancelled(signal), if failure.is_none() => {},
        }
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

fn dispatch_input(
    lease: LlamaWorkerLease,
    input: PreparedLlamaInput,
    profile: LlamaContextProfile,
    execution: &LlamaBatchExecution,
    signal: Option<&CancellationToken>,
    execution_lease: Option<Arc<dyn Send + Sync>>,
) -> Result<Dispatch, ModelError> {
    check_cancelled(signal)?;
    tracing::debug!(
        architecture = execution.architecture,
        gpu = execution.backend.is_gpu(),
        backend = ?execution.backend,
        execution_concurrency = execution.concurrency,
        desired_threads = profile.threads,
        desired_contexts = execution.budget.contexts,
        desired_context_tokens = profile.context_size,
        desired_batch_tokens = profile.batch_capacity,
        desired_micro_batch_tokens = profile.micro_batch_capacity,
        "llama.cpp embedding execution budget"
    );
    let (sender, receiver) = oneshot::channel();
    let worker = Arc::clone(&lease.worker);
    let completion = Arc::clone(&lease.operation.completion);
    let job = LlamaWorkerJob {
        input,
        profile,
        signal: signal.cloned(),
        response: LlamaResponseSender {
            sender: Some(sender),
            lease: Some(lease),
            execution_lease,
            completion,
        },
    };
    match worker.submit(job) {
        Submission::Accepted => Ok(Dispatch::Submitted(PendingJob { receiver })),
        Submission::Retired(job) => Ok(Dispatch::Retired(job.input)),
        Submission::Disconnected => Err(ModelError::internal(
            "llama.cpp context worker stopped unexpectedly",
        )),
    }
}

impl LlamaOperationLease {
    fn register(
        state: &Arc<StdMutex<LlamaContextPoolState>>,
        completion: &Arc<CompletionSignal>,
        limit: usize,
        scope: Option<Arc<()>>,
    ) -> Arc<Self> {
        let mut locked = lock_llama_mutex(state);
        let limit = limit.min(locked.admission.capacity);
        let id = locked.admission.register(limit, scope);
        let retired = retire_idle_workers(&mut locked);
        drop(locked);
        shutdown_workers(retired);
        completion.notify();
        Arc::new(Self {
            state: Arc::clone(state),
            completion: Arc::clone(completion),
            id,
        })
    }

    fn try_lease_worker(
        self: &Arc<Self>,
        create_worker: impl FnOnce() -> Result<Arc<LlamaContextWorker>, ModelError>,
    ) -> Result<Option<LlamaWorkerLease>, ModelError> {
        let mut state = lock_llama_mutex(&self.state);
        if !state.admission.try_acquire(self.id) {
            return Ok(None);
        }
        let worker = state
            .workers
            .iter()
            .find(|worker| {
                !worker.busy.load(Ordering::Acquire) && !worker.failed.load(Ordering::Acquire)
            })
            .cloned();
        let worker = match worker {
            Some(worker) => worker,
            None => match create_worker() {
                Ok(worker) => {
                    state.workers.push(Arc::clone(&worker));
                    worker
                }
                Err(error) => {
                    state.admission.release(self.id);
                    self.completion.notify();
                    return Err(error);
                }
            },
        };
        worker.busy.store(true, Ordering::Release);
        tracing::debug!(
            pooled_workers = state.workers.len(),
            active_context_jobs = state.admission.in_flight,
            shared_context_limit = state.admission.limit(),
            "llama.cpp context acquired"
        );
        Ok(Some(LlamaWorkerLease {
            operation: Arc::clone(self),
            worker,
        }))
    }
}

fn retire_idle_workers(state: &mut LlamaContextPoolState) -> Vec<Arc<LlamaContextWorker>> {
    let limit = state.admission.limit();
    let mut retired = Vec::new();
    let mut index = state.workers.len();
    while index > 0 {
        index -= 1;
        let worker = &state.workers[index];
        if !worker.busy.load(Ordering::Acquire)
            && (worker.failed.load(Ordering::Acquire) || (limit > 0 && state.workers.len() > limit))
        {
            retired.push(state.workers.remove(index));
        }
    }
    retired
}

fn shutdown_workers(workers: Vec<Arc<LlamaContextWorker>>) {
    // The runtime-owned shutdown guard joins after native cleanup, or during
    // runtime shutdown. This path can run on the worker itself and must not join.
    for worker in workers {
        worker.retire();
    }
}

impl Drop for LlamaContextPool {
    fn drop(&mut self) {
        shutdown_workers(std::mem::take(&mut lock_llama_mutex(&self.state).workers));
    }
}

impl Drop for LlamaOperationLease {
    fn drop(&mut self) {
        let mut state = lock_llama_mutex(&self.state);
        state.admission.unregister(self.id);
        let retired = retire_idle_workers(&mut state);
        drop(state);
        shutdown_workers(retired);
        self.completion.notify();
    }
}

impl Drop for LlamaWorkerLease {
    fn drop(&mut self) {
        let mut state = lock_llama_mutex(&self.operation.state);
        self.worker.busy.store(false, Ordering::Release);
        state.admission.release(self.operation.id);
        let retired = retire_idle_workers(&mut state);
        drop(state);
        shutdown_workers(retired);
        self.operation.completion.notify();
    }
}

#[cfg(test)]
mod tests;
