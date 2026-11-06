use super::super::{LlamaToken, should_fallback_to_cpu};
use super::*;

#[test]
fn overlapping_operations_share_the_maximum_budget_instead_of_adding_it() {
    let mut admission = ContextAdmission::default();
    let first = admission.register(4);
    let second = admission.register(4);
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
    let mut admission = ContextAdmission::default();
    let small = admission.register(1);
    let large = admission.register(4);
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
fn releasing_a_large_operation_applies_the_remaining_limit() {
    let mut admission = ContextAdmission::default();
    let large = admission.register(4);
    let small = admission.register(1);
    assert!(admission.try_acquire(small));
    assert!(admission.try_acquire(large));
    admission.release(large);
    admission.unregister(large);
    assert_eq!(admission.limit(), 1);
    assert!(!admission.try_acquire(small));
    admission.release(small);
    assert!(admission.try_acquire(small));
    admission.release(small);
    admission.unregister(small);
    assert_eq!(admission.in_flight, 0);
    assert_eq!(admission.limit(), 0);
}

#[test]
fn cancellation_and_failed_jobs_release_their_ownership_before_retry() {
    let mut admission = ContextAdmission::default();
    let cancelled = admission.register(2);
    assert!(admission.try_acquire(cancelled));
    assert!(admission.try_acquire(cancelled));
    let waiting = admission.register(2);
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
    let retry = admission.register(2);
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
fn warm_workers_survive_operation_gaps_and_lower_budgets_retire_only_idle_workers() {
    let mut state = LlamaContextPoolState {
        workers: (0..4).map(|_| idle_worker()).collect(),
        ..LlamaContextPoolState::default()
    };
    assert!(retire_idle_workers(&mut state).is_empty());
    let busy = Arc::clone(&state.workers[3]);
    busy.busy.store(true, Ordering::Release);
    state.admission.register(1);
    assert_eq!(retire_idle_workers(&mut state).len(), 3);
    assert_eq!(state.workers.len(), 1);
    assert!(Arc::ptr_eq(&state.workers[0], &busy));
    busy.busy.store(false, Ordering::Release);
    assert!(retire_idle_workers(&mut state).is_empty());
    busy.failed.store(true, Ordering::Release);
    assert_eq!(retire_idle_workers(&mut state).len(), 1);
    assert!(state.workers.is_empty());
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
fn each_physical_job_sizes_qwen_kv_from_its_own_tokens() {
    let inputs = [32, 8_192, 64, 64]
        .into_iter()
        .enumerate()
        .map(|(index, length)| PreparedLlamaInput {
            index,
            tokens: vec![LlamaToken::new(7); length],
        })
        .collect::<Vec<_>>();
    let budget = LlamaExecutionBudget::resolve("qwen3", LlamaExecutionBackend::OtherGpu, 4, 1)
        .expect("packing-capable GPU budget");
    let execution = LlamaBatchExecution {
        architecture: "qwen3".to_owned(),
        context_size: 8_192,
        budget,
        concurrency: 4,
    };
    let batches = LlamaBatchPlan::new(&inputs, 8_192, budget.batch_tokens)
        .expect("plan")
        .into_batches(inputs);
    let profiles = batches
        .iter()
        .map(|inputs| {
            execution.context_profile(
                &LlamaBatchPlan::new(inputs, 8_192, budget.batch_tokens).expect("physical batch"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        profiles
            .iter()
            .map(|profile| profile.context_size)
            .collect::<Vec<_>>(),
        [256, 8_192, 256]
    );
    assert_eq!(
        profiles
            .iter()
            .map(|profile| profile.sequence_capacity)
            .collect::<Vec<_>>(),
        [1, 1, 2]
    );
    assert_eq!(
        profiles
            .iter()
            .map(|profile| profile.micro_batch_capacity)
            .collect::<Vec<_>>(),
        [32, 256, 128]
    );
}

#[test]
fn cuda_keeps_whole_sequences_in_separate_jobs_without_reducing_context_budget() {
    let inputs = [32, 8_192, 64, 64]
        .into_iter()
        .enumerate()
        .map(|(index, length)| PreparedLlamaInput {
            index,
            tokens: vec![LlamaToken::new(7); length],
        })
        .collect::<Vec<_>>();
    let budget = LlamaExecutionBudget::resolve("qwen3", LlamaExecutionBackend::Cuda, 4, 1)
        .expect("CUDA budget");
    assert_eq!(budget.contexts, 4);
    let execution = LlamaBatchExecution {
        architecture: "qwen3".to_owned(),
        context_size: 8_192,
        budget,
        concurrency: 4,
    };
    let batches = LlamaBatchPlan::new(&inputs, 8_192, budget.batch_tokens)
        .expect("CUDA plan")
        .into_batches(inputs.clone());
    assert_eq!(batches.len(), inputs.len());
    for (batch, expected) in batches.iter().zip(&inputs) {
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].index, expected.index);
        assert_eq!(batch[0].tokens, expected.tokens);
        let plan = LlamaBatchPlan::new(batch, 8_192, budget.batch_tokens).expect("single job");
        let profile = execution.context_profile(&plan);
        assert_eq!(profile.sequence_capacity, 1);
        assert!(profile.context_size as usize >= expected.tokens.len());
        assert!(profile.micro_batch_capacity <= 256);
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
