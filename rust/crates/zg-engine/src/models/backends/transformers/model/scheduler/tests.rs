use super::*;

fn entry() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

async fn request(
    queue: &InferenceQueue,
    purpose: EmbeddingPurpose,
    width: usize,
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
                input_ids: vec![1; 4 * width],
                attention_mask: vec![1; 4 * width],
                token_type_ids: vec![0; 4 * width],
                position_ids: vec![0; 4 * width],
                padding_input_id: 0,
                batch_size: 4,
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
            ready: Instant::now()
                .checked_sub(policy::BATCH_WAIT)
                .expect("recent instant"),
            _admission: admission,
        },
        receiver,
    )
}

fn infer_count(work: Work) -> usize {
    match work {
        Work::Infer(requests) => requests.len(),
        _ => panic!("expected ready inference"),
    }
}

#[tokio::test]
async fn queries_precede_documents_and_are_never_coalesced() {
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let (document, _doc) = request(&queue, EmbeddingPurpose::Document, 64).await;
    let (query, _query) = request(&queue, EmbeddingPurpose::Query, 64).await;
    state.push_back(document);
    state.push_back(query);
    let Work::Infer(mut selected) =
        state.next(entry(), TransformersExecutionProvider::Cuda, Instant::now())
    else {
        panic!("query ready")
    };
    assert_eq!(selected.len(), 1);
    assert_eq!(selected.remove(0).options.purpose, EmbeddingPurpose::Query);
    assert_eq!(state.documents.len(), 1);
    assert_eq!(
        infer_count(state.next(entry(), TransformersExecutionProvider::Cuda, Instant::now())),
        1
    );
}

#[tokio::test]
async fn cpu_serializes_batches_and_cuda_limits_padded_shape() {
    for (provider, width, expected) in [
        (TransformersExecutionProvider::Cpu, 64, 1),
        (TransformersExecutionProvider::Cuda, 64, 4),
        (TransformersExecutionProvider::Cuda, 160, 4),
        (TransformersExecutionProvider::Cuda, 256, 4),
    ] {
        let queue = InferenceQueue::default();
        let mut state = QueueState::default();
        let mut receivers = Vec::new();
        for _ in 0..4 {
            let (request, receiver) = request(&queue, EmbeddingPurpose::Document, width).await;
            state.push_back(request);
            receivers.push(receiver);
        }
        assert_eq!(
            infer_count(state.next(entry(), provider, Instant::now())),
            expected
        );
        assert_eq!(state.documents.len(), 4 - expected);
    }
}

#[tokio::test]
async fn cancellation_of_one_member_preserves_other_results_and_offsets() {
    let queue = InferenceQueue::default();
    let (mut first, first_result) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let (second, second_result) = request(&queue, EmbeddingPurpose::Document, 2).await;
    let signal = CancellationToken::new();
    first.options.signal = Some(signal.clone());
    signal.cancel();
    finish(
        vec![first, second],
        Ok(EmbeddingResult {
            vectors: (0..8)
                .map(|i| vec![f32::from(u16::try_from(i).expect("small index"))])
                .collect(),
            truncated: vec![1, 5],
        }),
    );
    assert_eq!(
        first_result
            .await
            .expect("first reply")
            .expect_err("cancelled")
            .code(),
        crate::EngineError::CANCELLED
    );
    let second = second_result
        .await
        .expect("second reply")
        .expect("unaffected caller");
    assert_eq!(
        second.vectors,
        vec![vec![4.0], vec![5.0], vec![6.0], vec![7.0]]
    );
    assert_eq!(second.truncated, vec![1]);
    assert_eq!(queue.document_slots.available_permits(), 16);
}

#[tokio::test]
async fn shared_errors_keep_disposition_and_release_capacity() {
    let queue = InferenceQueue::default();
    let (first, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    finish(
        vec![first],
        Err(ModelError::storage_failure("native model unavailable")),
    );
    assert!(
        receiver
            .await
            .expect("reply")
            .expect_err("native failure")
            .should_fail_fast()
    );
    let (next, receiver) = request(&queue, EmbeddingPurpose::Document, 2).await;
    finish(
        vec![next],
        Ok(EmbeddingResult {
            vectors: vec![vec![1.0]; 4],
            truncated: vec![],
        }),
    );
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
    assert!(matches!(
        state.next(entry(), TransformersExecutionProvider::Cuda, Instant::now()),
        Work::Empty
    ));
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
async fn coalescing_is_independent_of_concurrency_and_fixed_batches_bypass_it() {
    for budget in [1, 2] {
        let queue = InferenceQueue::default();
        let mut state = QueueState::default();
        let mut receivers = Vec::new();
        for width in [64, 128, 64, 64] {
            let (mut request, receiver) = request(&queue, EmbeddingPurpose::Document, width).await;
            request.options.execution_concurrency = budget;
            state.push_back(request);
            receivers.push(receiver);
        }
        assert_eq!(
            infer_count(state.next(entry(), TransformersExecutionProvider::Cuda, Instant::now())),
            3
        );
        assert_eq!(state.documents.len(), 1);
    }
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let mut receivers = Vec::new();
    for _ in 0..3 {
        let (mut request, receiver) = request(&queue, EmbeddingPurpose::Document, 64).await;
        request.options.fixed_batch = true;
        request.ready = Instant::now();
        state.push_back(request);
        receivers.push(receiver);
    }
    assert_eq!(
        infer_count(state.next(entry(), TransformersExecutionProvider::Cuda, Instant::now())),
        1
    );
}

#[tokio::test]
async fn coalescing_wait_is_bounded_and_queries_skip_it() {
    let queue = InferenceQueue::default();
    let mut state = QueueState::default();
    let (mut document, _document) = request(&queue, EmbeddingPurpose::Document, 64).await;
    let ready = Instant::now();
    document.ready = ready;
    state.push_back(document);
    assert!(
        matches!(state.next(entry(), TransformersExecutionProvider::Cuda, ready), Work::Wait(until) if until == ready + policy::BATCH_WAIT)
    );
    let (query, _query) = request(&queue, EmbeddingPurpose::Query, 64).await;
    state.push_back(query);
    assert_eq!(
        infer_count(state.next(entry(), TransformersExecutionProvider::Cuda, ready)),
        1
    );
    assert_eq!(
        infer_count(state.next(
            entry(),
            TransformersExecutionProvider::Cuda,
            ready + policy::BATCH_WAIT
        )),
        1
    );
}

#[tokio::test]
async fn merged_resources_retain_every_admission_and_lease_until_native_completion() {
    let queue = InferenceQueue::default();
    let first_lease = Arc::new(());
    let second_lease = Arc::new(());
    let (mut first, _first_reply) = request(&queue, EmbeddingPurpose::Document, 64).await;
    let (mut second, _second_reply) = request(&queue, EmbeddingPurpose::Document, 64).await;
    first.options.execution_lease = Some(first_lease.clone());
    second.options.execution_lease = Some(second_lease.clone());
    let resources = Arc::new(BatchResources {
        requests: StdMutex::new(vec![first, second]),
    });
    let native_owned = Arc::clone(&resources);
    drop(resources);
    assert_eq!(queue.document_slots.available_permits(), 14);
    assert_eq!(Arc::strong_count(&first_lease), 2);
    assert_eq!(Arc::strong_count(&second_lease), 2);
    drop(native_owned);
    assert_eq!(queue.document_slots.available_permits(), 16);
    assert_eq!(Arc::strong_count(&first_lease), 1);
    assert_eq!(Arc::strong_count(&second_lease), 1);
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
        request.options.fixed_batch = true;
        state.push_back(request);
        receivers.push(receiver);
    }
    // Lane 0 is occupied. Lane 1 remains usable by the C2 document, while
    // the C1 document/query remain in their queues instead of pre-dispatching.
    let Work::Infer(selected) = state.next_eligible(
        entry(),
        TransformersExecutionProvider::Cuda,
        Instant::now(),
        |options| options.execution_concurrency == 2,
    ) else {
        panic!("eligible C2 request");
    };
    assert_eq!(selected[0].options.execution_concurrency, 2);
    assert_eq!(state.documents.len(), 1);
    assert_eq!(state.queries.len(), 1);
    // Once lane 0 becomes available, the query wins over the waiting document.
    let Work::Infer(selected) = state.next_eligible(
        entry(),
        TransformersExecutionProvider::Cuda,
        Instant::now(),
        |_| true,
    ) else {
        panic!("eligible query");
    };
    assert_eq!(selected[0].options.purpose, EmbeddingPurpose::Query);
    assert_eq!(state.documents.len(), 1);
    assert!(matches!(
        state.next_eligible(
            entry(),
            TransformersExecutionProvider::Cuda,
            Instant::now(),
            |_| false
        ),
        Work::Blocked
    ));
}

#[tokio::test]
async fn eight_admitted_minilm_requests_form_two_batches_at_both_execution_budgets() {
    for budget in [1, 2] {
        for rows in [1, 4] {
            let queue = InferenceQueue::default();
            let mut state = QueueState::default();
            let mut receivers = Vec::new();
            let now = Instant::now();
            for _ in 0..8 {
                let (mut request, receiver) = request(&queue, EmbeddingPurpose::Document, 64).await;
                request.options.execution_concurrency = budget;
                request.ready = now;
                request.prepared.as_mut().expect("prepared").batch_size = rows;
                state.push_back(request);
                receivers.push(receiver);
            }
            for _ in 0..2 {
                let Work::Infer(requests) =
                    state.next(entry(), TransformersExecutionProvider::Cuda, now)
                else {
                    panic!("ready native batch");
                };
                assert_eq!(requests.len(), 4);
                assert_eq!(
                    requests
                        .iter()
                        .map(|request| request.prepared.as_ref().expect("prepared").batch_size)
                        .sum::<usize>(),
                    4 * rows
                );
            }
            assert!(state.documents.is_empty());
        }
    }
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
