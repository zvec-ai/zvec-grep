//! Shared ownership and dispatch of physical embedding batches.
use std::collections::{BTreeMap, VecDeque};

use super::{
    Arc, AtomicBool, CancellationToken, Condvar, Duration, EmbeddingResult, LlamaBatchPlan,
    LlamaContextPool, LlamaContextProfile, LlamaContextWorker, LlamaExecutionBackend,
    LlamaExecutionBudget, LlamaModel, LlamaWorkerJob, LlamaWorkerLease, LlamaWorkerMessage,
    LlamaWorkerResponse, ModelError, Ordering, PoisonError, PreparedLlamaBatch, PreparedLlamaInput,
    StdMutex, check_cancelled, llama_context_worker, llama_micro_batch_capacity, lock_llama_mutex,
    mpsc, thread,
};

#[derive(Default)]
pub(super) struct LlamaContextPoolState {
    pub(super) workers: Vec<Arc<LlamaContextWorker>>,
    admission: ContextAdmission,
}

#[derive(Default)]
struct ContextAdmission {
    operations: BTreeMap<usize, OperationAdmission>,
    next_operation: usize,
    in_flight: usize,
}

struct OperationAdmission {
    limit: usize,
    in_flight: usize,
}

impl ContextAdmission {
    fn register(&mut self, limit: usize) -> usize {
        let id = self.next_operation;
        self.next_operation = id.checked_add(1).expect("operation identity exhausted");
        self.operations.insert(
            id,
            OperationAdmission {
                limit,
                in_flight: 0,
            },
        );
        id
    }

    fn limit(&self) -> usize {
        self.operations
            .values()
            .map(|operation| operation.limit)
            .max()
            .unwrap_or(0)
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
        let operation = self.operations.remove(&id).expect("registered operation");
        assert_eq!(
            operation.in_flight, 0,
            "native jobs must finish before operation release"
        );
    }
}

pub(super) struct LlamaOperationLease<'a> {
    pool: &'a LlamaContextPool,
    id: usize,
}

#[derive(Default)]
pub(super) struct CompletionSignal {
    generation: StdMutex<u64>,
    changed: Condvar,
}

impl CompletionSignal {
    fn generation(&self) -> u64 {
        *lock_llama_mutex(&self.generation)
    }

    fn notify(&self) {
        let mut generation = lock_llama_mutex(&self.generation);
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn wait(&self, observed: u64) {
        let generation = lock_llama_mutex(&self.generation);
        drop(
            self.changed
                .wait_while(generation, |current| *current == observed)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

pub(super) struct LlamaResponseSender {
    sender: Option<mpsc::SyncSender<LlamaWorkerResponse>>,
    completion: Arc<CompletionSignal>,
}

impl LlamaResponseSender {
    pub(super) fn send(mut self, result: LlamaWorkerResponse) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for LlamaResponseSender {
    fn drop(&mut self) {
        // Disconnect before waking the owner, including Rust panic paths.
        drop(self.sender.take());
        self.completion.notify();
    }
}

struct PendingJob<'a> {
    receiver: Option<mpsc::Receiver<LlamaWorkerResponse>>,
    lease: LlamaWorkerLease<'a>,
}

impl PendingJob<'_> {
    fn try_receive(&mut self) -> Option<LlamaWorkerResponse> {
        match self.receiver.as_ref().expect("pending response").try_recv() {
            Ok(result) => {
                self.receiver = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                self.lease.worker.failed.store(true, Ordering::Release);
                Some(Err(ModelError::internal("llama.cpp context worker failed")))
            }
        }
    }
}

impl Drop for PendingJob<'_> {
    fn drop(&mut self) {
        // Error/cancellation must drain native work before returning its slot.
        if let Some(receiver) = self.receiver.take()
            && receiver.recv().is_err()
        {
            self.lease.worker.failed.store(true, Ordering::Release);
        }
    }
}

struct LlamaBatchExecution {
    architecture: String,
    context_size: u32,
    budget: LlamaExecutionBudget,
    concurrency: usize,
}

impl LlamaBatchExecution {
    fn context_profile(&self, plan: &LlamaBatchPlan) -> LlamaContextProfile {
        // Capacity belongs to this physical batch, not the longest input
        // elsewhere in the application call.
        LlamaContextProfile {
            context_size: if self.architecture == "qwen3" {
                plan.token_capacity.max(256).min(self.context_size)
            } else {
                self.context_size
            },
            batch_capacity: plan.token_capacity,
            micro_batch_capacity: llama_micro_batch_capacity(
                &self.architecture,
                plan.token_capacity,
                self.budget.batch_tokens,
            ),
            sequence_capacity: plan.sequence_capacity,
            threads: self.budget.threads,
        }
    }
}

fn collect_completed(
    pending: &mut Vec<PendingJob<'_>>,
    vectors: &mut [Option<Vec<f32>>],
    failure: &mut Option<ModelError>,
) {
    let mut index = 0;
    while index < pending.len() {
        if let Some(result) = pending[index].try_receive() {
            drop(pending.swap_remove(index));
            match result {
                Ok(output) => {
                    for (index, vector) in output {
                        if let Some(target) = vectors.get_mut(index) {
                            *target = Some(vector);
                        }
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
        cpu_contexts: usize,
    ) -> Self {
        Self {
            model,
            backend,
            cpu_contexts,
            state: StdMutex::new(LlamaContextPoolState::default()),
            changed: Condvar::new(),
            completion: Arc::new(CompletionSignal::default()),
        }
    }

    pub(super) fn run(
        &self,
        prepared: PreparedLlamaBatch,
        context_size: u32,
        execution_concurrency: usize,
        signal: Option<&CancellationToken>,
    ) -> Result<EmbeddingResult, ModelError> {
        check_cancelled(signal)?;
        let PreparedLlamaBatch { inputs, truncated } = prepared;
        let input_count = inputs.len();
        let architecture = self
            .model
            .meta_val_str("general.architecture")
            .map_err(|error| {
                ModelError::internal("Unable to read llama.cpp model architecture")
                    .with_cause(error)
            })?;
        let budget = LlamaExecutionBudget::resolve(
            &architecture,
            self.backend,
            execution_concurrency,
            self.cpu_contexts,
        )?;
        let execution = LlamaBatchExecution {
            architecture,
            context_size,
            budget,
            concurrency: execution_concurrency,
        };
        let plan = LlamaBatchPlan::new(&inputs, context_size, budget.batch_tokens)?;
        let mut batches = VecDeque::from(plan.into_batches(inputs));
        let operation = self.register_operation(budget.contexts);
        let completion = Arc::clone(&self.completion);
        // Declared after operation so unwinding drains jobs before unregistering.
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
            while failure.is_none() && !batches.is_empty() && pending.len() < budget.contexts {
                let leased = if pending.is_empty() {
                    self.lease_worker(&operation, signal).map(Some)
                } else {
                    self.try_lease_worker(&operation)
                };
                let lease = match leased {
                    Ok(Some(lease)) => lease,
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                };
                let inputs = batches.pop_front().expect("planned batch");
                match self.dispatch_batch(lease, inputs, &execution, signal) {
                    Ok(job) => pending.push(job),
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
                break;
            }
            completion.wait(generation);
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

    fn dispatch_batch<'a>(
        &self,
        lease: LlamaWorkerLease<'a>,
        inputs: Vec<PreparedLlamaInput>,
        execution: &LlamaBatchExecution,
        signal: Option<&CancellationToken>,
    ) -> Result<PendingJob<'a>, ModelError> {
        check_cancelled(signal)?;
        let plan = LlamaBatchPlan::new(
            &inputs,
            execution.context_size,
            execution.budget.batch_tokens,
        )?;
        let profile = execution.context_profile(&plan);
        tracing::debug!(
            architecture = execution.architecture,
            gpu = self.backend.is_gpu(),
            backend = ?self.backend,
            execution_concurrency = execution.concurrency,
            desired_threads = profile.threads,
            desired_contexts = execution.budget.contexts,
            desired_batch_token_budget = ?execution.budget.batch_tokens,
            desired_context_tokens = profile.context_size,
            desired_batch_tokens = profile.batch_capacity,
            desired_micro_batch_tokens = profile.micro_batch_capacity,
            desired_sequences = profile.sequence_capacity,
            "llama.cpp embedding execution budget"
        );
        let (sender, receiver) = mpsc::sync_channel(1);
        let job = LlamaWorkerJob {
            inputs,
            plan,
            profile,
            signal: signal.cloned(),
            response: LlamaResponseSender {
                sender: Some(sender),
                completion: Arc::clone(&self.completion),
            },
        };
        if lease
            .worker
            .sender
            .send(LlamaWorkerMessage::Embed(job))
            .is_err()
        {
            lease.worker.failed.store(true, Ordering::Release);
            return Err(ModelError::internal(
                "llama.cpp context worker stopped unexpectedly",
            ));
        }
        Ok(PendingJob {
            receiver: Some(receiver),
            lease,
        })
    }

    pub(super) fn register_operation(&self, limit: usize) -> LlamaOperationLease<'_> {
        let mut state = lock_llama_mutex(&self.state);
        let id = state.admission.register(limit);
        let retired = retire_idle_workers(&mut state);
        drop(state);
        shutdown_workers(retired);
        self.changed.notify_all();
        self.completion.notify();
        LlamaOperationLease { pool: self, id }
    }

    fn try_lease_worker<'a>(
        &'a self,
        operation: &'a LlamaOperationLease<'_>,
    ) -> Result<Option<LlamaWorkerLease<'a>>, ModelError> {
        let mut state = lock_llama_mutex(&self.state);
        if !state.admission.try_acquire(operation.id) {
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
            None => match self.create_worker() {
                Ok(worker) => {
                    state.workers.push(Arc::clone(&worker));
                    worker
                }
                Err(error) => {
                    state.admission.release(operation.id);
                    self.changed.notify_all();
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
            pool: self,
            worker,
            operation_id: operation.id,
        }))
    }

    pub(super) fn lease_worker<'a>(
        &'a self,
        operation: &'a LlamaOperationLease<'_>,
        signal: Option<&CancellationToken>,
    ) -> Result<LlamaWorkerLease<'a>, ModelError> {
        loop {
            check_cancelled(signal)?;
            if let Some(worker) = self.try_lease_worker(operation)? {
                return Ok(worker);
            }
            let state = lock_llama_mutex(&self.state);
            if state.admission.can_acquire(operation.id) {
                continue;
            }
            drop(
                self.changed
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }

    fn create_worker(&self) -> Result<Arc<LlamaContextWorker>, ModelError> {
        let model = Arc::clone(&self.model);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("zg-llama-context".to_owned())
            .spawn(move || llama_context_worker(&model, &receiver))
            .map_err(|error| {
                ModelError::internal("Unable to create llama.cpp context worker").with_cause(error)
            })?;
        Ok(Arc::new(LlamaContextWorker {
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            sender,
            join: StdMutex::new(Some(join)),
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
    for worker in &workers {
        let _ = worker.sender.send(LlamaWorkerMessage::Shutdown);
    }
    for worker in workers {
        if let Some(join) = lock_llama_mutex(&worker.join).take() {
            let _ = join.join();
        }
    }
}

impl Drop for LlamaContextPool {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        shutdown_workers(std::mem::take(&mut state.workers));
    }
}

impl Drop for LlamaOperationLease<'_> {
    fn drop(&mut self) {
        let mut state = lock_llama_mutex(&self.pool.state);
        state.admission.unregister(self.id);
        // With no operation, retain the warm pool. Worker-local 30s idle expiry
        // releases native buffers; a new lower budget retires surplus workers.
        let retired = retire_idle_workers(&mut state);
        drop(state);
        shutdown_workers(retired);
        self.pool.changed.notify_all();
        self.pool.completion.notify();
    }
}

impl Drop for LlamaWorkerLease<'_> {
    fn drop(&mut self) {
        let mut state = lock_llama_mutex(&self.pool.state);
        self.worker.busy.store(false, Ordering::Release);
        state.admission.release(self.operation_id);
        let retired = retire_idle_workers(&mut state);
        drop(state);
        shutdown_workers(retired);
        self.pool.changed.notify_all();
        self.pool.completion.notify();
    }
}

#[cfg(test)]
mod tests;
