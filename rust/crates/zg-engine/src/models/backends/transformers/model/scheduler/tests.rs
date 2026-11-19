use super::*;

async fn request(
    queue: &InferenceQueue,
    purpose: EmbeddingPurpose,
    width: usize,
) -> (
    Request,
    oneshot::Receiver<Result<EmbeddingResult, ModelError>>,
) {
    request_rows(queue, purpose, width, 4).await
}

async fn request_rows(
    queue: &InferenceQueue,
    purpose: EmbeddingPurpose,
    width: usize,
    rows: usize,
) -> (
    Request,
    oneshot::Receiver<Result<EmbeddingResult, ModelError>>,
) {
    let (reply, receiver) = oneshot::channel();
    let admission = queue.admit(128, purpose, None).await.expect("admission");
    (
        Request {
            texts: vec!["fixture".into()],
            prepared: Some(PreparedBatch {
                input_ids: vec![1; rows * width],
                attention_mask: vec![1; rows * width],
                token_type_ids: vec![0; rows * width],
                position_ids: vec![0; rows * width],
                batch_size: rows,
                sequence_length: width,
                truncated: vec![1],
            }),
            options: EmbeddingOptions {
                purpose,
                execution_concurrency: 4,
                ..EmbeddingOptions::default()
            },
            reply,
            enqueued: Instant::now(),
            _admission: admission,
        },
        receiver,
    )
}

#[tokio::test]
async fn queries_precede_documents() {
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let (document, _doc) = request(&queue, EmbeddingPurpose::Document, 64).await;
    let (query, _query) = request(&queue, EmbeddingPurpose::Query, 64).await;
    state.push_back(document);
    state.push_back(query);
    let Work::Infer(selected) = state.next_eligible(|_| true) else {
        panic!("query ready")
    };
    assert_eq!(selected.options.purpose, EmbeddingPurpose::Query);
    assert_eq!(state.documents.len(), 1);
    assert!(matches!(state.next_eligible(|_| true), Work::Infer(_)));
}

#[tokio::test]
async fn each_request_runs_immediately_with_its_original_batch_shape() {
    for budget in [1, 2, 4] {
        let queue = InferenceQueue::default();
        let mut state = QueueState::default();
        let mut receivers = Vec::new();
        for (rows, width) in [(1, 64), (1, 64), (2, 128), (4, 256)] {
            let (mut request, receiver) =
                request_rows(&queue, EmbeddingPurpose::Document, width, rows).await;
            request.options.execution_concurrency = budget;
            state.push_back(request);
            receivers.push(receiver);
        }
        for (rows, width) in [(1, 64), (1, 64), (2, 128), (4, 256)] {
            let Work::Infer(selected) = state.next_eligible(|_| true) else {
                panic!("request must not wait for other requests to fill a batch");
            };
            let prepared = selected.prepared.as_ref().expect("prepared");
            assert_eq!(
                (prepared.batch_size, prepared.sequence_length),
                (rows, width)
            );
        }
        assert!(state.documents.is_empty());
    }
}

#[tokio::test]
async fn cancellation_of_one_request_preserves_other_results() {
    let queue = InferenceQueue::default();
    let (mut first, first_result) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let (second, second_result) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let signal = CancellationToken::new();
    first.options.signal = Some(signal.clone());
    signal.cancel();
    let result = EmbeddingResult {
        vectors: vec![vec![1.0]; 4],
        truncated: vec![1],
    };
    first.complete(Ok(result.clone()));
    second.complete(Ok(result));
    assert_eq!(
        first_result
            .await
            .expect("reply")
            .expect_err("cancelled")
            .code(),
        crate::EngineError::CANCELLED
    );
    let second = second_result
        .await
        .expect("reply")
        .expect("unaffected caller");
    assert_eq!(second.vectors, vec![vec![1.0]; 4]);
    assert_eq!(second.truncated, vec![1]);
    assert_eq!(queue.document_slots.available_permits(), 16);
}

#[tokio::test]
async fn shared_errors_keep_disposition_and_release_capacity() {
    let queue = InferenceQueue::default();
    let (first, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    first.complete(Err(ModelError::storage_failure("native model unavailable")));
    assert!(
        receiver
            .await
            .expect("reply")
            .expect_err("native failure")
            .should_fail_fast()
    );
    let (next, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    next.complete(Ok(EmbeddingResult {
        vectors: vec![vec![1.0]; 4],
        truncated: vec![],
    }));
    assert!(receiver.await.expect("recovery reply").is_ok());
    assert_eq!(queue.document_bytes.available_permits(), DOCUMENT_BYTES);
}

#[tokio::test]
async fn bounded_document_admission_reserves_query_capacity_and_cancels_waiters() {
    let queue = InferenceQueue::default();
    let mut admitted = Vec::new();
    for _ in 0..4 {
        admitted.push(
            queue
                .admit(MAX_REQUEST_BYTES, EmbeddingPurpose::Document, None)
                .await
                .expect("document"),
        );
    }
    assert_eq!(queue.document_bytes.available_permits(), 0);
    let query = queue
        .admit(128, EmbeddingPurpose::Query, None)
        .await
        .expect("reserved query capacity");
    let signal = CancellationToken::new();
    let mut pending = Box::pin(queue.admit(128, EmbeddingPurpose::Document, Some(&signal)));
    assert!(futures_util::poll!(&mut pending).is_pending());
    signal.cancel();
    assert_eq!(
        pending.await.err().expect("cancelled admission").code(),
        crate::EngineError::CANCELLED
    );
    drop(admitted);
    drop(query);
    assert_eq!(queue.document_slots.available_permits(), 16);
    assert_eq!(queue.query_slots.available_permits(), 4);
}

#[tokio::test]
async fn dropped_and_cancelled_requests_do_not_run_or_hold_permits() {
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let (first, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    drop(receiver);
    state.push_back(first);
    let (mut second, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let signal = CancellationToken::new();
    signal.cancel();
    second.options.signal = Some(signal);
    state.push_back(second);
    assert!(matches!(state.next_eligible(|_| true), Work::Empty));
    assert_eq!(
        receiver
            .await
            .expect("reply")
            .expect_err("cancelled")
            .code(),
        crate::EngineError::CANCELLED
    );
    assert_eq!(queue.document_slots.available_permits(), 16);
}

#[test]
fn input_storage_budget_includes_joining_and_prefixes() {
    let inputs = [vec![
        Content::Text("hello".into()),
        Content::Code("world".into()),
    ]];
    assert_eq!(input_bytes(&inputs, Some("query: ")).expect("size"), 18);
    assert!(
        input_bytes(
            &[vec![Content::Text("x".repeat(MAX_REQUEST_BYTES + 1))]],
            None
        )
        .is_err()
    );
}

#[tokio::test]
async fn dropping_a_worker_releases_pending_calls_without_stopping_a_new_generation() {
    let queue = Arc::new(InferenceQueue::default());
    let (pending, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    {
        let mut state = lock_std_mutex(&queue.state);
        state.running = true;
        state.generation = 1;
        state.push_back(pending);
    }
    drop(DriverGuard {
        queue: Arc::clone(&queue),
        generation: 1,
    });
    assert!(
        receiver
            .await
            .expect("worker error")
            .expect_err("worker stopped")
            .should_fail_fast()
    );
    assert!(!lock_std_mutex(&queue.state).running);
    assert_eq!(queue.document_slots.available_permits(), 16);
    let (pending, receiver) = request(&queue, EmbeddingPurpose::Query, 2).await;
    {
        let mut state = lock_std_mutex(&queue.state);
        state.running = true;
        state.generation = 2;
        state.push_back(pending);
    }
    drop(DriverGuard {
        queue: Arc::clone(&queue),
        generation: 1,
    });
    assert!(lock_std_mutex(&queue.state).running);
    assert_eq!(lock_std_mutex(&queue.state).queries.len(), 1);
    drop(DriverGuard {
        queue: Arc::clone(&queue),
        generation: 2,
    });
    assert!(receiver.await.expect("new worker error").is_err());
    assert_eq!(queue.query_slots.available_permits(), 4);
}

#[tokio::test]
async fn request_resources_retain_admission_and_lease_until_native_completion() {
    let queue = InferenceQueue::default();
    let lease = Arc::new(());
    let (mut request, reply) = request(&queue, EmbeddingPurpose::Document, 64).await;
    request.options.execution_lease = Some(lease.clone());
    let signal = CancellationToken::new();
    request.options.signal = Some(signal.clone());
    let resources = Arc::new(RequestResources {
        request: StdMutex::new(Some(request)),
    });
    let native_owned = Arc::clone(&resources);
    signal.cancel();
    drop(reply);
    drop(resources);
    assert_eq!(queue.document_slots.available_permits(), 15);
    assert_eq!(Arc::strong_count(&lease), 2);
    drop(native_owned);
    assert_eq!(queue.document_slots.available_permits(), 16);
    assert_eq!(Arc::strong_count(&lease), 1);
}

#[tokio::test]
async fn single_lane_waiter_does_not_block_a_two_lane_request_or_an_eligible_query() {
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let mut receivers = Vec::new();
    for (purpose, budget) in [
        (EmbeddingPurpose::Document, 1),
        (EmbeddingPurpose::Document, 2),
        (EmbeddingPurpose::Query, 1),
    ] {
        let (mut request, receiver) = request(&queue, purpose, 64).await;
        request.options.execution_concurrency = budget;
        state.push_back(request);
        receivers.push(receiver);
    }
    // Lane 0 is occupied. Lane 1 remains usable by the C2 document, while
    // the C1 document/query remain in their queues instead of pre-dispatching.
    let Work::Infer(selected) = state.next_eligible(|options| options.execution_concurrency == 2)
    else {
        panic!("eligible C2 request");
    };
    assert_eq!(selected.options.execution_concurrency, 2);
    assert_eq!(state.documents.len(), 1);
    assert_eq!(state.queries.len(), 1);
    // Once lane 0 becomes available, the query wins over the waiting document.
    let Work::Infer(selected) = state.next_eligible(|_| true) else {
        panic!("eligible query");
    };
    assert_eq!(selected.options.purpose, EmbeddingPurpose::Query);
    assert_eq!(state.documents.len(), 1);
    assert!(matches!(state.next_eligible(|_| false), Work::Blocked));
}

#[tokio::test]
async fn request_completion_waits_for_execution_lease_and_admission_release() {
    struct DropGate {
        started: Option<oneshot::Sender<()>>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Drop for DropGate {
        fn drop(&mut self) {
            let _ = self.started.take().expect("drop once").send(());
            lock_std_mutex(&self.release)
                .recv()
                .expect("allow lease destruction");
        }
    }
    let queue = InferenceQueue::default();
    let (mut request, mut reply) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let (started, dropping) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    request.options.execution_lease = Some(Arc::new(DropGate {
        started: Some(started),
        release: StdMutex::new(released),
    }));
    let completion = std::thread::spawn(move || {
        request.complete(Ok(EmbeddingResult {
            vectors: vec![vec![1.0]; 4],
            truncated: vec![],
        }));
    });
    dropping.await.expect("lease destructor entered");
    assert!(matches!(
        reply.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(queue.document_slots.available_permits(), 15);
    release.send(()).expect("release lease");
    assert!(reply.await.expect("reply follows cleanup").is_ok());
    assert_eq!(queue.document_slots.available_permits(), 16);
    completion.join().expect("completion thread");
}
