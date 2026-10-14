use super::*;

// Self-contained ONNX Identity(float[1]) graph, IR 9 / opset 13. This exercises
// real native cancellation without downloading weights or timing an inference.
const IDENTITY: &[u8] = &[
    8, 9, 58, 62, 10, 16, 10, 1, 120, 18, 1, 121, 34, 8, 73, 100, 101, 110, 116, 105, 116, 121, 18,
    8, 105, 100, 101, 110, 116, 105, 116, 121, 90, 15, 10, 1, 120, 18, 10, 10, 8, 8, 1, 18, 4, 10,
    2, 8, 1, 98, 15, 10, 1, 121, 18, 10, 10, 8, 8, 1, 18, 4, 10, 2, 8, 1, 66, 2, 16, 13,
];

fn identity_session() -> Session {
    Session::builder()
        .expect("session builder")
        .commit_from_memory(IDENTITY)
        .expect("identity graph")
}

fn native_run(session: &mut Session, options: &RunOptions) -> bool {
    let input = Tensor::from_array(([1], vec![42.0_f32])).expect("input tensor");
    session
        .run_with_options(ort::inputs![input], options)
        .is_ok()
}

#[tokio::test]
async fn token_cancellation_terminates_native_run_and_next_request_recovers() {
    let mut session = identity_session();
    let options = Arc::new(RunOptions::new().expect("run options"));
    let signal = CancellationToken::new();
    let mut guard = RunCancellation::new(Some(signal.clone()), Arc::clone(&options));
    assert!(native_run(&mut session, &options));
    signal.cancel();
    guard
        .watcher
        .take()
        .expect("watcher")
        .await
        .expect("cancel watcher");
    assert!(guard.is_cancelled());
    assert!(!native_run(&mut session, &options));
    drop(guard);
    assert!(native_run(
        &mut session,
        &RunOptions::new().expect("fresh run options")
    ));
}

#[tokio::test]
async fn dropping_embedding_future_stops_native_work_without_a_signal() {
    let mut session = identity_session();
    let options = Arc::new(RunOptions::new().expect("run options"));
    let guard = RunCancellation::new(None, Arc::clone(&options));
    assert!(native_run(&mut session, &options));
    drop(guard);
    assert!(!native_run(&mut session, &options));
    assert!(native_run(
        &mut session,
        &RunOptions::new().expect("fresh run options")
    ));
}
