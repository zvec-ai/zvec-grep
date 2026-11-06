use super::*;
use LlamaExecutionBackend::{Cpu, Cuda, OtherGpu};

#[test]
fn gpu_backend_classification_uses_actual_native_device_identity() {
    for (backend, name) in [("CUDA", "Tesla T4"), ("cuda", "NVIDIA GPU"), ("", "CUDA0")] {
        assert_eq!(LlamaExecutionBackend::from_gpu_device(backend, name), Cuda);
    }
    for (backend, name) in [
        ("Metal", "Apple M4"),
        ("MTL", "MTL0"),
        ("Vulkan", "NVIDIA GeForce RTX 4090"),
        ("Vulkan", "Apple M4"),
        ("SYCL", "Intel Arc"),
    ] {
        assert_eq!(
            LlamaExecutionBackend::from_gpu_device(backend, name),
            OtherGpu
        );
    }
    assert!(!Cpu.is_gpu());
    assert!(Cuda.is_gpu());
    assert!(OtherGpu.is_gpu());
}

#[test]
fn cpu_context_configuration_requires_a_positive_integer() {
    assert_eq!(parse_cpu_contexts(None), Ok(1));
    for (value, expected) in [("1", 1), (" 2 ", 2), ("24", 24)] {
        assert_eq!(parse_cpu_contexts(Some(value)), Ok(expected));
    }
    for value in [
        "",
        " ",
        "0",
        "-1",
        "+2",
        "1.5",
        "many",
        "9999999999999999999999999999999999",
    ] {
        assert!(parse_cpu_contexts(Some(value)).is_err(), "{value}");
    }
}

#[test]
fn cpu_budget_partitions_total_compute_threads_without_enabling_batching() {
    for contexts in [1, 2, 3, 8] {
        for threads in [1, 2, 4, 8, 24] {
            for architecture in ["gemma-embedding", "qwen3"] {
                let budget =
                    LlamaExecutionBudget::resolve(architecture, Cpu, contexts * threads, contexts)
                        .expect("valid CPU thread partition");
                assert_eq!(budget.contexts, contexts);
                assert_eq!(
                    usize::try_from(budget.threads).expect("positive threads"),
                    threads
                );
                assert_eq!(budget.batch_tokens, None);
            }
        }
    }
    for (concurrency, contexts) in [(1, 2), (3, 2), (8, 3), (1, 0), (usize::MAX, 1)] {
        let error = LlamaExecutionBudget::resolve("qwen3", Cpu, concurrency, contexts)
            .expect_err("invalid CPU execution configuration");
        assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
        assert!(error.should_fail_fast());
    }
}

#[test]
fn cpu_auto_aligns_to_configured_contexts_with_at_least_one_thread_each() {
    for (available, contexts, expected) in [
        (16, 1, 4),
        (16, 2, 4),
        (16, 3, 3),
        (16, 8, 8),
        (1, 2, 2),
        (0, 1, 1),
    ] {
        assert_eq!(automatic_cpu_concurrency(available, contexts), expected);
    }
    for contexts in [1, 2, 3, 8] {
        let budget =
            LlamaExecutionBudget::resolve("qwen3", Cpu, 0, contexts).expect("automatic CPU budget");
        assert_eq!(budget.contexts, contexts);
        assert_eq!(
            usize::try_from(budget.threads).expect("threads") * contexts,
            default_cpu_concurrency(contexts)
        );
    }
}

#[test]
fn gpu_context_budget_preserves_backend_packing_policy_and_cpu_helper_threads() {
    for (backend, gemma_packing, qwen_packing) in
        [(Cuda, None, None), (OtherGpu, Some(128), Some(256))]
    {
        for count in [1, 2, 4, 8, 24, usize::MAX] {
            let gemma = LlamaExecutionBudget::resolve("gemma-embedding", backend, count, 0)
                .expect("GPU budget ignores CPU context configuration");
            let qwen =
                LlamaExecutionBudget::resolve("qwen3", backend, count, 3).expect("GPU budget");
            assert_eq!((gemma.contexts, qwen.contexts), (count, count));
            assert_eq!((gemma.threads, qwen.threads), (1, 1));
            assert_eq!(gemma.batch_tokens, gemma_packing);
            assert_eq!(qwen.batch_tokens, qwen_packing);
        }
        assert_eq!(
            LlamaExecutionBudget::resolve("unknown", backend, 8, 1)
                .expect("unknown architecture")
                .batch_tokens,
            None
        );
    }
}

#[test]
fn automatic_budget_is_resolved_again_for_the_actual_fallback_device() {
    for architecture in ["gemma-embedding", "qwen3"] {
        for backend in [Cuda, OtherGpu] {
            let gpu_default =
                LlamaExecutionBudget::resolve(architecture, backend, 0, 1).expect("automatic GPU");
            assert_eq!(
                gpu_default,
                LlamaExecutionBudget::resolve(architecture, backend, 1, 1).expect("explicit GPU")
            );
            assert!(LlamaExecutionBudget::resolve(architecture, backend, 1, 2).is_ok());
        }
        let cpu_default =
            LlamaExecutionBudget::resolve(architecture, Cpu, 0, 1).expect("automatic CPU fallback");
        let cpu_explicit =
            LlamaExecutionBudget::resolve(architecture, Cpu, 1, 1).expect("explicit CPU fallback");
        assert_eq!(
            usize::try_from(cpu_default.threads).expect("threads"),
            default_cpu_concurrency(1)
        );
        assert_eq!(cpu_explicit.threads, 1);
        let error = LlamaExecutionBudget::resolve(architecture, Cpu, 1, 2)
            .expect_err("incompatible CPU fallback budget");
        assert!(error.should_fail_fast());
        assert!(LlamaExecutionBudget::resolve(architecture, Cpu, 0, 2).is_ok());
    }
}
