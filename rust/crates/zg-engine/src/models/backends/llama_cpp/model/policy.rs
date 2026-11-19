//! Context concurrency and CPU threads are independent on every device.
use crate::models::runtime::llama_config::LlamaResources;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LlamaExecutionBackend {
    Cpu,
    Cuda,
    OtherGpu,
}

impl LlamaExecutionBackend {
    pub(super) fn from_gpu_device(backend: &str, name: &str) -> Self {
        if backend.to_ascii_lowercase().contains("cuda")
            || name.to_ascii_lowercase().contains("cuda")
        {
            Self::Cuda
        } else {
            Self::OtherGpu
        }
    }

    pub(super) const fn is_gpu(self) -> bool {
        !matches!(self, Self::Cpu)
    }
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
        backend: LlamaExecutionBackend,
        requested: usize,
        resources: &LlamaResources,
    ) -> Self {
        let batch_tokens = if backend == LlamaExecutionBackend::OtherGpu {
            match architecture {
                "gemma-embedding" => Some(128),
                "qwen3" => Some(256),
                _ => None,
            }
        } else {
            // CUDA multi-sequence packing did not pass T4 equivalence checks.
            None
        };
        Self {
            contexts: requested.max(1).min(resources.contexts),
            threads: if backend.is_gpu() {
                1
            } else {
                resources.cpu_threads
            },
            batch_tokens,
        }
    }
}

#[cfg(test)]
mod tests;
