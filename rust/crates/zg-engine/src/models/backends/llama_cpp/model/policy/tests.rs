use super::*;
use crate::models::runtime::llama_config::LlamaSettings;

#[test]
fn actual_gpu_device_selects_cuda_policy() {
    for (backend, name) in [("CUDA", "Tesla T4"), ("cuda", "NVIDIA GPU"), ("", "CUDA0")] {
        assert_eq!(
            LlamaExecutionBackend::from_gpu_device(backend, name),
            LlamaExecutionBackend::Cuda
        );
    }
    for (backend, name) in [("Metal", "Apple M4"), ("Vulkan", "NVIDIA GPU")] {
        assert_eq!(
            LlamaExecutionBackend::from_gpu_device(backend, name),
            LlamaExecutionBackend::OtherGpu
        );
    }
}

#[test]
fn requests_limit_contexts_without_changing_fixed_threads() {
    let resources = LlamaSettings {
        server_contexts: 4,
        threads: None,
        hardware_threads: 16,
    }
    .resources(4);
    for backend in [
        LlamaExecutionBackend::Cpu,
        LlamaExecutionBackend::Cuda,
        LlamaExecutionBackend::OtherGpu,
    ] {
        for requested in [0, 1, 2, 4, 8, 24] {
            let budget = LlamaExecutionBudget::resolve(backend, requested, &resources);
            assert_eq!(budget.contexts, requested.clamp(1, 4));
            assert_eq!(budget.threads, if backend.is_gpu() { 1 } else { 2 });
        }
    }
}

#[test]
fn cpu_thread_override_does_not_increase_gpu_helper_threads() {
    let resources = LlamaSettings {
        server_contexts: 2,
        threads: Some(3),
        hardware_threads: 16,
    }
    .resources(2);
    let cpu = LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cpu, 1, &resources);
    let cuda = LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cuda, 1, &resources);
    assert_eq!(cpu.threads, 3);
    assert_eq!(cuda.threads, 1);
    assert_eq!(
        LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cpu, 8, &resources).contexts,
        2
    );
}
