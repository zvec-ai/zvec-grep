use super::*;
use crate::models::catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};

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
fn metal_context_pool_caps_physical_workers_without_ignoring_user_limit() {
    assert_eq!(physical_context_worker_limit(true, 1), 1);
    assert_eq!(physical_context_worker_limit(true, 2), 2);
    assert_eq!(physical_context_worker_limit(true, 4), 2);
    assert_eq!(physical_context_worker_limit(false, 4), 4);
}

#[test]
fn context_batch_capacity_tracks_tokens_instead_of_full_context() {
    assert_eq!(llama_batch_capacity(7, 2_048).expect("capacity"), 32);
    assert_eq!(llama_batch_capacity(65, 2_048).expect("capacity"), 128);
    assert_eq!(llama_batch_capacity(2_048, 2_048).expect("capacity"), 2_048);
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
