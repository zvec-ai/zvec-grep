use super::*;

const QWEN_CONTEXT_SIZE: u32 = 8_192;
const QWEN_DIMENSION: usize = 1_024;

#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with Qwen3 embedding; compares individual and batched vectors"]
async fn cached_qwen3_cpu_batches_preserve_vectors_and_kv_isolation() {
    qwen_batches_preserve_vectors_and_kv_isolation(Device::Cpu).await;
}

#[tokio::test]
#[ignore = "requires cached Qwen3 embedding; compares context reuse at fixed threads with fresh CPU contexts"]
async fn cached_qwen3_cpu_concurrency_changes_match_fresh_contexts() {
    let loaded = load_qwen_fixture(Device::Cpu).await;
    let mut input = qwen_batch_fixture(&loaded.model).remove(2);
    input.index = 0;
    for concurrency in [1, 2, 4, 8, 1] {
        let actual = qwen_infer(&loaded, vec![input.clone()], concurrency).await;
        let fresh = LlamaContextPool::new(
            Arc::clone(&loaded.model),
            loaded.contexts.backend,
            test_resources(1),
        );
        let expected = fresh
            .run(
                PreparedLlamaBatch {
                    inputs: vec![input.clone()],
                    truncated: Vec::new(),
                },
                QWEN_CONTEXT_SIZE,
                concurrency,
                None,
                None,
            )
            .await
            .expect("fresh CPU context");
        eprintln!("CPU fixed-thread fresh/reused comparison C={concurrency}");
        assert_embedding_close(&actual[0], &expected.vectors[0]);
    }
}

#[tokio::test]
#[ignore = "requires cached Qwen3 embedding; CPU context limit 2 with requests C=2/4/8 and fixed-thread reference"]
async fn cached_qwen3_cpu_two_contexts_preserve_vectors_and_recover() {
    let loaded = load_qwen_fixture(Device::Cpu).await;
    assert_cpu_two_context_pool(
        &loaded,
        &qwen_batch_fixture(&loaded.model),
        QWEN_CONTEXT_SIZE,
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires cached Qwen3 embedding and Metal; no CPU fallback"]
async fn cached_qwen3_metal_batches_preserve_vectors_and_kv_isolation() {
    qwen_batches_preserve_vectors_and_kv_isolation(Device::Metal).await;
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "cuda"))]
#[tokio::test]
#[ignore = "requires cached Qwen3 embedding and CUDA plugins; run in a fresh process; no CPU fallback"]
async fn cached_qwen3_cuda_batches_preserve_vectors_and_kv_isolation() {
    qwen_batches_preserve_vectors_and_kv_isolation(Device::Cuda).await;
}

async fn load_qwen_fixture(device: Device) -> Arc<LoadedLlamaModel> {
    init_llama_test_logs();
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/qwen3-embedding-0.6b"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(device),
            ..ModelConfig::default()
        },
        ModelComputeRuntime::shared(),
        test_resources(8),
    );
    let loaded = model
        .ensure_loaded(None, None, None)
        .await
        .expect("cached model");
    assert_eq!(loaded.gpu, device != Device::Cpu);
    assert_eq!(
        loaded.contexts.backend,
        match device {
            Device::Cpu => LlamaExecutionBackend::Cpu,
            Device::Cuda => LlamaExecutionBackend::Cuda,
            _ => LlamaExecutionBackend::OtherGpu,
        }
    );
    loaded
}

fn qwen_tokens(model: &LlamaModel, text: &str, length: usize) -> Vec<LlamaToken> {
    let mut tokens = model
        .str_to_token(&text.repeat(length), AddBos::Always)
        .expect("Qwen fixture tokens");
    assert!(tokens.len() >= length);
    tokens.truncate(length);
    tokens
}

fn qwen_batch_fixture(model: &LlamaModel) -> Vec<PreparedLlamaInput> {
    [
        ("authentication middleware checks access permissions ", 32),
        ("configuration parser reports invalid syntax ", 24),
        ("vector search ranks documents using similarity ", 257),
        ("image processing resizes and rotates pictures ", 65),
        ("cache eviction releases unused entries and buffers ", 513),
        ("authentication middleware checks access permissions ", 32),
        ("database transaction rolls back failed writes ", 63),
        (
            "Instruct: Retrieve relevant documents\nQuery: parse errors ",
            96,
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (text, length))| PreparedLlamaInput {
        index,
        tokens: qwen_tokens(model, text, length),
    })
    .collect()
}

async fn qwen_infer(
    loaded: &LoadedLlamaModel,
    inputs: Vec<PreparedLlamaInput>,
    concurrency: usize,
) -> Vec<Vec<f32>> {
    let result = loaded
        .contexts
        .run(
            PreparedLlamaBatch {
                inputs,
                truncated: Vec::new(),
            },
            QWEN_CONTEXT_SIZE,
            concurrency,
            None,
            None,
        )
        .await
        .expect("Qwen embedding inference");
    assert!(result.truncated.is_empty());
    assert!(result.vectors.iter().all(|vector| {
        vector.len() == QWEN_DIMENSION && vector.iter().all(|value| value.is_finite())
    }));
    result.vectors
}

async fn qwen_individual_reference(
    loaded: &LoadedLlamaModel,
    inputs: &[PreparedLlamaInput],
    concurrency: usize,
) -> Vec<Vec<f32>> {
    let fresh = LlamaContextPool::new(
        Arc::clone(&loaded.model),
        loaded.contexts.backend,
        test_resources(1),
    );
    let mut vectors = Vec::new();
    for input in inputs {
        vectors.push(
            fresh
                .run(
                    PreparedLlamaBatch {
                        inputs: vec![PreparedLlamaInput {
                            index: 0,
                            tokens: input.tokens.clone(),
                        }],
                        truncated: Vec::new(),
                    },
                    QWEN_CONTEXT_SIZE,
                    concurrency,
                    None,
                    None,
                )
                .await
                .expect("individual reference with matching CPU threads")
                .vectors
                .remove(0),
        );
    }
    vectors
}

async fn qwen_batches_preserve_vectors_and_kv_isolation(device: Device) {
    let loaded = load_qwen_fixture(device).await;
    assert_context_worker_ownership(&loaded).await;
    let inputs = qwen_batch_fixture(&loaded.model);
    let mut expected = Vec::new();
    for input in &inputs {
        expected.push(
            qwen_infer(
                &loaded,
                vec![PreparedLlamaInput {
                    index: 0,
                    tokens: input.tokens.clone(),
                }],
                1,
            )
            .await
            .remove(0),
        );
    }
    assert_embedding_close(&expected[0], &expected[5]);

    for concurrency in [1, 2, 4, 8] {
        // Native CPU flash attention can partition a one-token tail differently
        // across thread counts. Compare reuse against a fresh context with the
        // same compute budget, retaining the strict vector error threshold.
        let cpu_reference = if loaded.gpu {
            None
        } else {
            Some(qwen_individual_reference(&loaded, &inputs, concurrency).await)
        };
        let expected = cpu_reference.as_ref().unwrap_or(&expected);
        // Submit out of index order so input dispatch and output reordering
        // must preserve each sequence's own pooled embedding.
        let reordered = [6, 2, 0, 7, 3, 5, 1, 4]
            .into_iter()
            .map(|index| inputs[index].clone())
            .collect();
        let result = qwen_infer(&loaded, reordered, concurrency).await;
        assert_eq!(result.len(), expected.len());
        for (index, (actual, expected)) in result.iter().zip(expected).enumerate() {
            eprintln!("Qwen batch equivalence C={concurrency} input={index}");
            assert_embedding_close(actual, expected);
        }
        assert_embedding_close(&result[0], &result[5]);

        // The same context must clear prior sequence KV before a short query,
        // then accept another batch of independent inputs without stale positions.
        let query = qwen_infer(&loaded, vec![inputs[0].clone()], concurrency).await;
        assert_embedding_close(&query[0], &expected[0]);
        let repeated = qwen_infer(&loaded, inputs.clone(), concurrency).await;
        assert_eq!(repeated.len(), expected.len());
        for (actual, expected) in repeated.iter().zip(expected) {
            assert_embedding_close(actual, expected);
        }
        assert_context_worker_ownership(&loaded).await;
    }

    if loaded.gpu {
        assert_qwen_sequence_isolation(&loaded, &inputs).await;
    }
    let recovery_reference = if loaded.gpu {
        expected
    } else {
        qwen_individual_reference(&loaded, &inputs, 4).await
    };
    assert_qwen_failure_recovery(&loaded, &inputs, &recovery_reference).await;
}

async fn assert_qwen_sequence_isolation(loaded: &LoadedLlamaModel, inputs: &[PreparedLlamaInput]) {
    // A target must stay unchanged when another input's token values change,
    // including when independent jobs reuse the same context.
    for target in [0, 3] {
        let mut pair = vec![inputs[target].clone(), inputs[1].clone()];
        pair[0].index = 0;
        pair[1].index = 1;
        let before = qwen_infer(loaded, pair.clone(), 8).await;
        pair[1].tokens.reverse();
        let after = qwen_infer(loaded, pair, 8).await;
        let difference = before[0]
            .iter()
            .zip(&after[0])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            difference < 1e-5,
            "Qwen cross-sequence interference: max_absolute_difference={difference}"
        );
        assert_ne!(before[1], after[1]);
    }
}

async fn assert_qwen_failure_recovery(
    loaded: &LoadedLlamaModel,
    inputs: &[PreparedLlamaInput],
    expected: &[Vec<f32>],
) {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        loaded
            .contexts
            .run(
                PreparedLlamaBatch {
                    inputs: inputs.to_vec(),
                    truncated: Vec::new(),
                },
                QWEN_CONTEXT_SIZE,
                4,
                None,
                Some(&cancelled),
            )
            .await
            .is_err()
    );
    assert_context_worker_ownership(loaded).await;
    assert!(
        loaded
            .contexts
            .run(
                PreparedLlamaBatch {
                    inputs: vec![PreparedLlamaInput {
                        index: 0,
                        tokens: vec![LlamaToken::new(-1)],
                    }],
                    truncated: Vec::new(),
                },
                QWEN_CONTEXT_SIZE,
                4,
                None,
                None,
            )
            .await
            .is_err()
    );
    let recovered = qwen_infer(loaded, inputs.to_vec(), 4).await;
    assert_eq!(recovered.len(), expected.len());
    for (actual, expected) in recovered.iter().zip(expected) {
        assert_embedding_close(actual, expected);
    }
    assert_context_worker_ownership(loaded).await;
}

#[tokio::test]
#[ignore = "requires cached Qwen3 embedding; expensive 8192-token CPU regression, run separately"]
async fn cached_qwen3_cpu_handles_full_context_and_reuse() {
    qwen_handles_full_context_and_reuse(Device::Cpu).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires cached Qwen3 embedding and Metal; exercises 8192 tokens; no CPU fallback"]
async fn cached_qwen3_metal_handles_full_context_and_reuse() {
    qwen_handles_full_context_and_reuse(Device::Metal).await;
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "cuda"))]
#[tokio::test]
#[ignore = "requires cached Qwen3 embedding and CUDA plugins; 8192 tokens; run in a fresh process"]
async fn cached_qwen3_cuda_handles_full_context_and_reuse() {
    qwen_handles_full_context_and_reuse(Device::Cuda).await;
}

async fn qwen_handles_full_context_and_reuse(device: Device) {
    let loaded = load_qwen_fixture(device).await;
    let tokens = qwen_tokens(
        &loaded.model,
        "authentication middleware validates access permissions ",
        usize::try_from(QWEN_CONTEXT_SIZE).expect("context size"),
    );
    let infer = async |length| {
        qwen_infer(
            &loaded,
            vec![PreparedLlamaInput {
                index: 0,
                tokens: tokens[..length].to_vec(),
            }],
            4,
        )
        .await
        .remove(0)
    };
    let before = infer(32).await;
    let long = infer(tokens.len()).await;
    assert!(long.iter().any(|value| value.abs() > 0.0));
    let after = infer(32).await;
    assert_embedding_close(&after, &before);
    assert_context_worker_ownership(&loaded).await;
}
