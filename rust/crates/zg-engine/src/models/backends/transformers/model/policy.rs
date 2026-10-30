//! Internal budgets: user concurrency never multiplies native sessions.
use std::{num::NonZeroUsize, time::Duration};

use super::{TransformersConfig, TransformersExecutionProvider};

const MINILM_MAX_ROWS: usize = 16;
const MINILM_PADDED_TOKENS: usize = 1536;
const BATCH_TARGET: Duration = Duration::from_millis(20);

pub(super) fn cpu_threads(requested: usize) -> usize {
    let available = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let physical = sysinfo::System::physical_core_count().unwrap_or(available);
    requested.clamp(
        1,
        available
            .min(physical)
            .clamp(1, crate::models::runtime::LOCAL_CONCURRENCY_CAP),
    )
}

pub(super) fn cuda_rows(entry: TransformersConfig) -> usize {
    // Larger shapes have only been validated for this catalog model. Other
    // models retain their existing batch ceiling until separately measured.
    if entry.reference == "local/all-minilm-l6-v2" {
        MINILM_MAX_ROWS
    } else {
        entry.max_batch_size
    }
}

pub(super) fn execution_limit(
    entry: TransformersConfig,
    provider: TransformersExecutionProvider,
    requested: usize,
) -> usize {
    match provider {
        TransformersExecutionProvider::Cpu => cpu_threads(requested),
        TransformersExecutionProvider::Cuda => {
            requested.clamp(1, (cuda_rows(entry) / entry.max_batch_size).max(1))
        }
        _ => 1,
    }
}

pub(super) struct BatchPolicy {
    padded_tokens: usize,
    observations: usize,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self {
            padded_tokens: MINILM_PADDED_TOKENS,
            observations: 0,
        }
    }
}

impl BatchPolicy {
    pub(super) fn request_limit(
        entry: TransformersConfig,
        concurrency: usize,
        interactive: bool,
    ) -> usize {
        let maximum = execution_limit(entry, TransformersExecutionProvider::Cuda, concurrency);
        maximum.min(if interactive { 2 } else { maximum })
    }

    pub(super) fn fits(
        &self,
        entry: TransformersConfig,
        rows: usize,
        width: usize,
        interactive: bool,
    ) -> bool {
        let tokens = if interactive {
            self.padded_tokens / 2
        } else {
            self.padded_tokens
        };
        rows <= cuda_rows(entry) && rows.saturating_mul(width) <= tokens
    }

    pub(super) fn observe(
        &mut self,
        rows: usize,
        width: usize,
        elapsed: Duration,
        base_rows: usize,
    ) {
        self.observations = self.observations.saturating_add(1);
        // Ignore initial provider warm-up. A slow merged batch reduces future
        // work, while a single caller's base batch is always allowed through.
        if self.observations > 3 && rows > base_rows && elapsed > BATCH_TARGET {
            self.padded_tokens = self
                .padded_tokens
                .min((rows / 2).max(base_rows).saturating_mul(width));
        }
    }
}

#[cfg(test)]
mod tests;
