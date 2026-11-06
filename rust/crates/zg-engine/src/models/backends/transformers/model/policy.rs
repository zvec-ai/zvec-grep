//! Execution lanes, CPU threads and native batch shapes are separate budgets.
use super::{TransformersConfig, TransformersExecutionProvider};
use std::{num::NonZeroUsize, time::Duration};

pub(super) const SESSION_ENV: &str = "ZVEC_GREP_ORT_SESSION";
pub(super) const BATCH_WAIT: Duration = Duration::from_millis(1);

pub(super) fn parse_session_count(value: Option<&str>) -> Option<usize> {
    value.map_or(Some(1), |value| {
        value
            .trim()
            .parse::<NonZeroUsize>()
            .ok()
            .map(NonZeroUsize::get)
    })
}

pub(super) fn configured_session_count() -> usize {
    match std::env::var(SESSION_ENV) {
        Ok(value) => parse_session_count(Some(&value)).unwrap_or_else(|| {
            tracing::warn!(
                environment = SESSION_ENV,
                "Expected a positive ORT session count; using 1"
            );
            1
        }),
        Err(std::env::VarError::NotPresent) => 1,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::warn!(
                environment = SESSION_ENV,
                "Invalid ORT session count; using 1"
            );
            1
        }
    }
}

pub(super) fn pool_size(provider: TransformersExecutionProvider, configured: usize) -> usize {
    match provider {
        TransformersExecutionProvider::Cpu | TransformersExecutionProvider::Cuda => configured,
        _ => 1,
    }
}

pub(super) fn hardware_threads() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

pub(super) fn cpu_threads(requested: usize) -> usize {
    requested.clamp(1, hardware_threads())
}

pub(super) fn threads_per_session(budget: usize, sessions: usize) -> usize {
    (cpu_threads(budget) / sessions).max(1)
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
        TransformersExecutionProvider::Cpu => cpu_threads(requested),
        TransformersExecutionProvider::Cuda => requested.clamp(1, sessions),
        _ => 1,
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
