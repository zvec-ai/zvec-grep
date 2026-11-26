use super::*;
use crate::models::runtime::ModelComputeRuntime;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

struct NativeResult {
    cleaned: Arc<AtomicBool>,
    returned: Arc<AtomicBool>,
}

impl Drop for NativeResult {
    fn drop(&mut self) {
        assert!(
            !self.returned.load(Ordering::Acquire),
            "shutdown returned before native cleanup"
        );
        self.cleaned.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn native_work_returns_errors_and_recovers_after_panic() {
    let compute = ModelComputeRuntime::for_test(1);
    let error = compute
        .run_native(|| Err::<(), _>(ModelError::invalid_argument("fixture")))
        .await
        .expect("worker result")
        .expect_err("loader error");
    assert_eq!(error.code(), crate::EngineError::INVALID_ARGUMENT);
    let error = compute
        .run_native(|| panic!("fixture native panic"))
        .await
        .expect_err("panic becomes a worker error");
    assert_eq!(error.code(), crate::EngineError::INTERNAL);
    assert_eq!(compute.run_native(|| 42).await.expect("recovery"), 42);
}

#[test]
fn cancelled_native_work_and_abandoned_results_finish_before_runtime_shutdown() {
    for panic in [false, true] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let compute = ModelComputeRuntime::for_test(1);
        let cleaned = Arc::new(AtomicBool::new(false));
        let returned = Arc::new(AtomicBool::new(false));
        let native = NativeResult {
            cleaned: Arc::clone(&cleaned),
            returned: Arc::clone(&returned),
        };
        let (release, held) = mpsc::channel();
        runtime.block_on(async {
            let (started, running) = oneshot::channel();
            let mut request = Box::pin(compute.run_native(move || {
                let _ = started.send(());
                held.recv_timeout(Duration::from_secs(10))
                    .expect("release loader");
                assert!(!panic, "fixture native panic after cancellation");
                native
            }));
            assert!(futures_util::poll!(&mut request).is_pending());
            running.await.expect("loader started");
            drop(request);
            // Cancelling the requester neither waits for nor destroys native work.
            assert!(!cleaned.load(Ordering::Acquire));
        });
        let (shutting_down, entered) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let shutdown_cleaned = Arc::clone(&cleaned);
        let shutdown = std::thread::spawn(move || {
            shutting_down.send(()).expect("announce shutdown");
            drop(runtime);
            returned.store(true, Ordering::Release);
            assert!(shutdown_cleaned.load(Ordering::Acquire));
            finished.send(()).expect("shutdown finished");
        });
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("shutdown entered");
        assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).expect("finish native work");
        done.recv_timeout(Duration::from_secs(10))
            .expect("shutdown waited for native cleanup");
        shutdown.join().expect("shutdown thread");
        assert!(cleaned.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn outstanding_native_work_leaves_async_scheduling_available() {
    let compute = ModelComputeRuntime::for_test(2);
    let (release, held) = mpsc::channel();
    let (started, running) = oneshot::channel();
    let mut request = Box::pin(compute.run_native(move || {
        let _ = started.send(());
        held.recv_timeout(Duration::from_secs(10))
            .expect("release native work");
        7
    }));
    assert!(futures_util::poll!(&mut request).is_pending());
    running.await.expect("native work started");
    assert_eq!(compute.run(|| 42).await.expect("unrelated compute"), 42);
    assert_eq!(
        tokio::spawn(async { 3 }).await.expect("async scheduling"),
        3
    );
    release.send(()).expect("finish native work");
    assert_eq!(request.await.expect("native result"), 7);
}
