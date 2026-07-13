use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{EngineError, Result};

static NO_PROGRESS: () = ();

/// Cooperative cancellation shared by a caller and a running operation.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Structured progress emitted by long-running engine operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Scanning,
    Indexing { completed: usize, total: usize },
    Embedding { segments: usize },
    Persisting { files: usize },
    Searching { completed: usize, total: usize },
    Complete,
}

pub trait ProgressSink: Send + Sync {
    fn report(&self, event: ProgressEvent);
}

impl<F> ProgressSink for F
where
    F: Fn(ProgressEvent) + Send + Sync,
{
    fn report(&self, event: ProgressEvent) {
        self(event);
    }
}

/// Controls and observes a single engine operation.
pub struct OperationContext<'a> {
    pub cancellation: &'a CancellationToken,
    pub progress: &'a dyn ProgressSink,
}

impl<'a> OperationContext<'a> {
    pub fn new(cancellation: &'a CancellationToken, progress: &'a dyn ProgressSink) -> Self {
        Self {
            cancellation,
            progress,
        }
    }

    pub fn quiet(cancellation: &'a CancellationToken) -> Self {
        Self::new(cancellation, &NO_PROGRESS)
    }
}

impl ProgressSink for () {
    fn report(&self, _event: ProgressEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_between_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(EngineError::Cancelled)));
    }
}
