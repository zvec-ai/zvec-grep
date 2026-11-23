use super::super::tests::test_resources;
use super::super::{LlamaToken, should_fallback_to_cpu};
use super::*;

#[test]
fn overlapping_operations_share_the_fixed_capacity_instead_of_adding_budgets() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let first = admission.register(4, None);
    let second = admission.register(4, None);
    for id in [first, second, first, second] {
        assert!(admission.try_acquire(id));
    }
    assert_eq!(admission.in_flight, 4);
    assert!(!admission.try_acquire(first));
    assert!(!admission.try_acquire(second));
    admission.release(first);
    assert!(admission.try_acquire(second));
    assert_eq!(admission.in_flight, 4);
}

#[test]
fn an_operation_cannot_borrow_another_operations_larger_limit() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let small = admission.register(1, None);
    let large = admission.register(4, None);
    assert!(admission.try_acquire(small));
    assert!(!admission.try_acquire(small));
    for _ in 0..3 {
        assert!(admission.try_acquire(large));
    }
    assert!(!admission.try_acquire(large));
    admission.release(small);
    admission.unregister(small);
    assert!(admission.try_acquire(large));
}

#[test]
fn releasing_an_operation_preserves_server_capacity() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let large = admission.register(4, None);
    let small = admission.register(1, None);
    assert!(admission.try_acquire(small));
    assert!(admission.try_acquire(large));
    admission.release(large);
    admission.unregister(large);
    assert_eq!(admission.limit(), 4);
    assert!(!admission.try_acquire(small));
    admission.release(small);
    assert!(admission.try_acquire(small));
    admission.release(small);
    admission.unregister(small);
    assert_eq!(admission.in_flight, 0);
    assert_eq!(admission.limit(), 4);
}

#[test]
fn cancellation_and_failed_jobs_release_their_ownership_before_retry() {
    let mut admission = ContextAdmission {
        capacity: 2,
        ..ContextAdmission::default()
    };
    let cancelled = admission.register(2, None);
    assert!(admission.try_acquire(cancelled));
    assert!(admission.try_acquire(cancelled));
    let waiting = admission.register(2, None);
    assert!(!admission.try_acquire(waiting));
    // Cancellation keeps ownership until each already submitted job drains.
    admission.release(cancelled);
    assert!(admission.try_acquire(waiting));
    assert!(!admission.try_acquire(waiting));
    admission.release(cancelled);
    admission.unregister(cancelled);
    assert!(admission.try_acquire(waiting));
    admission.release(waiting);
    admission.release(waiting);
    admission.unregister(waiting);
    let retry = admission.register(2, None);
    assert!(admission.try_acquire(retry));
    assert!(admission.try_acquire(retry));
}

fn idle_worker() -> Arc<LlamaContextWorker> {
    let (sender, _) = mpsc::channel();
    Arc::new(LlamaContextWorker {
        busy: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        sender,
        join: StdMutex::new(None),
    })
}

#[test]
fn small_requests_keep_fixed_capacity_and_retire_only_failed_workers() {
    let mut state = LlamaContextPoolState {
        workers: (0..4).map(|_| idle_worker()).collect(),
        admission: ContextAdmission {
            capacity: 4,
            ..ContextAdmission::default()
        },
    };
    state.admission.register(1, None);
    assert!(retire_idle_workers(&mut state).is_empty());
    state.workers[3].failed.store(true, Ordering::Release);
    assert_eq!(retire_idle_workers(&mut state).len(), 1);
    assert_eq!(state.workers.len(), 3);
}

#[test]
fn batches_of_one_request_share_its_context_allowance() {
    let mut admission = ContextAdmission {
        capacity: 4,
        ..ContextAdmission::default()
    };
    let scope = Arc::new(());
    let first_batch = admission.register(1, Some(Arc::clone(&scope)));
    let second_batch = admission.register(1, Some(scope));
    assert_eq!(first_batch, second_batch);
    assert!(admission.try_acquire(first_batch));
    assert!(!admission.try_acquire(second_batch));
    let other_request = admission.register(4, Some(Arc::new(())));
    assert!(admission.try_acquire(other_request));
    admission.unregister(second_batch);
    assert!(!admission.try_acquire(first_batch));
    admission.release(first_batch);
    assert!(admission.try_acquire(first_batch));
    admission.release(first_batch);
    admission.unregister(first_batch);
    admission.release(other_request);
    admission.unregister(other_request);
    assert!(admission.operations.is_empty());
}

#[test]
fn a_dropped_response_disconnects_and_wakes_its_owner_without_native_inference() {
    let completion = Arc::new(CompletionSignal::default());
    let (sender, receiver) = mpsc::sync_channel(1);
    let generation = completion.generation();
    drop(LlamaResponseSender {
        sender: Some(sender),
        completion: Arc::clone(&completion),
    });
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    assert_ne!(completion.generation(), generation);
    completion.wait(generation);
}

#[test]
fn cancellation_response_keeps_its_structured_error_and_notifies_waiters() {
    let completion = Arc::new(CompletionSignal::default());
    let (sender, receiver) = mpsc::sync_channel(1);
    let generation = completion.generation();
    LlamaResponseSender {
        sender: Some(sender),
        completion: Arc::clone(&completion),
    }
    .send(Err(ModelError::cancelled(
        "cancelled after native completion",
    )
    .with_cause("fixture")));
    let error = receiver
        .try_recv()
        .expect("response")
        .expect_err("cancelled");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert_eq!(error.cause(), Some("fixture"));
    assert!(!should_fallback_to_cpu(true, &error));
    assert_ne!(completion.generation(), generation);
}

#[test]
fn each_input_sizes_its_own_context_on_every_backend() {
    for backend in [
        LlamaExecutionBackend::Cpu,
        LlamaExecutionBackend::Cuda,
        LlamaExecutionBackend::OtherGpu,
    ] {
        for architecture in ["qwen3", "gemma-embedding"] {
            let context_size = if architecture == "qwen3" {
                8_192
            } else {
                2_048
            };
            let budget = LlamaExecutionBudget::resolve(backend, 4, &test_resources(4));
            assert_eq!(budget.contexts, 4);
            let execution = LlamaBatchExecution {
                architecture: architecture.to_owned(),
                context_size,
                budget,
                concurrency: 4,
            };
            let profiles = [32, context_size as usize, 64, 64]
                .into_iter()
                .enumerate()
                .map(|(index, length)| {
                    execution
                        .context_profile(&PreparedLlamaInput {
                            index,
                            tokens: vec![LlamaToken::new(7); length],
                        })
                        .expect("single input profile")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                profiles
                    .iter()
                    .map(|profile| profile.context_size)
                    .collect::<Vec<_>>(),
                if architecture == "qwen3" {
                    vec![256, 8_192, 256, 256]
                } else {
                    vec![2_048; 4]
                }
            );
            assert_eq!(
                profiles
                    .iter()
                    .map(|profile| profile.micro_batch_capacity)
                    .collect::<Vec<_>>(),
                if architecture == "qwen3" {
                    vec![32, 256, 64, 64]
                } else {
                    vec![32, 2_048, 64, 64]
                }
            );
        }
    }
}

#[test]
fn invalid_input_lengths_are_rejected_before_dispatch() {
    let execution = LlamaBatchExecution {
        architecture: "qwen3".to_owned(),
        context_size: 256,
        budget: LlamaExecutionBudget::resolve(LlamaExecutionBackend::Cpu, 4, &test_resources(4)),
        concurrency: 4,
    };
    for length in [0, 257] {
        let error = execution
            .context_profile(&PreparedLlamaInput {
                index: 0,
                tokens: vec![LlamaToken::new(7); length],
            })
            .expect_err("invalid input length");
        assert!(!should_fallback_to_cpu(true, &error));
    }
}

#[test]
fn another_operations_completion_wakes_waiters_with_a_pending_long_job() {
    let completion = Arc::new(CompletionSignal::default());
    let long_operation = Arc::clone(&completion);
    let short_operation = Arc::clone(&completion);
    let observed = long_operation.generation();
    // A global capacity release must be observable even if this operation's
    // own pending native job has not completed.
    short_operation.notify();
    assert_ne!(long_operation.generation(), observed);
    long_operation.wait(observed);
}
