//! Exactly one live native session, including provider and thread-budget changes.
use super::{
    Device, EmbeddingOptions, EmbeddingPurpose, EmbeddingResult, ModelDownloadProgressReporter,
    ModelError, ModelProgress, PathBuf, PreparedBatch, Session, StdMutex, TransformersConfig,
    TransformersExecutionProvider, check_cancelled, load_session, lock_std_mutex, policy,
    resolve_execution_provider, run_session,
};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct SessionOwner {
    model_path: PathBuf,
    state: StdMutex<SessionState>,
    initial_provider: TransformersExecutionProvider,
    cpu_fallback: AtomicBool,
}

struct SessionState {
    session: Option<Session>,
    provider: TransformersExecutionProvider,
    threads: usize,
    generation: usize,
    #[cfg(test)]
    largest_batch: usize,
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
        let threads = if requested == TransformersExecutionProvider::Cpu {
            policy::cpu_threads(concurrency)
        } else {
            1
        };
        let (session, provider, threads) = match load_session(&model_path, requested, threads) {
            Ok(session) => (session, requested, threads),
            Err(error) if requested != TransformersExecutionProvider::Cpu => {
                let warning = format!(
                    "Transformers {} embedding initialization failed ({}), falling back to CPU.",
                    requested.name(),
                    error
                );
                if !reporter.warning(warning.clone()) {
                    tracing::warn!("{warning}");
                }
                let threads = policy::cpu_threads(concurrency);
                (
                    load_session(&model_path, TransformersExecutionProvider::Cpu, threads)?,
                    TransformersExecutionProvider::Cpu,
                    threads,
                )
            }
            Err(error) => return Err(error),
        };
        tracing::debug!(
            provider = provider.name(),
            threads,
            "Transformers ONNX {} embedding execution provider selected",
            provider.name()
        );
        Ok(Self {
            model_path,
            initial_provider: provider,
            cpu_fallback: AtomicBool::new(false),
            state: StdMutex::new(SessionState {
                session: Some(session),
                provider,
                threads,
                generation: 1,
                #[cfg(test)]
                largest_batch: 0,
            }),
        })
    }

    pub(super) fn provider(&self) -> TransformersExecutionProvider {
        if self.cpu_fallback.load(Ordering::Acquire) {
            TransformersExecutionProvider::Cpu
        } else {
            self.initial_provider
        }
    }

    pub(super) fn run(
        &self,
        prepared: &PreparedBatch,
        entry: TransformersConfig,
        options: &EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        let mut state = lock_std_mutex(&self.state);
        check_cancelled(options.signal.as_ref())?;
        if state.provider == TransformersExecutionProvider::Cpu
            && options.purpose == EmbeddingPurpose::Document
        {
            let threads = policy::cpu_threads(options.execution_concurrency);
            if state.threads != threads {
                // Destroy first: loading the replacement must not briefly double
                // model memory. Only the queue worker runs native inference.
                state.session.take();
                state.threads = threads;
            }
        }
        self.ensure_session(&mut state)?;
        #[cfg(test)]
        {
            state.largest_batch = state.largest_batch.max(prepared.batch_size);
        }
        let result = run_session(
            state.session.as_mut().expect("session initialized"),
            prepared,
            entry,
            options.signal.as_ref(),
        );
        match result {
            Err(error)
                if state.provider != TransformersExecutionProvider::Cpu
                    && error.code() != crate::EngineError::CANCELLED =>
            {
                let warning = format!(
                    "Transformers {} embedding inference failed ({}), falling back to CPU.",
                    state.provider.name(),
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
                state.session.take();
                state.provider = TransformersExecutionProvider::Cpu;
                self.cpu_fallback.store(true, Ordering::Release);
                state.threads = policy::cpu_threads(options.execution_concurrency);
                self.ensure_session(&mut state)?;
                run_session(
                    state.session.as_mut().expect("CPU session initialized"),
                    prepared,
                    entry,
                    options.signal.as_ref(),
                )
                .map_err(ModelError::shared)
            }
            result => result,
        }
    }

    fn ensure_session(&self, state: &mut SessionState) -> Result<(), ModelError> {
        if state.session.is_none() {
            state.session = Some(load_session(
                &self.model_path,
                state.provider,
                state.threads,
            )?);
            state.generation += 1;
            tracing::debug!(
                provider = state.provider.name(),
                threads = state.threads,
                generation = state.generation,
                "Transformers ONNX session replaced"
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn configuration(&self) -> (usize, usize) {
        let state = lock_std_mutex(&self.state);
        (state.threads, state.generation)
    }

    #[cfg(test)]
    pub(super) fn largest_batch(&self) -> usize {
        lock_std_mutex(&self.state).largest_batch
    }
}
