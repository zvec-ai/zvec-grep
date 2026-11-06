//! CPU thread partitioning and independent GPU context/batching budgets.
use super::ModelError;

pub(super) const CPU_CONTEXTS_ENV: &str = "ZVEC_GREP_LLAMA_CPU_CONTEXTS";

pub(super) fn configured_cpu_contexts() -> Result<usize, String> {
    match std::env::var(CPU_CONTEXTS_ENV) {
        Ok(value) => parse_cpu_contexts(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(1),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{CPU_CONTEXTS_ENV} must be a positive integer"))
        }
    }
}

fn parse_cpu_contexts(value: Option<&str>) -> Result<usize, String> {
    let value = value.unwrap_or("1").trim();
    if !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(contexts) = value.parse::<usize>()
        && contexts > 0
    {
        Ok(contexts)
    } else {
        Err(format!("{CPU_CONTEXTS_ENV} must be a positive integer"))
    }
}

pub(super) fn default_cpu_concurrency(contexts: usize) -> usize {
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    automatic_cpu_concurrency(available, contexts)
}

fn automatic_cpu_concurrency(available: usize, contexts: usize) -> usize {
    contexts * (available.clamp(1, 4) / contexts).max(1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LlamaExecutionBudget {
    pub(super) contexts: usize,
    pub(super) threads: i32,
    pub(super) batch_tokens: Option<usize>,
}

impl LlamaExecutionBudget {
    pub(super) fn resolve(
        architecture: &str,
        gpu: bool,
        concurrency: usize,
        cpu_contexts: usize,
    ) -> Result<Self, ModelError> {
        if gpu {
            // Packing is independent of C. First form useful physical batches,
            // then distribute those batches across the shared context pool.
            let batch_tokens = match architecture {
                "gemma-embedding" => Some(128),
                "qwen3" => Some(256),
                _ => None,
            };
            Ok(Self {
                contexts: concurrency.max(1),
                threads: 1,
                batch_tokens,
            })
        } else {
            // Preserve automatic selection until the actual device is known,
            // including a GPU request that falls back to CPU after loading.
            if cpu_contexts == 0 {
                return Err(ModelError::invalid_argument(format!(
                    "{CPU_CONTEXTS_ENV} must be greater than zero"
                ))
                .shared());
            }
            let total_threads = if concurrency == 0 {
                default_cpu_concurrency(cpu_contexts)
            } else {
                concurrency
            };
            if total_threads < cpu_contexts || total_threads % cpu_contexts != 0 {
                return Err(ModelError::invalid_argument(format!(
                    "llama.cpp CPU concurrency must be at least {cpu_contexts} and divisible by \
                     {CPU_CONTEXTS_ENV}={cpu_contexts}"
                ))
                .shared());
            }
            let threads = total_threads / cpu_contexts;
            let threads = i32::try_from(threads).map_err(|error| {
                ModelError::invalid_argument("llama.cpp thread count exceeds the native limit")
                    .with_cause(error)
                    .shared()
            })?;
            // Every CPU job keeps the validated single-sequence path. The
            // model's K contexts share C total native compute threads.
            Ok(Self {
                contexts: cpu_contexts,
                threads,
                batch_tokens: None,
            })
        }
    }
}

#[cfg(test)]
mod tests;
