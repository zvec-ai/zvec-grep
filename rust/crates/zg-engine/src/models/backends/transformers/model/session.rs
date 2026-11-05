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
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

use super::{
    Arc, Device, EmbeddingOptions, EmbeddingPurpose, ModelDownloadProgressReporter, ModelError,
    ModelProgress, NativeOutput, PathBuf, PreparedBatch, Session, StdMutex, TransformersConfig,
    TransformersExecutionProvider, check_cancelled, load_session, lock_std_mutex, policy,
    resolve_execution_provider, run_session,
};
use crate::models::spi::ModelExecutionStats;

pub(super) struct SessionOwner {
    workers: [StdMutex<Option<Worker>>; policy::CUDA_LANES],
    slots: [Arc<Semaphore>; policy::CUDA_LANES],
    factory: Arc<Factory>,
    shared: Arc<Shared>,
    requested: TransformersExecutionProvider,
}

type Factory = dyn Fn(usize) -> Result<Box<dyn Runner>, ModelError> + Send + Sync;

trait Runner {
    fn configure(
        &mut self,
        _options: &EmbeddingOptions,
        _force_cpu: bool,
    ) -> Result<(), ModelError> {
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
}

struct Worker {
    sender: Option<mpsc::Sender<Message>>,
    thread: Option<std::thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

enum Message {
    Run(Box<Job>),
    Retire,
}

struct Job {
    prepared: Arc<PreparedBatch>,
    entry: TransformersConfig,
    options: EmbeddingOptions,
    enqueued: Vec<Instant>,
    reply: oneshot::Sender<NativeCompletion>,
    _permit: LanePermit,
}

struct NativeCompletion {
    result: Result<NativeOutput, ModelError>,
    retry_on_cpu: bool,
    started: bool,
}

struct LanePermit {
    permit: Option<OwnedSemaphorePermit>,
    available: Arc<Notify>,
}

pub(super) struct Reservation {
    lane: usize,
    permit: LanePermit,
}

impl Drop for LanePermit {
    fn drop(&mut self) {
        // Notify after releasing the lane, so a woken waiter can reserve it.
        drop(self.permit.take());
        self.available.notify_waiters();
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
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<Self, ModelError> {
        let requested = resolve_execution_provider(device);
        if cfg!(target_os = "macos") && device == Some(Device::Metal) {
            let warning = "Metal was requested for the Transformers ONNX model, but the available CoreML path regresses throughput and memory; using ORT CPU instead.";
            if !reporter.warning(warning) {
                tracing::warn!("{warning}");
            }
        }
        let shared = Arc::new(Shared::new(requested));
        let worker_shared = Arc::clone(&shared);
        let reporter = reporter.clone();
        let factory: Arc<Factory> = Arc::new(move |lane| {
            let fallback = worker_shared.fallback.load(Ordering::Acquire);
            if lane != 0 && fallback {
                return Err(ModelError::internal("ONNX GPU lane retired").shared());
            }
            let provider = if fallback {
                TransformersExecutionProvider::Cpu
            } else {
                requested
            };
            let threads = if provider == TransformersExecutionProvider::Cpu {
                policy::cpu_threads(concurrency)
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
            if let Err(error) = state.ensure_session() {
                if lane != 0 || provider == TransformersExecutionProvider::Cpu {
                    return Err(error);
                }
                let warning = format!(
                    "Transformers {} embedding initialization failed ({}), falling back to CPU.",
                    provider.name(),
                    error
                );
                if !reporter.warning(warning.clone()) {
                    tracing::warn!("{warning}");
                }
                worker_shared.fallback.store(true, Ordering::Release);
                state.provider = TransformersExecutionProvider::Cpu;
                state.threads = policy::cpu_threads(concurrency);
                state.ensure_session()?;
            }
            Ok(Box::new(state))
        });
        let owner = Self::new(requested, shared, factory);
        owner.start_worker(0)?.recv().map_err(|error| {
            ModelError::internal("ONNX worker initialization stopped").with_cause(error)
        })??;
        Ok(owner)
    }

    fn new(
        requested: TransformersExecutionProvider,
        shared: Arc<Shared>,
        factory: Arc<Factory>,
    ) -> Self {
        Self {
            workers: std::array::from_fn(|_| StdMutex::new(None)),
            slots: std::array::from_fn(|_| Arc::new(Semaphore::new(1))),
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
        let lanes = if self.provider() == TransformersExecutionProvider::Cuda {
            options.execution_concurrency.clamp(1, policy::CUDA_LANES)
        } else {
            1
        };
        for lane in 0..lanes {
            if let Ok(permit) = Arc::clone(&self.slots[lane]).try_acquire_owned() {
                return Some(Reservation {
                    lane,
                    permit: LanePermit {
                        permit: Some(permit),
                        available: Arc::clone(&self.shared.available),
                    },
                });
            }
        }
        None
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
                // Lane 1 finishes any owned GPU job, then destroys its Session.
                // Only lane 0 may create a CPU fallback Session.
                if let Some(worker) = &*lock_std_mutex(&self.workers[1]) {
                    let _ = worker.send(Message::Retire);
                }
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
        let Reservation { lane, permit } = if let Some(reservation) = reservation {
            reservation
        } else {
            self.reserve(&options).await?
        };
        // Initialization happens on the native worker, never on a Tokio thread.
        drop(self.start_worker(lane)?);
        let (reply, receiver) = oneshot::channel();
        let job = Job {
            prepared,
            entry,
            options,
            enqueued,
            reply,
            _permit: permit,
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
    fn new(provider: TransformersExecutionProvider) -> Self {
        Self {
            fallback: AtomicBool::new(false),
            available: Arc::new(Notify::new()),
            stats: StdMutex::new(ModelExecutionStats {
                actual_provider: provider.name(),
                ..ModelExecutionStats::default()
            }),
            inflight: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
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
                    let Message::Run(job) = message else {
                        drop(runner);
                        // A retired CUDA lane stays empty until the owner is dropped.
                        runner = Err(ModelError::internal("ONNX GPU lane retired").shared());
                        continue;
                    };
                    let Job {
                        prepared,
                        entry,
                        options,
                        enqueued,
                        reply,
                        _permit: permit,
                    } = *job;
                    let force_cpu = shared.fallback.load(Ordering::Acquire);
                    let retry_on_cpu = requested != TransformersExecutionProvider::Cpu
                        && (!force_cpu || lane != 0);
                    let mut native_started = false;
                    let result = if lane != 0 && force_cpu {
                        Err(ModelError::internal("ONNX GPU lane retired").shared())
                    } else if let Err(error) = check_cancelled(options.signal.as_ref()) {
                        Err(error)
                    } else {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            let runner = runner.as_mut().map_err(|error| error.clone())?;
                            runner.configure(&options, force_cpu)?;
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
                self.shared.generation.fetch_add(1, Ordering::Release);
            }
        }
        Ok(())
    }
}

impl Runner for SessionState {
    fn configure(&mut self, options: &EmbeddingOptions, force_cpu: bool) -> Result<(), ModelError> {
        check_cancelled(options.signal.as_ref())?;
        if force_cpu && self.provider != TransformersExecutionProvider::Cpu {
            self.clear();
            self.provider = TransformersExecutionProvider::Cpu;
            self.threads = policy::cpu_threads(options.execution_concurrency);
        }
        if self.provider == TransformersExecutionProvider::Cpu
            && options.purpose == EmbeddingPurpose::Document
        {
            let threads = policy::cpu_threads(options.execution_concurrency);
            if self.threads != threads {
                self.clear();
                self.threads = threads;
            }
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
            stats.queue_wait_ns = stats
                .queue_wait_ns
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
