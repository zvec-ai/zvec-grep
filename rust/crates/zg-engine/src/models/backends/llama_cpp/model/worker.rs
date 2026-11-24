//! Keep native worker teardown ahead of the owning async runtime's shutdown.

use std::{
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread::{self, JoinHandle},
};

use tokio::sync::oneshot;

use super::{LlamaContextWorker, LlamaWorkerMessage, ModelError, Ordering};

pub(super) fn spawn(
    run: impl FnOnce(&mpsc::Receiver<LlamaWorkerMessage>) + Send + 'static,
) -> Result<Arc<LlamaContextWorker>, ModelError> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
        ModelError::internal("llama.cpp context workers require an async runtime").with_cause(error)
    })?;
    let (sender, receiver) = mpsc::channel();
    let (completed, finished) = oneshot::channel::<()>();
    let worker = Arc::new(LlamaContextWorker {
        busy: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        sender,
    });
    let join = thread::Builder::new()
        .name("zg-llama-context".to_owned())
        .spawn(move || {
            // Close even on panic, but only after run's native contexts and
            // captured model/resources have been destroyed.
            let _completed = completed;
            run(&receiver);
        })
        .map_err(|error| {
            ModelError::internal("Unable to create llama.cpp context worker").with_cause(error)
        })?;
    // Construct outside the future: cancellation before its first poll must
    // still stop and join the native thread. Never run this guard on that thread.
    let shutdown = Shutdown {
        worker: Arc::clone(&worker),
        join: Some(join),
    };
    runtime.spawn(async move {
        let _shutdown = shutdown;
        let _ = finished.await;
    });
    Ok(worker)
}

struct Shutdown {
    worker: Arc<LlamaContextWorker>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        // An engine may outlive this runtime. Retire its old worker so a later
        // runtime cannot enqueue work behind Shutdown on the closed receiver.
        self.worker.failed.store(true, Ordering::Release);
        let _ = self.worker.sender.send(LlamaWorkerMessage::Shutdown);
        if let Some(join) = self.join.take() {
            // Normally native cleanup has already finished. Runtime teardown
            // also waits here, before CUDA's process-wide exit handlers run.
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests;
