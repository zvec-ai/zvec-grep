use super::*;

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
                let budget = LlamaExecutionBudget::resolve(
                    architecture,
                    false,
                    contexts * threads,
                    contexts,
                )
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
        assert!(LlamaExecutionBudget::resolve("qwen3", false, concurrency, contexts).is_err());
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
        let budget = LlamaExecutionBudget::resolve("qwen3", false, 0, contexts)
            .expect("automatic CPU budget");
        assert_eq!(budget.contexts, contexts);
        assert_eq!(
            usize::try_from(budget.threads).expect("threads") * contexts,
            default_cpu_concurrency(contexts)
        );
    }
}

#[test]
fn gpu_context_budget_does_not_change_packing_or_cpu_helper_threads() {
    for count in [1, 2, 4, 8, 24, usize::MAX] {
        let gemma = LlamaExecutionBudget::resolve("gemma-embedding", true, count, 0)
            .expect("GPU budget ignores CPU context configuration");
        let qwen = LlamaExecutionBudget::resolve("qwen3", true, count, 3).expect("GPU budget");
        assert_eq!((gemma.contexts, qwen.contexts), (count, count));
        assert_eq!((gemma.threads, qwen.threads), (1, 1));
        assert_eq!(gemma.batch_tokens, Some(128));
        assert_eq!(qwen.batch_tokens, Some(256));
    }
    assert_eq!(
        LlamaExecutionBudget::resolve("unknown", true, 8, 1)
            .expect("unknown architecture")
            .batch_tokens,
        None
    );
}

#[test]
fn automatic_budget_is_resolved_again_for_the_actual_fallback_device() {
    for architecture in ["gemma-embedding", "qwen3"] {
        let gpu_default =
            LlamaExecutionBudget::resolve(architecture, true, 0, 1).expect("automatic GPU");
        assert_eq!(
            gpu_default,
            LlamaExecutionBudget::resolve(architecture, true, 1, 1).expect("explicit GPU")
        );
        let cpu_default = LlamaExecutionBudget::resolve(architecture, false, 0, 1)
            .expect("automatic CPU fallback");
        let cpu_explicit = LlamaExecutionBudget::resolve(architecture, false, 1, 1)
            .expect("explicit CPU fallback");
        assert_eq!(
            usize::try_from(cpu_default.threads).expect("threads"),
            default_cpu_concurrency(1)
        );
        assert_eq!(cpu_explicit.threads, 1);
        assert!(LlamaExecutionBudget::resolve(architecture, true, 1, 2).is_ok());
        assert!(LlamaExecutionBudget::resolve(architecture, false, 1, 2).is_err());
        assert!(LlamaExecutionBudget::resolve(architecture, false, 0, 2).is_ok());
    }
}
