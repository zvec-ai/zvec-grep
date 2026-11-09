//! Fixed execution pools and bounded native batch shapes.
use super::{TransformersConfig, TransformersExecutionProvider};
use std::time::Duration;

pub(super) const BATCH_WAIT: Duration = Duration::from_millis(1);

pub(super) fn pool_size(provider: TransformersExecutionProvider, configured: usize) -> usize {
    match provider {
        TransformersExecutionProvider::CoreMl => 1,
        _ => configured,
    }
}

/// Running batches plus one prepared batch per pool, independent of C.
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

pub(super) struct BatchPolicy;

impl BatchPolicy {
    pub(super) fn max_requests(entry: TransformersConfig) -> usize {
        if entry.reference == "local/all-minilm-l6-v2" {
            4
        } else {
            1
        }
    }

    pub(super) fn max_rows(entry: TransformersConfig) -> usize {
        // Coalescing shares the model's batch budget; it must not multiply
        // per-session activation memory by the number of admitted requests.
        entry.max_batch_size
    }

    pub(super) fn fits(entry: TransformersConfig, rows: usize, width: usize) -> bool {
        let token_budget = entry.max_batch_size.saturating_mul(entry.max_input_tokens);
        rows <= Self::max_rows(entry) && rows.saturating_mul(width) <= token_budget
    }

    pub(super) fn length_bucket(width: usize) -> usize {
        width
            .max(1)
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests;
