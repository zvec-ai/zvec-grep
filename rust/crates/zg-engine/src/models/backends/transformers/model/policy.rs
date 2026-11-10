//! Fixed execution pools and bounded request admission.
use super::TransformersExecutionProvider;

pub(super) fn pool_size(provider: TransformersExecutionProvider, configured: usize) -> usize {
    match provider {
        TransformersExecutionProvider::CoreMl => 1,
        _ => configured,
    }
}

/// Bound preparation and queued work by pool capacity, independently of each
/// operation's execution concurrency.
pub(super) fn admission(provider: TransformersExecutionProvider, sessions: usize) -> usize {
    if provider == TransformersExecutionProvider::Cuda {
        sessions.saturating_add(1).saturating_mul(4)
    } else {
        sessions.saturating_add(2)
    }
}

pub(super) fn execution_limit(
    provider: TransformersExecutionProvider,
    requested: usize,
    sessions: usize,
) -> usize {
    match provider {
        TransformersExecutionProvider::CoreMl => 1,
        _ => requested.clamp(1, sessions),
    }
}

#[cfg(test)]
mod tests;
