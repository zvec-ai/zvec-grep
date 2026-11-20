use super::*;

fn settings(hardware_threads: usize, threads: Option<i32>) -> LlamaSettings {
    LlamaSettings {
        server_contexts: 4,
        threads,
        hardware_threads,
    }
}

#[test]
fn defaults_divide_half_the_available_cpus_by_capacity() {
    for (hardware, contexts, threads) in [(16, 1, 8), (16, 4, 2), (10, 2, 2), (1, 8, 1)] {
        let resources = settings(hardware, None).resources(contexts);
        assert_eq!(resources.contexts, contexts);
        assert_eq!(resources.cpu_threads, threads);
    }
    assert_eq!(settings(16, Some(3)).resources(4).cpu_threads, 3);
}

#[test]
fn overrides_require_positive_integers() {
    assert_eq!(positive(" 4 "), Some(4));
    for value in [
        "",
        " ",
        "0",
        "-1",
        "1.5",
        "+2",
        "many",
        "184467440737095516160",
    ] {
        assert_eq!(positive(value), None);
    }
}

#[test]
fn shared_slots_release_on_success_error_and_unwind() {
    let resources = settings(16, None).resources(2);
    let other_model = Arc::clone(&resources);
    let first = resources.acquire(None).expect("first model");
    let second = other_model.acquire(None).expect("second model");
    assert_eq!(*resources.in_flight.lock().expect("slot counter"), 2);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(resources.acquire(Some(&cancelled)).is_err());
    drop(first);
    let result: Result<(), ModelError> = (|| {
        let _slot = resources.acquire(None)?;
        Err(ModelError::internal("fixture failure"))
    })();
    assert!(result.is_err());
    assert_eq!(*resources.in_flight.lock().expect("slot counter"), 1);
    assert!(
        std::panic::catch_unwind(|| {
            let _slot = resources.acquire(None).expect("available slot");
            panic!("fixture unwind");
        })
        .is_err()
    );
    assert_eq!(*resources.in_flight.lock().expect("slot counter"), 1);
    drop(second);
    assert_eq!(*resources.in_flight.lock().expect("slot counter"), 0);
}

#[test]
fn cancelled_waiter_does_not_consume_a_slot() {
    let resources = settings(8, None).resources(1);
    let occupied = resources.acquire(None).expect("available slot");
    let signal = CancellationToken::new();
    let (ready, waiting) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let resources = Arc::clone(&resources);
        let token = signal.clone();
        let waiter = scope.spawn(move || {
            ready.send(()).expect("waiting receiver");
            resources.acquire(Some(&token)).map(|_| ())
        });
        waiting.recv().expect("waiter started");
        signal.cancel();
        assert_eq!(
            waiter
                .join()
                .expect("waiter thread")
                .expect_err("cancelled waiter")
                .code(),
            crate::EngineError::CANCELLED
        );
    });
    assert_eq!(*resources.in_flight.lock().expect("slot counter"), 1);
    drop(occupied);
    assert!(resources.acquire(None).is_ok());
}
