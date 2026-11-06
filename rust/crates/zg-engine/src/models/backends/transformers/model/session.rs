//! Each native Session is created, run and destroyed on one fixed OS thread.
//! Jobs retain their permits, inputs and execution leases until native completion.
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, oneshot};

use super::{
    Arc, Device, EmbeddingOptions, EmbeddingPurpose, ModelDownloadProgressReporter, ModelError,
    ModelProgress, NativeOutput, PathBuf, PreparedBatch, Session, StdMutex, TransformersConfig,
    TransformersExecutionProvider, check_cancelled, load_session, lock_std_mutex, policy,
    resolve_execution_provider, run_session,
};
use crate::models::spi::ModelExecutionStats;

pub(super) struct SessionOwner {
    workers: Vec<StdMutex<Option<Worker>>>,
    factory: Arc<Factory>,
    shared: Arc<Shared>,
    requested: TransformersExecutionProvider,
}

type Factory = dyn Fn(usize) -> Result<Box<dyn Runner>, ModelError> + Send + Sync;

trait Runner {
    fn configure(&mut self, _threads: usize, _force_cpu: bool) -> Result<(), ModelError> {
        Ok(())
    }

    fn run(
        &mut self,
        prepared: &PreparedBatch,
        entry: TransformersConfig,
        options: &EmbeddingOptions,
        force_cpu: bool,
    ) -> Result<NativeOutput, ModelError>;
}

struct Shared {
    fallback: AtomicBool,
    available: Arc<Notify>,
    stats: StdMutex<ModelExecutionStats>,
    inflight: AtomicUsize,
    generation: AtomicUsize,
    pool: StdMutex<PoolState>,
}

struct PoolState {
    busy: Vec<Option<EmbeddingPurpose>>,
    cpu_threads: Option<usize>,
    changing: bool,
    drain: bool,
}

/// Owned by the reservation and every native configuration job. If the async
/// waiter is cancelled, new work still waits until all native changes settle.
struct PoolTransition {
    shared: Arc<Shared>,
    threads: usize,
    succeeded: AtomicBool,
}

struct Worker {
    sender: Option<mpsc::Sender<Message>>,
    thread: Option<std::thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

enum Message {
    Run(Box<Job>),
    Configure {
        threads: usize,
        transition: Arc<PoolTransition>,
        reply: oneshot::Sender<Result<(), ModelError>>,
    },
}

struct Job {
    prepared: Arc<PreparedBatch>,
    entry: TransformersConfig,
    options: EmbeddingOptions,
    enqueued: Vec<Instant>,
    reply: oneshot::Sender<NativeCompletion>,
    _permit: LanePermit,
    cpu_threads: Option<usize>,
}

struct NativeCompletion {
    result: Result<NativeOutput, ModelError>,
    retry_on_cpu: bool,
    started: bool,
}

struct LanePermit {
    lane: usize,
    shared: Arc<Shared>,
}

pub(super) struct Reservation {
    lane: usize,
    permit: LanePermit,
    cpu_threads: Option<usize>,
    transition: Option<Arc<PoolTransition>>,
}

impl Drop for LanePermit {
    fn drop(&mut self) {
        lock_std_mutex(&self.shared.pool).busy[self.lane] = None;
        self.shared.available.notify_waiters();
    }
}

impl Drop for PoolTransition {
    fn drop(&mut self) {
        let mut pool = lock_std_mutex(&self.shared.pool);
        pool.cpu_threads = self
            .succeeded
            .load(Ordering::Acquire)
            .then_some(self.threads);
        pool.changing = false;
        drop(pool);
        self.shared.available.notify_waiters();
    }
}

struct SessionState {
    model_path: PathBuf,
    session: Option<Session>,
    provider: TransformersExecutionProvider,
    threads: usize,
    lane: usize,
    shared: Arc<Shared>,
}

impl SessionOwner {
    pub(super) fn load(
        model_path: PathBuf,
        device: Option<Device>,
        concurrency: usize,
        session_count: usize,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<Self, ModelError> {
        let requested = resolve_execution_provider(device);
        if cfg!(target_os = "macos") && device == Some(Device::Metal) {
            let warning = "Metal was requested for the Transformers ONNX model, but the available CoreML path regresses throughput and memory; using ORT CPU instead.";
            if !reporter.warning(warning) {
                tracing::warn!("{warning}");
            }
        }
        let shared = Arc::new(Shared::new(requested, session_count, concurrency));
        let worker_shared = Arc::clone(&shared);
        let factory: Arc<Factory> = Arc::new(move |lane| {
            let fallback = worker_shared.fallback.load(Ordering::Acquire);
            let provider = if fallback {
                TransformersExecutionProvider::Cpu
            } else {
                requested
            };
            let threads = if provider == TransformersExecutionProvider::Cpu {
                lock_std_mutex(&worker_shared.pool)
                    .cpu_threads
                    .unwrap_or_else(|| policy::threads_per_session(concurrency, session_count))
            } else {
                1
            };
            let mut state = SessionState {
                model_path: model_path.clone(),
                session: None,
                provider,
                threads,
                lane,
                shared: Arc::clone(&worker_shared),
            };
            state.ensure_session()?;
            Ok(Box::new(state))
        });
        let owner = Self::new(requested, shared, factory);
        if let Err(error) = owner.initialize() {
            if requested == TransformersExecutionProvider::Cpu {
                return Err(error);
            }
            // Dispose the entire partial GPU pool before constructing the CPU
            // pool. A successful preparation always owns exactly S sessions.
            for worker in &owner.workers {
                drop(lock_std_mutex(worker).take());
            }
            let warning = format!(
                "Transformers {} embedding initialization failed ({}), falling back to CPU.",
                requested.name(),
                error
            );
            if !reporter.warning(warning.clone()) {
                tracing::warn!("{warning}");
            }
            owner.shared.fallback.store(true, Ordering::Release);
            lock_std_mutex(&owner.shared.pool).cpu_threads =
                Some(policy::threads_per_session(concurrency, session_count));
            owner.initialize()?;
        }
        tracing::info!(
            provider = owner.provider().name(),
            sessions = session_count,
            execution_budget = concurrency,
            threads_per_session = owner.stats().cpu_threads,
            "Transformers ONNX {} embedding execution provider selected",
            owner.provider().name()
        );
        Ok(owner)
    }

    fn initialize(&self) -> Result<(), ModelError> {
        for lane in 0..self.workers.len() {
            self.start_worker(lane)?.recv().map_err(|error| {
                ModelError::internal("ONNX worker initialization stopped").with_cause(error)
            })??;
        }
        Ok(())
    }

    fn new(
        requested: TransformersExecutionProvider,
        shared: Arc<Shared>,
        factory: Arc<Factory>,
    ) -> Self {
        let session_count = lock_std_mutex(&shared.pool).busy.len();
        Self {
            workers: (0..session_count).map(|_| StdMutex::new(None)).collect(),
            factory,
            shared,
            requested,
        }
    }

    fn start_worker(
        &self,
        lane: usize,
    ) -> Result<mpsc::Receiver<Result<(), ModelError>>, ModelError> {
        let mut worker = lock_std_mutex(&self.workers[lane]);
        let (sender, ready) = mpsc::channel();
        if worker.is_some() {
            let _ = sender.send(Ok(()));
        } else {
            *worker = Some(Worker::start(
                lane,
                self.requested,
                Arc::clone(&self.factory),
                Arc::clone(&self.shared),
                sender,
            )?);
        }
        Ok(ready)
    }

    pub(super) fn provider(&self) -> TransformersExecutionProvider {
        if self.shared.fallback.load(Ordering::Acquire) {
            TransformersExecutionProvider::Cpu
        } else {
            self.requested
        }
    }

    pub(super) fn stats(&self) -> ModelExecutionStats {
        lock_std_mutex(&self.shared.stats).clone()
    }

    pub(super) fn record_preparation(&self, elapsed: Duration) {
        let mut stats = lock_std_mutex(&self.shared.stats);
        stats.prepared_batches += 1;
        stats.preparation_ns = stats.preparation_ns.saturating_add(nanoseconds(elapsed));
    }

    pub(super) fn record_postprocessing(&self, elapsed: Duration) {
        let mut stats = lock_std_mutex(&self.shared.stats);
        stats.postprocessing_ns = stats.postprocessing_ns.saturating_add(nanoseconds(elapsed));
    }

    #[cfg(test)]
    pub(super) fn availability(&self) -> &Notify {
        &self.shared.available
    }

    pub(super) fn availability_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shared.available)
    }

    pub(super) fn try_reserve(&self, options: &EmbeddingOptions) -> Option<Reservation> {
        let mut pool = lock_std_mutex(&self.shared.pool);
        if pool.changing {
            return None;
        }
        let busy = pool.busy.iter().flatten().count();
        let cpu_threads = if self.provider() == TransformersExecutionProvider::Cpu {
            let desired = if options.purpose == EmbeddingPurpose::Query {
                pool.cpu_threads.unwrap_or_else(|| {
                    policy::threads_per_session(options.execution_concurrency, pool.busy.len())
                })
            } else {
                policy::threads_per_session(options.execution_concurrency, pool.busy.len())
            };
            if busy > 0 && (pool.drain || pool.cpu_threads != Some(desired)) {
                pool.drain = true;
                return None;
            }
            pool.drain = false;
            Some(desired)
        } else {
            // C limits document executions, not numbered slots. Queries have
            // their own one-run budget and may use any idle session in the pool.
            let active = pool
                .busy
                .iter()
                .flatten()
                .filter(|purpose| **purpose == options.purpose)
                .count();
            let limit = if options.purpose == EmbeddingPurpose::Query {
                1
            } else {
                options.execution_concurrency.clamp(1, pool.busy.len())
            };
            if active >= limit {
                return None;
            }
            None
        };
        let lane = pool.busy.iter().position(Option::is_none)?;
        let transition =
            if let Some(threads) = cpu_threads.filter(|_| pool.cpu_threads != cpu_threads) {
                pool.changing = true;
                Some(Arc::new(PoolTransition {
                    shared: Arc::clone(&self.shared),
                    threads,
                    succeeded: AtomicBool::new(false),
                }))
            } else {
                None
            };
        pool.busy[lane] = Some(options.purpose);
        Some(Reservation {
            lane,
            permit: LanePermit {
                lane,
                shared: Arc::clone(&self.shared),
            },
            cpu_threads,
            transition,
        })
    }

    async fn reserve(&self, options: &EmbeddingOptions) -> Result<Reservation, ModelError> {
        loop {
            let notified = self.shared.available.notified();
            tokio::pin!(notified);
            // Register before inspecting lanes to avoid a release/wait race.
            notified.as_mut().enable();
            check_cancelled(options.signal.as_ref())?;
            if let Some(reservation) = self.try_reserve(options) {
                return Ok(reservation);
            }
            if let Some(signal) = &options.signal {
                tokio::select! { biased;
                    () = signal.cancelled() => return Err(ModelError::cancelled("Transformers embedding was cancelled")),
                    () = &mut notified => {}
                }
            } else {
                notified.await;
            }
        }
    }

    pub(super) async fn run(
        &self,
        prepared: Arc<PreparedBatch>,
        entry: TransformersConfig,
        options: EmbeddingOptions,
        enqueued: Vec<Instant>,
        reservation: Option<Reservation>,
    ) -> Result<NativeOutput, ModelError> {
        let completed = self
            .dispatch(
                Arc::clone(&prepared),
                entry,
                options.clone(),
                enqueued.clone(),
                reservation,
            )
            .await?;
        match completed.result {
            Err(error)
                if completed.retry_on_cpu && error.code() != crate::EngineError::CANCELLED =>
            {
                let first_fallback = !self.shared.fallback.swap(true, Ordering::AcqRel);
                lock_std_mutex(&self.shared.stats).actual_provider = "cpu";
                // New work drains all GPU jobs, then converts the entire fixed
                // pool to CPU before running any fallback inference.
                if first_fallback {
                    let warning = format!(
                        "Transformers {} embedding inference failed ({}), falling back to CPU.",
                        self.requested.name(),
                        error
                    );
                    if let Some(progress) = &options.on_progress {
                        progress(ModelProgress::Warning {
                            model_ref: entry.reference.to_owned(),
                            message: warning,
                        });
                    } else {
                        tracing::warn!("{warning}");
                    }
                }
                self.shared.available.notify_waiters();
                let enqueued = if completed.started {
                    Vec::new()
                } else {
                    enqueued
                };
                self.dispatch(prepared, entry, options, enqueued, None)
                    .await?
                    .result
                    .map_err(ModelError::shared)
            }
            result => result,
        }
    }

    async fn dispatch(
        &self,
        prepared: Arc<PreparedBatch>,
        entry: TransformersConfig,
        options: EmbeddingOptions,
        enqueued: Vec<Instant>,
        reservation: Option<Reservation>,
    ) -> Result<NativeCompletion, ModelError> {
        let Reservation {
            lane,
            permit,
            cpu_threads,
            transition,
        } = if let Some(reservation) = reservation {
            reservation
        } else {
            self.reserve(&options).await?
        };
        // Initialization happens on the native worker, never on a Tokio thread.
        drop(self.start_worker(lane)?);
        if let Some(transition) = transition {
            self.configure_pool(&transition).await?;
            transition.succeeded.store(true, Ordering::Release);
            // Publish the new uniform CPU budget before admitting other jobs.
            drop(transition);
        }
        let (reply, receiver) = oneshot::channel();
        let job = Job {
            prepared,
            entry,
            options,
            enqueued,
            reply,
            _permit: permit,
            cpu_threads,
        };
        lock_std_mutex(&self.workers[lane])
            .as_ref()
            .expect("worker started")
            .send(Message::Run(Box::new(job)))
            .map_err(|_| ModelError::internal("ONNX native worker stopped").shared())?;
        receiver.await.map_err(|error| {
            ModelError::internal("ONNX native worker stopped")
                .with_cause(error)
                .shared()
        })
    }

    async fn configure_pool(&self, transition: &Arc<PoolTransition>) -> Result<(), ModelError> {
        let mut replies = Vec::with_capacity(self.workers.len());
        for lane in 0..self.workers.len() {
            drop(self.start_worker(lane)?);
            let (reply, receiver) = oneshot::channel();
            lock_std_mutex(&self.workers[lane])
                .as_ref()
                .expect("worker started")
                .send(Message::Configure {
                    threads: transition.threads,
                    transition: Arc::clone(transition),
                    reply,
                })
                .map_err(|_| ModelError::internal("ONNX configuration worker stopped").shared())?;
            replies.push(receiver);
        }
        let mut failure = None;
        for reply in replies {
            if let Err(error) = reply
                .await
                .map_err(|error| {
                    ModelError::internal("ONNX pool configuration stopped").with_cause(error)
                })
                .and_then(std::convert::identity)
            {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error.shared());
        }
        lock_std_mutex(&self.shared.stats).pool_reconfigurations += 1;
        tracing::debug!(
            sessions = self.workers.len(),
            threads_per_session = transition.threads,
            actual_thread_budget = self.workers.len().saturating_mul(transition.threads),
            "Configured ORT CPU session pool"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn configuration(&self) -> (usize, usize) {
        (
            self.stats().cpu_threads.max(1),
            self.shared.generation.load(Ordering::Acquire),
        )
    }

    #[cfg(test)]
    pub(super) fn largest_batch(&self) -> usize {
        self.stats().max_native_batch_rows
    }
}

impl Shared {
    fn new(provider: TransformersExecutionProvider, sessions: usize, budget: usize) -> Self {
        Self {
            fallback: AtomicBool::new(false),
            available: Arc::new(Notify::new()),
            stats: StdMutex::new(ModelExecutionStats {
                actual_provider: provider.name(),
                ..ModelExecutionStats::default()
            }),
            inflight: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
            pool: StdMutex::new(PoolState {
                busy: vec![None; sessions],
                cpu_threads: (provider == TransformersExecutionProvider::Cpu)
                    .then(|| policy::threads_per_session(budget, sessions)),
                changing: false,
                drain: false,
            }),
        }
    }
}

impl Worker {
    fn send(&self, message: Message) -> Result<(), mpsc::SendError<Message>> {
        self.sender.as_ref().expect("worker is alive").send(message)
    }

    fn start(
        lane: usize,
        requested: TransformersExecutionProvider,
        factory: Arc<Factory>,
        shared: Arc<Shared>,
        ready: mpsc::Sender<Result<(), ModelError>>,
    ) -> Result<Self, ModelError> {
        let (sender, receiver) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name(format!("zg-ort-{lane}"))
            .spawn(move || {
                let mut runner = factory(lane);
                let _ = ready.send(runner.as_ref().map(|_| ()).map_err(Clone::clone));
                for message in receiver {
                    let job = match message {
                        Message::Run(job) => job,
                        Message::Configure {
                            threads,
                            transition,
                            reply,
                        } => {
                            let attempted = catch_unwind(AssertUnwindSafe(|| {
                                if runner.is_err() {
                                    runner = factory(lane);
                                }
                                runner
                                    .as_mut()
                                    .map_err(|error| error.clone())?
                                    .configure(threads, true)
                            }));
                            let result = if let Ok(result) = attempted {
                                result
                            } else {
                                drop(runner);
                                runner = factory(lane);
                                Err(ModelError::internal("ONNX configuration worker panicked")
                                    .shared())
                            };
                            // Release native ownership before waking the async
                            // waiter, including during cancellation/shutdown.
                            drop(transition);
                            let _ = reply.send(result);
                            continue;
                        }
                    };
                    let Job {
                        prepared,
                        entry,
                        options,
                        enqueued,
                        reply,
                        _permit: permit,
                        cpu_threads,
                    } = *job;
                    let force_cpu = cpu_threads.is_some();
                    let retry_on_cpu =
                        requested != TransformersExecutionProvider::Cpu && !force_cpu;
                    let mut native_started = false;
                    let result = if shared.fallback.load(Ordering::Acquire) && !force_cpu {
                        Err(ModelError::internal("ONNX GPU pool is falling back to CPU").shared())
                    } else if let Err(error) = check_cancelled(options.signal.as_ref()) {
                        Err(error)
                    } else {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            let runner = runner.as_mut().map_err(|error| error.clone())?;
                            runner.configure(cpu_threads.unwrap_or(1), force_cpu)?;
                            check_cancelled(options.signal.as_ref())?;
                            let _measurement =
                                NativeMeasurement::start(&shared, &prepared, &enqueued);
                            native_started = true;
                            runner.run(&prepared, entry, &options, force_cpu)
                        }));
                        if let Ok(result) = result {
                            result
                        } else {
                            // Destroy the suspect Session on its owner thread.
                            drop(runner);
                            runner = factory(lane);
                            Err(ModelError::internal("ONNX native worker panicked").shared())
                        }
                    };
                    // Lease and permit live through native completion even if the
                    // receiver, request task, or model owner has been dropped.
                    // Release them before acknowledging completion: the final
                    // lease may destroy this owner on its own native thread.
                    drop(prepared);
                    drop(options);
                    drop(permit);
                    if worker_stopping.load(Ordering::Acquire) {
                        // Self-join is impossible. Complete Session destruction
                        // here before waking a caller that may exit the process.
                        drop(runner);
                        runner = Err(ModelError::internal("ONNX native worker stopped").shared());
                    }
                    let _ = reply.send(NativeCompletion {
                        result,
                        retry_on_cpu,
                        started: native_started,
                    });
                }
            })
            .map_err(|error| {
                ModelError::internal("Unable to start ONNX native worker").with_cause(error)
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            stopping,
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // Disconnect the queue before joining so an idle worker can terminate.
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() == std::thread::current().id() {
                // Releasing the final runtime lease inside a native job can
                // destroy this owner. That worker drops its Session before reply.
                return;
            }
            if thread.join().is_err() {
                tracing::warn!("ONNX native worker panicked during shutdown");
            }
        }
    }
}

impl SessionState {
    fn clear(&mut self) {
        if self.session.take().is_some() {
            lock_std_mutex(&self.shared.stats).session_count -= 1;
        }
    }

    fn ensure_session(&mut self) -> Result<(), ModelError> {
        if self.session.is_none() {
            self.session = Some(load_session(&self.model_path, self.provider, self.threads)?);
            let mut stats = lock_std_mutex(&self.shared.stats);
            stats.session_count += 1;
            stats.actual_provider = if self.shared.fallback.load(Ordering::Acquire) {
                "cpu"
            } else {
                self.provider.name()
            };
            if self.lane == 0 {
                stats.cpu_threads = self.threads;
                stats.cpu_threads_total = self
                    .threads
                    .saturating_mul(lock_std_mutex(&self.shared.pool).busy.len());
                self.shared.generation.fetch_add(1, Ordering::Release);
            }
        }
        Ok(())
    }
}

impl Runner for SessionState {
    fn configure(&mut self, threads: usize, force_cpu: bool) -> Result<(), ModelError> {
        if force_cpu && self.provider != TransformersExecutionProvider::Cpu {
            self.clear();
            self.provider = TransformersExecutionProvider::Cpu;
        }
        if self.threads != threads {
            self.clear();
            self.threads = threads;
        }
        self.ensure_session()
    }

    fn run(
        &mut self,
        prepared: &PreparedBatch,
        _entry: TransformersConfig,
        options: &EmbeddingOptions,
        _force_cpu: bool,
    ) -> Result<NativeOutput, ModelError> {
        run_session(
            self.session.as_mut().expect("session initialized"),
            prepared,
            options.signal.as_ref(),
        )
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.clear();
    }
}

fn nanoseconds(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

struct NativeMeasurement<'a> {
    shared: &'a Shared,
    started: Instant,
}

impl<'a> NativeMeasurement<'a> {
    fn start(shared: &'a Shared, prepared: &PreparedBatch, enqueued: &[Instant]) -> Self {
        let inflight = shared.inflight.fetch_add(1, Ordering::AcqRel) + 1;
        let mut stats = lock_std_mutex(&shared.stats);
        stats.native_inflight_peak = stats.native_inflight_peak.max(inflight);
        for queued in enqueued {
            stats.backend_to_native_ns = stats
                .backend_to_native_ns
                .saturating_add(nanoseconds(queued.elapsed()));
        }
        stats.native_runs += 1;
        stats.native_rows += prepared.batch_size as u64;
        let tokens = prepared.batch_size.saturating_mul(prepared.sequence_length);
        stats.padded_tokens += tokens as u64;
        stats.max_native_batch_rows = stats.max_native_batch_rows.max(prepared.batch_size);
        stats.max_padded_tokens = stats.max_padded_tokens.max(tokens);
        Self {
            shared,
            started: Instant::now(),
        }
    }
}

impl Drop for NativeMeasurement<'_> {
    fn drop(&mut self) {
        self.shared.inflight.fetch_sub(1, Ordering::AcqRel);
        let mut stats = lock_std_mutex(&self.shared.stats);
        stats.inference_ns = stats
            .inference_ns
            .saturating_add(nanoseconds(self.started.elapsed()));
    }
}

#[cfg(test)]
mod tests;
