//! Independent CPU compute and GPU sequence-packing budgets.
use super::ModelError;

pub(super) fn default_cpu_concurrency() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get().min(4))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LlamaExecutionBudget {
    pub(super) threads: i32,
    pub(super) batch_tokens: Option<usize>,
}

impl LlamaExecutionBudget {
    pub(super) fn resolve(
        architecture: &str,
        gpu: bool,
        concurrency: usize,
    ) -> Result<Self, ModelError> {
        if gpu {
            // These conservative tiers bound packing, not the length of a
            // single input. CPU support work does not scale with the GPU tier.
            let tier = concurrency.clamp(1, 8);
            let batch_tokens = match architecture {
                "gemma-embedding" => Some(128 * tier),
                "qwen3" => Some(256 * tier),
                _ => None,
            };
            Ok(Self {
                threads: 1,
                batch_tokens,
            })
        } else {
            // Preserve automatic selection until the actual device is known,
            // including a GPU request that falls back to CPU after loading.
            let threads = if concurrency == 0 {
                default_cpu_concurrency()
            } else {
                concurrency
            };
            let threads = i32::try_from(threads).map_err(|error| {
                ModelError::invalid_argument("llama.cpp thread count exceeds the native limit")
                    .with_cause(error)
            })?;
            // Keep the numerically validated single-sequence CPU path. C scales
            // its compute threads, never the number of contexts or batch tasks.
            Ok(Self {
                threads,
                batch_tokens: None,
            })
        }
    }
}

#[cfg(test)]
mod tests;
