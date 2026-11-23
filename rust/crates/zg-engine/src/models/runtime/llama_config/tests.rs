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
fn environment_reuses_context_parallelism_and_fixes_threads_in_both_modes() {
    let settings = LlamaSettings::from_environment(
        |name| match name {
            "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM" => Ok("4".to_owned()),
            "ZVEC_GREP_LLAMA_THREADS_PER_CONTEXT" => Ok("3".to_owned()),
            // The index request default must not enlarge Server capacity.
            "ZVEC_GREP_EMBEDDING_CONCURRENCY" => Ok("8".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        },
        16,
    );
    let server = settings.resources(settings.server_contexts);
    assert_eq!(server.contexts, 4);
    assert_eq!(server.cpu_threads, 3);
    for requested in [1, 2, 8] {
        let direct = settings.resources(requested);
        assert_eq!(direct.contexts, requested);
        assert_eq!(direct.cpu_threads, 3);
    }
}

#[test]
fn absent_or_invalid_environment_keeps_automatic_threads_and_one_server_context() {
    for value in [None, Some("0"), Some("-1"), Some("invalid")] {
        let settings = LlamaSettings::from_environment(
            |_| {
                value
                    .map(str::to_owned)
                    .ok_or(std::env::VarError::NotPresent)
            },
            16,
        );
        assert_eq!(settings.server_contexts, 1);
        assert_eq!(settings.threads, None);
        assert_eq!(settings.resources(4).cpu_threads, 2);
    }
    let settings = LlamaSettings::from_environment(
        |name| match name {
            "ZVEC_GREP_LLAMA_THREADS_PER_CONTEXT" => Ok("2147483648".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        },
        16,
    );
    assert_eq!(settings.threads, None);
    assert_eq!(settings.resources(1).cpu_threads, 8);
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
