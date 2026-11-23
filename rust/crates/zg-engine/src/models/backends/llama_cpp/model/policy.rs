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
}

impl LlamaExecutionBudget {
    pub(super) fn resolve(
        backend: LlamaExecutionBackend,
        requested: usize,
        resources: &LlamaResources,
    ) -> Self {
        Self {
            contexts: requested.max(1).min(resources.contexts),
            threads: if backend.is_gpu() {
                1
            } else {
                resources.cpu_threads
            },
        }
    }
}

#[cfg(test)]
mod tests;
