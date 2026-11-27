use super::*;

#[test]
fn runtime_shutdown_joins_an_unpolled_worker_guard_even_if_the_owner_survives() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let cleaned = Arc::new(AtomicBool::new(false));
    let native_cleaned = Arc::clone(&cleaned);
    let worker = runtime.block_on(async {
        spawn(move |receiver| {
            assert!(matches!(receiver.recv(), Ok(LlamaWorkerMessage::Shutdown)));
            native_cleaned.store(true, Ordering::Release);
        })
        .expect("worker")
    });
    // block_on's immediately-ready future never polls the spawned guard.
    // Retaining worker also proves shutdown does not depend on channel disconnect.
    drop(runtime);
    assert!(cleaned.load(Ordering::Acquire));
    assert!(worker.failed.load(Ordering::Acquire));
    assert!(worker.sender.send(LlamaWorkerMessage::Shutdown).is_err());
}

#[tokio::test]
async fn worker_panic_retires_the_worker_and_releases_its_native_ownership() {
    struct Cleanup(Arc<AtomicBool>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleanup = Cleanup(Arc::clone(&cleaned));
    let worker = spawn(move |_| {
        let _cleanup = cleanup;
        panic!("fixture native worker panic");
    })
    .expect("worker");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !worker.failed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker guard observed panic");
    assert!(cleaned.load(Ordering::Acquire));
}
