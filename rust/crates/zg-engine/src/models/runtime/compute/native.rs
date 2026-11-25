//! Track native ownership on the existing compute pool through runtime teardown.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, PoisonError},
};

use rayon::ThreadPool;
use tokio::{runtime::Handle, sync::oneshot};

use crate::models::spi::ModelError;

pub(super) async fn run<F, T>(pool: &ThreadPool, task: F) -> Result<T, ModelError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let runtime = Handle::try_current().map_err(|error| {
        ModelError::internal("Native model work requires an async runtime").with_cause(error)
    })?;
    let state = Arc::new(Completion::default());
    let (completed, finished) = oneshot::channel();
    let completion = CompleteOnDrop {
        state: Arc::clone(&state),
        _completed: completed,
    };
    let (sender, receiver) = oneshot::channel();
    // Enqueue before attaching the guard: an already-shutting-down runtime may
    // drop a spawned future synchronously. Its native job must be able to finish.
    pool.spawn(move || {
        let _completion = completion;
        // If the requesting future disappeared, destroy the native result here
        // before announcing completion. This also covers task unwinding.
        let result = catch_unwind(AssertUnwindSafe(task))
            .map_err(|_| ModelError::internal("Native model compute worker panicked"));
        drop(sender.send(result));
    });
    let shutdown = WaitOnDrop(state);
    runtime.spawn(async move {
        let _shutdown = shutdown;
        let _ = finished.await;
    });
    receiver.await.map_err(|error| {
        ModelError::internal("Native model compute worker failed").with_cause(error)
    })?
}

#[derive(Default)]
struct Completion {
    finished: Mutex<bool>,
    changed: Condvar,
}

struct CompleteOnDrop {
    state: Arc<Completion>,
    _completed: oneshot::Sender<()>,
}

impl Drop for CompleteOnDrop {
    fn drop(&mut self) {
        *self
            .state
            .finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.state.changed.notify_all();
    }
}

struct WaitOnDrop(Arc<Completion>);

impl Drop for WaitOnDrop {
    fn drop(&mut self) {
        let finished = self
            .0
            .finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Normal completion has already set the flag. Only runtime teardown
        // waits for outstanding native work; request cancellation stays async.
        drop(
            self.0
                .changed
                .wait_while(finished, |finished| !*finished)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

#[cfg(test)]
mod tests;
