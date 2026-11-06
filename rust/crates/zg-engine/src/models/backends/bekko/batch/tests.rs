use std::sync::{
    Barrier, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use rayon::ThreadPoolBuilder;

use super::*;

#[test]
fn parallel_batch_reuses_callers_pool_and_preserves_order() {
    let pool = ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("pool");
    let ready = Barrier::new(2);
    let workers = Mutex::new(Vec::new());
    let result = pool.install(|| {
        collect_batch(&["first".into(), "second".into()], None, |text| {
            workers.lock().expect("workers").push(
                pool.current_thread_index()
                    .expect("must use the caller's pool"),
            );
            ready.wait();
            Ok((
                vec![if text == "first" { 1.0 } else { 2.0 }],
                text == "second",
            ))
        })
        .expect("batch")
    });
    assert_eq!(result.vectors, [vec![1.0], vec![2.0]]);
    assert_eq!(result.truncated, [1]);
    let mut workers = workers.into_inner().expect("workers");
    workers.sort_unstable();
    assert_eq!(workers, [0, 1]);
}

#[test]
fn errors_follow_input_order_and_do_not_poison_later_batches() {
    let pool = ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("pool");
    let ready = Barrier::new(2);
    let calls = AtomicUsize::new(0);
    pool.install(|| {
        let error = collect_batch(&["first".into(), "second".into()], None, |text| {
            calls.fetch_add(1, Ordering::Relaxed);
            ready.wait();
            Err(ModelError::invalid_argument(text))
        })
        .expect_err("failed batch");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(error.to_string().contains("first"));
        let recovered = collect_batch(&["ok".into(), "ok".into()], None, |_| {
            Ok((vec![1.0], false))
        })
        .expect("recovered");
        assert_eq!(recovered.vectors.len(), 2);
    });
}

#[test]
fn cancellation_before_and_during_parallel_work_never_returns_partial_results() {
    let pool = ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("pool");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = collect_batch(&["input".into()], Some(&cancelled), |_| {
        panic!("pre-cancelled batch must not run")
    })
    .expect_err("cancelled");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);

    let signal = CancellationToken::new();
    let ready = Barrier::new(2);
    let error = pool.install(|| {
        collect_batch(&["first".into(), "second".into()], Some(&signal), |_| {
            ready.wait();
            signal.cancel();
            Ok((vec![1.0], false))
        })
        .expect_err("cancelled before returning vectors")
    });
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
}

#[test]
fn single_input_runs_inline_on_current_worker() {
    let caller = std::thread::current().id();
    let result = collect_batch(&["single".into()], None, |_| {
        assert_eq!(std::thread::current().id(), caller);
        Ok((vec![0.5], true))
    })
    .expect("single input");
    assert_eq!(result.vectors, [vec![0.5]]);
    assert_eq!(result.truncated, [0]);
}
