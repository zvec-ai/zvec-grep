use super::*;

#[test]
fn cpu_budget_changes_threads_without_batching_or_a_context_cap() {
    for count in [1, 2, 4, 8, 24] {
        for architecture in ["gemma-embedding", "qwen3"] {
            let budget = LlamaExecutionBudget::resolve(architecture, false, count)
                .expect("valid test configuration");
            assert_eq!(
                usize::try_from(budget.threads).expect("positive threads"),
                count
            );
            assert_eq!(budget.batch_tokens, None);
        }
    }
    assert!(LlamaExecutionBudget::resolve("qwen3", false, usize::MAX).is_err());
}

#[test]
fn gpu_budget_changes_packing_without_multiplying_cpu_threads() {
    for count in [1, 2, 4, 8] {
        let gemma = LlamaExecutionBudget::resolve("gemma-embedding", true, count)
            .expect("valid test configuration");
        let qwen =
            LlamaExecutionBudget::resolve("qwen3", true, count).expect("valid test configuration");
        assert_eq!((gemma.threads, qwen.threads), (1, 1));
        assert_eq!(gemma.batch_tokens, Some(128 * count));
        assert_eq!(qwen.batch_tokens, Some(256 * count));
    }
    assert_eq!(
        LlamaExecutionBudget::resolve("qwen3", true, usize::MAX).expect("valid test configuration"),
        LlamaExecutionBudget::resolve("qwen3", true, 8).expect("valid test configuration"),
    );
    assert_eq!(
        LlamaExecutionBudget::resolve("unknown", true, 8)
            .expect("valid test configuration")
            .batch_tokens,
        None,
    );
}

#[test]
fn automatic_budget_is_resolved_again_for_the_actual_fallback_device() {
    let expected_cpu_threads =
        i32::try_from(default_cpu_concurrency()).expect("automatic CPU thread count");
    for architecture in ["gemma-embedding", "qwen3"] {
        let accelerator_default = LlamaExecutionBudget::resolve(architecture, true, 0)
            .expect("automatic GPU configuration");
        let accelerator_override = LlamaExecutionBudget::resolve(architecture, true, 1)
            .expect("explicit GPU configuration");
        assert_eq!(accelerator_default, accelerator_override);
        let automatic_fallback = LlamaExecutionBudget::resolve(architecture, false, 0)
            .expect("automatic CPU fallback configuration");
        let single_thread = LlamaExecutionBudget::resolve(architecture, false, 1)
            .expect("explicit CPU fallback configuration");
        assert_eq!(automatic_fallback.threads, expected_cpu_threads);
        assert_eq!(single_thread.threads, 1);
        assert_eq!(automatic_fallback.batch_tokens, None);
        assert_eq!(single_thread.batch_tokens, None);
    }
}
