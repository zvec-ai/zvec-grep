//! Execution lanes, CPU threads and native batch shapes are separate budgets.
use super::{TransformersConfig, TransformersExecutionProvider};
use std::{num::NonZeroUsize, time::Duration};

pub(super) const CUDA_LANES: usize = 2;
pub(super) const BATCH_WAIT: Duration = Duration::from_millis(1);

pub(super) fn hardware_threads() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

pub(super) fn cpu_threads(requested: usize) -> usize {
    requested.clamp(1, hardware_threads())
}

pub(super) fn execution_limit(
    _entry: TransformersConfig,
    provider: TransformersExecutionProvider,
    requested: usize,
) -> usize {
    match provider {
        TransformersExecutionProvider::Cpu => cpu_threads(requested),
        TransformersExecutionProvider::Cuda => requested.clamp(1, CUDA_LANES),
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
        if entry.reference == "local/all-minilm-l6-v2" {
            16
        } else {
            entry.max_batch_size
        }
    }

    pub(super) fn fits(entry: TransformersConfig, rows: usize, width: usize) -> bool {
        let token_budget = if entry.reference == "local/all-minilm-l6-v2" {
            4096
        } else {
            entry.max_batch_size.saturating_mul(entry.max_input_tokens)
        };
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
