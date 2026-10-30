use super::*;
use crate::models::catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};

#[test]
fn cpu_model_parameters_exclude_gpu_devices_and_layers() {
    let _registry = lock_backend_registry();
    let params = cpu_model_params().expect("CPU model parameters");
    assert_eq!(params.n_gpu_layers(), 0);
    assert!(params.devices().is_empty());
}

#[test]
fn cancelled_model_preparation_is_rejected() {
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(check_cancelled(Some(&signal)).is_err());
}

#[test]
fn cuda_release_defaults_to_automatic_device_selection() {
    let expected = if cfg!(all(
        feature = "cuda",
        any(target_os = "linux", target_os = "windows"),
        target_arch = "x86_64"
    )) {
        Some(Device::Auto)
    } else {
        None
    };
    assert_eq!(resolve_device(None), expected);
    assert_eq!(resolve_device(Some(Device::Cpu)), Some(Device::Cpu));
}

fn entry(reference: &str) -> LlamaCppConfig {
    get_embedding_model_catalog_entry(reference)
        .and_then(EmbeddingCatalogEntry::llama_cpp_config)
        .expect("llama.cpp catalog entry")
}

#[test]
fn formats_embeddinggemma_and_qwen_inputs_like_main() {
    assert_eq!(
        format_text("hello", EmbeddingPurpose::Query, "embeddinggemma"),
        "task: search result | query: hello"
    );
    assert_eq!(
        format_text("hello", EmbeddingPurpose::Document, "embeddinggemma"),
        "title: none | text: hello"
    );
    assert_eq!(
        format_text("hello", EmbeddingPurpose::Query, "qwen3"),
        "Instruct: Retrieve relevant documents for the given query\nQuery: hello"
    );
    assert_eq!(
        format_text("hello", EmbeddingPurpose::Document, "qwen3"),
        "hello"
    );
}

#[test]
fn pins_main_compatible_sources_and_cache_name() {
    let entry = entry("local/embeddinggemma-300m");
    assert_eq!(
        entry.download.hugging_face.revision,
        "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12"
    );
    assert_eq!(
        entry.cache_file,
        "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"
    );
}

#[test]
fn user_concurrency_controls_cpu_threads_per_context() {
    assert_eq!(threads_per_context(true, 4), 0);
    assert_eq!(threads_per_context(false, 1), 0);
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    assert_eq!(
        threads_per_context(false, 4),
        i32::try_from((cores / 4).max(1)).unwrap_or(i32::MAX)
    );
}

#[test]
fn context_pool_honors_user_limit_and_caps_at_eight() {
    assert_eq!(physical_context_worker_limit(true, 1), 1);
    assert_eq!(physical_context_worker_limit(true, 2), 2);
    assert_eq!(physical_context_worker_limit(true, 4), 4);
    assert_eq!(physical_context_worker_limit(false, 4), 4);
    assert_eq!(physical_context_worker_limit(true, 24), 8);
    assert_eq!(physical_context_worker_limit(false, 24), 8);
}

#[test]
fn context_batch_capacity_tracks_tokens_instead_of_full_context() {
    assert_eq!(llama_batch_capacity(7, 2_048).expect("capacity"), 32);
    assert_eq!(llama_batch_capacity(65, 2_048).expect("capacity"), 128);
    assert_eq!(llama_batch_capacity(2_048, 2_048).expect("capacity"), 2_048);
}

#[test]
fn embeddinggemma_micro_batch_covers_long_inputs_without_changing_qwen() {
    for tokens in [1, 32, 255, 256, 257, 1_024, 2_048] {
        let batch = llama_batch_capacity(tokens, 2_048).expect("batch capacity");
        let gemma = llama_micro_batch_capacity("gemma-embedding", batch);
        assert_eq!(gemma, batch);
        assert!(usize::try_from(gemma).expect("micro batch") >= tokens);
        assert_eq!(llama_micro_batch_capacity("qwen3", batch), batch.min(256));
    }
}

#[test]
fn multisequence_batching_is_limited_to_gpu_gemma() {
    assert!(use_multisequence_batching("gemma-embedding", true));
    assert!(!use_multisequence_batching("gemma-embedding", false));
    assert!(!use_multisequence_batching("qwen3", true));
    assert!(!use_multisequence_batching("qwen3", false));
}

#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with embeddinggemma; compares batched and individual vectors"]
async fn cached_embeddinggemma_cpu_batches_preserve_vectors_and_context_limits() {
    embeddinggemma_batches_preserve_vectors_and_context_limits(Device::Cpu).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires cached embeddinggemma and Metal; no CPU fallback"]
async fn cached_embeddinggemma_metal_batches_preserve_vectors_and_context_limits() {
    embeddinggemma_batches_preserve_vectors_and_context_limits(Device::Metal).await;
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "cuda"))]
#[tokio::test]
#[ignore = "requires cached embeddinggemma and CUDA plugins; run in a fresh process; no CPU fallback"]
async fn cached_embeddinggemma_cuda_batches_preserve_vectors_and_context_limits() {
    embeddinggemma_batches_preserve_vectors_and_context_limits(Device::Cuda).await;
}

async fn embeddinggemma_batches_preserve_vectors_and_context_limits(device: Device) {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(device),
            ..ModelConfig::default()
        },
        ModelComputeRuntime::shared(),
    );
    let loaded = model.ensure_loaded(None, None).await.expect("cached model");
    assert_eq!(loaded.gpu, device != Device::Cpu);
    let inputs = gemma_batch_fixture(&loaded.model);
    assert_eq!(inputs[4].tokens.len(), 2_048);
    let prepared = || PreparedLlamaBatch {
        inputs: inputs.clone(),
        truncated: vec![4],
    };
    let mut expected = Vec::new();
    for input in &inputs {
        let result = loaded
            .contexts
            .run(
                PreparedLlamaBatch {
                    inputs: vec![PreparedLlamaInput {
                        index: 0,
                        tokens: input.tokens.clone(),
                    }],
                    truncated: Vec::new(),
                },
                2_048,
                1,
                threads_per_context(loaded.gpu, 1),
                None,
            )
            .expect("individual inference");
        expected.push(result.vectors[0].clone());
    }
    if loaded.gpu {
        assert_gemma_sequence_isolation(&loaded, &inputs);
    }
    for concurrency in [1, 2, 4, 8] {
        let threads = threads_per_context(loaded.gpu, concurrency);
        let result = loaded
            .contexts
            .run(prepared(), 2_048, concurrency, threads, None)
            .expect("batched inference");
        assert_eq!(result.truncated, [4]);
        for (actual, expected) in result.vectors.iter().zip(&expected) {
            assert_embedding_close(actual, expected);
        }
        assert_eq!(result.vectors.len(), inputs.len());
        let state = lock_llama_mutex(&loaded.contexts.state);
        assert_eq!(state.workers.len(), concurrency);
        assert!(
            state
                .workers
                .iter()
                .all(|worker| !worker.busy.load(Ordering::Acquire))
        );
        drop(state);

        // A larger sequence/token profile must remain safe for a later single query.
        let result = loaded
            .contexts
            .run(
                PreparedLlamaBatch {
                    inputs: vec![inputs[0].clone()],
                    truncated: Vec::new(),
                },
                2_048,
                concurrency,
                threads,
                None,
            )
            .expect("reuse after batch");
        assert_embedding_close(&result.vectors[0], &expected[0]);
    }
    assert_gemma_batch_failure_recovery(&loaded, &inputs, &expected);
}

fn gemma_batch_fixture(model: &LlamaModel) -> Vec<PreparedLlamaInput> {
    let texts = [
        "find authentication middleware".to_owned(),
        "parse configuration and handle errors".to_owned(),
        "sort vector search results".repeat(10),
        "long sequence of parser tokens ".repeat(100),
        "validate access permissions ".repeat(2_048),
        "find authentication middleware".to_owned(),
        "implement a cache eviction policy".repeat(20),
        "a separate document about image processing".to_owned(),
    ];
    texts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut tokens = model.str_to_token(text, AddBos::Always).expect("tokens");
            tokens.truncate(2_048);
            PreparedLlamaInput { index, tokens }
        })
        .collect()
}

fn assert_embedding_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), 768);
    assert_eq!(actual.len(), expected.len());
    assert!(actual.iter().all(|value| value.is_finite()));
    let norm = expected.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
    let error = actual
        .iter()
        .zip(expected)
        .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
        .sum::<f64>();
    let relative_l2 = (error / norm).sqrt();
    let actual_norm = actual.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
    let dot = actual
        .iter()
        .zip(expected)
        .map(|(&a, &b)| f64::from(a) * f64::from(b))
        .sum::<f64>();
    let cosine = dot / (actual_norm * norm).sqrt();
    eprintln!("batch equivalence relative_l2={relative_l2:.8} cosine={cosine:.9}");
    // Allow small GPU rounding differences; verify sequence isolation separately.
    assert!(
        cosine > 0.99999 && relative_l2 < 0.005,
        "batched output changed: relative_l2={relative_l2} cosine={cosine}"
    );
}

fn assert_gemma_sequence_isolation(loaded: &LoadedLlamaModel, inputs: &[PreparedLlamaInput]) {
    for target in [0, 6] {
        let mut pair = vec![inputs[target].clone(), inputs[1].clone()];
        pair[0].index = 0;
        pair[1].index = 1;
        let infer = |inputs| {
            loaded
                .contexts
                .run(
                    PreparedLlamaBatch {
                        inputs,
                        truncated: Vec::new(),
                    },
                    2_048,
                    1,
                    threads_per_context(loaded.gpu, 1),
                    None,
                )
                .expect("sequence isolation inference")
                .vectors
        };
        let before = infer(pair.clone());
        pair[1].tokens.reverse();
        let after = infer(pair);
        // Changing an unrelated sequence must not affect the target embedding.
        let difference = before[0]
            .iter()
            .zip(&after[0])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        eprintln!("sequence isolation max_absolute_difference={difference}");
        assert!(difference < 1e-5, "cross-sequence interference");
        assert_ne!(before[1], after[1]);
    }
}

fn assert_gemma_batch_failure_recovery(
    loaded: &LoadedLlamaModel,
    inputs: &[PreparedLlamaInput],
    expected: &[Vec<f32>],
) {
    let prepared = || PreparedLlamaBatch {
        inputs: inputs.to_vec(),
        truncated: vec![4],
    };
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(
        loaded
            .contexts
            .run(prepared(), 2_048, 4, 3, Some(&signal))
            .is_err()
    );
    // Invalid native tokens return an error without poisoning the leased context.
    assert!(
        loaded
            .contexts
            .run(
                PreparedLlamaBatch {
                    inputs: vec![PreparedLlamaInput {
                        index: 0,
                        tokens: vec![LlamaToken::new(-1)]
                    }],
                    truncated: Vec::new(),
                },
                2_048,
                4,
                3,
                None
            )
            .is_err()
    );
    let result = loaded
        .contexts
        .run(prepared(), 2_048, 4, 3, None)
        .expect("recover after failure");
    for (actual, expected) in result.vectors.iter().zip(expected) {
        assert_embedding_close(actual, expected);
    }
}

#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the embeddinggemma GGUF; exercises up to 2048 tokens"]
async fn cached_embeddinggemma_cpu_handles_long_inputs_and_reuse() {
    embeddinggemma_handles_long_inputs_and_reuse(Device::Cpu).await;
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "cuda"))]
#[tokio::test]
#[ignore = "requires cached embeddinggemma GGUF and a working NVIDIA GPU; run in a fresh process"]
async fn cached_embeddinggemma_cuda_handles_long_inputs_and_reuse() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,zg_engine=debug,llama-cpp-2=info")
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init()
        .expect("CUDA long-input regression must retain native execution evidence");
    embeddinggemma_handles_long_inputs_and_reuse(Device::Cuda).await;
}

async fn embeddinggemma_handles_long_inputs_and_reuse(device: Device) {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(device),
            ..ModelConfig::default()
        },
        ModelComputeRuntime::shared(),
    );
    let loaded = model.ensure_loaded(None, None).await.expect("cached model");
    assert_eq!(loaded.gpu, device != Device::Cpu);
    let tokens = loaded
        .model
        .str_to_token(&"authentication middleware ".repeat(2_048), AddBos::Always)
        .expect("long input tokens");
    assert!(tokens.len() >= 2_048);
    let prepared = |length| PreparedLlamaBatch {
        inputs: vec![PreparedLlamaInput {
            index: 0,
            tokens: tokens[..length].to_vec(),
        }],
        truncated: Vec::new(),
    };
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        loaded
            .contexts
            .run(prepared(257), 2_048, 1, 4, Some(&cancelled))
            .is_err()
    );
    let mut first: Option<Vec<f32>> = None;
    // Cross the former 256-token limit, grow the same worker's context, and
    // finally reuse its larger context for the original short input.
    for length in [32, 256, 257, 1_024, 2_048, 32] {
        let result = loaded
            .contexts
            .run(prepared(length), 2_048, 1, 4, None)
            .expect("full-sequence encoder inference");
        assert!(result.truncated.is_empty());
        assert_eq!(result.vectors.len(), 1);
        let vector = &result.vectors[0];
        assert_eq!(vector.len(), 768);
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm = vector.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
        assert!(norm > 0.0);
        if length == 32 {
            if let Some(previous) = first.as_ref() {
                let dot = previous
                    .iter()
                    .zip(vector)
                    .map(|(&a, &b)| f64::from(a) * f64::from(b))
                    .sum::<f64>();
                let previous_norm = previous.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
                assert!(dot / (previous_norm * norm).sqrt() > 0.999);
            } else {
                first = Some(vector.clone());
            }
        }
    }
}

#[tokio::test]
async fn rejects_and_removes_invalid_gguf_files() {
    let directory = tempfile::tempdir().expect("temporary cache");
    let path = directory.path().join("bad.gguf");
    fs::write(&path, b"<!doctype html><title>failure</title>")
        .await
        .expect("fixture");
    let error = validate_gguf_file(&path, "hf:test/model.gguf")
        .await
        .expect_err("invalid GGUF must fail");
    assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
    assert!(!path.exists());
}

#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the embeddinggemma GGUF"]
async fn cached_embeddinggemma_runs_real_llama_cpp_inference() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = model
        .embed(
            &[
                vec![Content::Text("find authentication middleware".to_owned())],
                vec![Content::Text("parse a configuration file".to_owned())],
            ],
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("real llama.cpp embedding");
    assert_eq!(result.vectors.len(), 2);
    assert!(result.vectors.iter().all(|vector| vector.len() == 768));
    // The Rust and Node bindings track different llama.cpp revisions, so
    // compare the raw-vector scale and representative coordinates with a
    // small cross-runtime tolerance instead of requiring bit equality.
    let main_first_values = [
        [
            -163.557_48,
            2.360_403_8,
            28.599_663,
            51.072_44,
            -36.060_03,
            -59.653_484,
            -53.783_993,
            -62.748_837,
            49.552_666,
            -7.057_061,
            11.277_911,
            -19.936_779,
        ],
        [
            -171.828_66,
            -21.072_908,
            -12.103_56,
            -41.734_7,
            13.669_633,
            20.436_523,
            -33.258_686,
            13.457_932,
            16.871_897,
            1.374_025,
            -40.426_422,
            -39.468_83,
        ],
    ];
    let main_norms = [1_060.960_9_f32, 1_086.006_5_f32];
    for (index, vector) in result.vectors.iter().enumerate() {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!(
            (norm - main_norms[index]).abs() < 2.0,
            "vector={index} norm={norm} expected={}",
            main_norms[index]
        );
        for (offset, (&actual, &expected)) in
            vector.iter().zip(&main_first_values[index]).enumerate()
        {
            assert!(
                (actual - expected).abs() < 2.5,
                "vector={index} offset={offset} actual={actual} expected={expected}"
            );
        }
    }
    assert!(result.truncated.is_empty());
}

#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "cuda"))]
#[tokio::test]
#[ignore = "requires installed CUDA plugins, a working NVIDIA GPU and cached embeddinggemma GGUF; run in a fresh process"]
async fn cached_embeddinggemma_uses_cuda() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    // CPU initialization must not consume the one-time optional GPU registration.
    // Keep the CPU model alive while loading the GPU model: no plugin unloading.
    let cpu = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache.clone()),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = cpu
        .embed(
            &[vec![Content::Text("find relevant code".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect("CPU inference before CUDA registration");
    assert_eq!(result.vectors[0].len(), 768);
    assert!(
        cpu.state
            .lock()
            .await
            .as_ref()
            .is_some_and(|loaded| !loaded.gpu)
    );
    let model = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(Device::Cuda),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = model
        .embed(
            &[vec![Content::Text("find relevant code".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect("CUDA llama.cpp inference");

    assert_eq!(result.vectors[0].len(), 768);
    assert!(result.vectors[0].iter().all(|value| value.is_finite()));
    let loaded = model.state.lock().await;
    assert!(
        loaded.as_ref().is_some_and(|loaded| loaded.gpu),
        "CUDA request unexpectedly fell back to CPU"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the embeddinggemma GGUF"]
async fn cached_embeddinggemma_uses_metal_and_supports_concurrency() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/embeddinggemma-300m"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(Device::Metal),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = model
        .embed(
            &[vec![Content::Text("find relevant code".to_owned())]],
            EmbeddingOptions {
                execution_concurrency: 2,
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("Metal llama.cpp inference");
    assert_eq!(result.vectors[0].len(), 768);
    let first_contents = [vec![Content::Text("authentication middleware".to_owned())]];
    let second_contents = [vec![Content::Text("configuration parser".to_owned())]];
    let first = model.embed(
        &first_contents,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let second = model.embed(
        &second_contents,
        EmbeddingOptions {
            execution_concurrency: 2,
            ..EmbeddingOptions::default()
        },
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        first.expect("first concurrent embedding").vectors[0].len(),
        768
    );
    assert_eq!(
        second.expect("second concurrent embedding").vectors[0].len(),
        768
    );
    let loaded = model.state.lock().await;
    let using_gpu = loaded.as_ref().is_some_and(|loaded| loaded.gpu);
    assert!(using_gpu, "Metal request unexpectedly fell back to CPU");
}

#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with the Qwen3 embedding GGUF"]
async fn cached_qwen3_runs_real_llama_cpp_inference() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let model = LlamaCppEmbeddingModel::new(
        entry("local/qwen3-embedding-0.6b"),
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let result = model
        .embed(
            &[vec![Content::Text(
                "find authentication middleware".to_owned(),
            )]],
            EmbeddingOptions {
                purpose: EmbeddingPurpose::Query,
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("real Qwen3 llama.cpp embedding");
    assert_eq!(result.vectors.len(), 1);
    assert_eq!(result.vectors[0].len(), 1_024);
    assert!(result.vectors[0].iter().all(|value| value.is_finite()));
    assert!(result.truncated.is_empty());
}
