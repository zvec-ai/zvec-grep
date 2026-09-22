use super::*;

#[test]
fn maps_devices_to_native_rust_execution_providers() {
    assert_eq!(
        resolve_execution_provider(None),
        TransformersExecutionProvider::Cpu
    );
    assert_eq!(
        resolve_execution_provider(Some(Device::Cpu)),
        TransformersExecutionProvider::Cpu
    );
    #[cfg(target_os = "macos")]
    {
        assert_eq!(
            resolve_execution_provider(Some(Device::Metal)),
            TransformersExecutionProvider::Cpu
        );
        assert_eq!(
            resolve_execution_provider(Some(Device::Auto)),
            TransformersExecutionProvider::Cpu
        );
    }
    #[cfg(not(target_os = "macos"))]
    assert_eq!(
        resolve_execution_provider(Some(Device::Metal)),
        TransformersExecutionProvider::CoreMl
    );
    assert_eq!(
        resolve_execution_provider(Some(Device::Vulkan)),
        TransformersExecutionProvider::WebGpu
    );
    assert_eq!(
        resolve_execution_provider(Some(Device::Cuda)),
        TransformersExecutionProvider::Cuda
    );
}

#[test]
fn coreml_uses_one_physical_session_for_all_user_concurrency() {
    assert_eq!(
        physical_session_limit(TransformersExecutionProvider::CoreMl, 1),
        1
    );
    assert_eq!(
        physical_session_limit(TransformersExecutionProvider::CoreMl, 4),
        1
    );
    assert_eq!(
        physical_session_limit(TransformersExecutionProvider::Cpu, 4),
        4
    );
}

#[test]
fn coreml_batch_merge_repads_rows_and_preserves_indexes() {
    let first = PreparedBatch {
        input_ids: vec![101, 102],
        attention_mask: vec![1, 1],
        token_type_ids: vec![0, 0],
        position_ids: vec![0, 1],
        padding_input_id: 0,
        batch_size: 1,
        sequence_length: 2,
        truncated: vec![0],
    };
    let second = PreparedBatch {
        input_ids: vec![201, 202, 203, 301, 302, 0],
        attention_mask: vec![1, 1, 1, 1, 1, 0],
        token_type_ids: vec![0; 6],
        position_ids: vec![0, 1, 2, 0, 1, 2],
        padding_input_id: 0,
        batch_size: 2,
        sequence_length: 3,
        truncated: vec![1],
    };
    let merged = merge_prepared_batches(&[&first, &second]).expect("merged batch");
    assert_eq!(merged.batch_size, 3);
    assert_eq!(merged.sequence_length, 3);
    assert_eq!(merged.input_ids, [101, 102, 0, 201, 202, 203, 301, 302, 0]);
    assert_eq!(merged.attention_mask, [1, 1, 0, 1, 1, 1, 1, 1, 0]);
    assert_eq!(merged.truncated, [0, 2]);
}
use crate::domain::model::Metric;

fn entry(pooling: &'static str, normalize: bool) -> TransformersConfig {
    let download =
        crate::models::catalog::get_embedding_model_catalog_entry("local/multilingual-e5-small")
            .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
            .expect("fixture download metadata")
            .download;
    TransformersConfig {
        reference: "local/test-transformer",
        provider: "local",
        model: "test-transformer",
        repo: "test/model-ONNX",
        revision: "0123456789abcdef",
        download,
        dtype: "q8",
        dimension: 3,
        metric: Metric::Cosine,
        pooling,
        normalize,
        query_prefix: Some("query: "),
        document_prefix: Some("passage: "),
        max_input_tokens: 2,
        max_batch_size: 4,
    }
}

#[test]
fn mean_pooling_and_normalization_match_transformers_pipeline() {
    let values = [
        1.0, 0.0, 0.0, // first token
        0.0, 1.0, 0.0, // second token
        9.0, 9.0, 9.0, // padding
    ];
    let vectors =
        pool_output(&[1, 3, 3], &values, &[1, 1, 0], 1, 3, entry("mean", true)).expect("pooling");
    assert!((vectors[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert!((vectors[0][1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert!(vectors[0][2].abs() < f32::EPSILON);
}

#[test]
fn cls_pooling_and_matrix_output_are_supported() {
    let cls = pool_output(
        &[1, 2, 3],
        &[1.0, 2.0, 3.0, 8.0, 8.0, 8.0],
        &[1, 1],
        1,
        2,
        entry("cls", false),
    )
    .expect("CLS pooling");
    assert_eq!(cls, [[1.0, 2.0, 3.0]]);
    let matrix = pool_output(&[1, 3], &[1.0, 2.0, 3.0], &[1], 1, 1, entry("mean", false))
        .expect("matrix output");
    assert_eq!(matrix, [[1.0, 2.0, 3.0]]);
}

#[test]
fn prefixes_and_catalog_info_match_main() {
    let model = TransformersEmbeddingModel::new(
        entry("mean", true),
        ModelConfig::default(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    assert_eq!(model.info().max_input_tokens, Some(2));
    assert_eq!(onnx_artifact("q4").expect("q4"), "onnx/model_q4.onnx");
    assert_eq!(
        onnx_artifact("q8").expect("q8"),
        "onnx/model_quantized.onnx"
    );
}

#[tokio::test]
async fn cancelled_initial_load_preserves_cancellation_code() {
    let cache = tempfile::tempdir().expect("model cache");
    let model = TransformersEmbeddingModel::new(
        entry("mean", true),
        ModelConfig {
            cache_dir: Some(cache.path().to_owned()),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let signal = CancellationToken::new();
    signal.cancel();
    let error = model
        .embed(
            &[vec![Content::Text("cancel before loading".to_owned())]],
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect_err("cancelled load");

    assert_eq!(error.code(), crate::EngineError::CANCELLED);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with a pinned ONNX model"]
async fn cached_minilm_runs_real_onnx_inference() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let entry = crate::models::catalog::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
        .expect("catalog entry");
    let model = TransformersEmbeddingModel::new(
        entry,
        ModelConfig {
            cache_dir: Some(cache),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    let loaded = model
        .ensure_loaded(None, None)
        .await
        .expect("download and load cached ONNX model");
    let tokenizer = Tokenizer::from_file(model.model_directory().join("tokenizer.json"))
        .expect("cached tokenizer");
    let prepared = prepare_batch(
        tokenizer,
        &[
            "find authentication middleware".to_owned(),
            "parse a configuration file".to_owned(),
        ],
        entry,
    )
    .expect("tokenize main oracle inputs");
    assert_eq!(
        prepared.input_ids,
        [
            101, 2424, 27280, 2690, 8059, 102, 0, 101, 11968, 3366, 1037, 9563, 5371, 102,
        ]
    );
    assert_eq!(
        prepared.attention_mask,
        [1, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1]
    );
    assert!(prepared.token_type_ids.iter().all(|&value| value == 0));
    let result = model
        .embed(
            &[
                vec![Content::Text("find authentication middleware".to_owned())],
                vec![Content::Text("parse a configuration file".to_owned())],
            ],
            EmbeddingOptions::default(),
        )
        .await
        .expect("real ONNX embedding");
    assert_eq!(result.vectors.len(), 2);
    assert!(result.vectors.iter().all(|vector| vector.len() == 384));
    let main_first_values = [
        [
            -0.082_118_884,
            0.026_001_254,
            -0.016_771_38,
            -0.107_571_7,
            0.079_822_97,
            -0.018_354_345,
            0.035_343_368,
            0.006_443_004_6,
            -0.010_101_517,
            0.013_123_883,
            0.013_252_53,
            -0.035_912_04,
        ],
        [
            0.027_389_433,
            0.040_037_83,
            -0.059_505_902,
            -0.038_954_25,
            -0.012_910_471,
            -0.016_948_676,
            0.027_840_037,
            0.076_388_21,
            -0.106_726_564,
            -0.027_350_506,
            0.054_113_51,
            0.034_919_977,
        ],
    ];
    for (index, vector) in result.vectors.iter().enumerate() {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
        for (offset, (&actual, &expected)) in
            vector.iter().zip(&main_first_values[index]).enumerate()
        {
            assert!(
                (actual - expected).abs() < 5e-3,
                "vector={index} offset={offset} actual={actual} expected={expected}"
            );
        }
    }
    assert!(result.truncated.is_empty());

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let first_loaded = Arc::clone(&loaded);
    let first_barrier = Arc::clone(&barrier);
    let first = model.compute_runtime.run(move || {
        first_loaded.sessions.run(2, |_session| {
            first_barrier.wait();
            Ok(())
        })
    });
    let second_loaded = Arc::clone(&loaded);
    let second = model.compute_runtime.run(move || {
        second_loaded.sessions.run(2, |_session| {
            barrier.wait();
            Ok(())
        })
    });
    let (first, second) = tokio::join!(first, second);
    first
        .expect("first compute task")
        .expect("first pooled session");
    second
        .expect("second compute task")
        .expect("second pooled session");
    assert_eq!(lock_std_mutex(&loaded.sessions.state).sessions.len(), 2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires ZVEC_GREP_TEST_MODEL_CACHE with a pinned ONNX model"]
async fn cached_minilm_routes_metal_to_cpu_with_warning() {
    let cache = env::var_os("ZVEC_GREP_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_TEST_MODEL_CACHE must point at the model cache");
    let entry = crate::models::catalog::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::catalog::EmbeddingCatalogEntry::transformers_config)
        .expect("catalog entry");
    let warnings = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&warnings);
    let model = TransformersEmbeddingModel::new(
        entry,
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
                on_progress: Some(Arc::new(move |progress| {
                    if let ModelProgress::Warning { message, .. } = progress {
                        lock_std_mutex(&captured).push(message);
                    }
                })),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("ORT CPU inference");
    assert_eq!(result.vectors[0].len(), entry.dimension);
    let loaded = model.state.lock().await;
    let provider = loaded
        .as_ref()
        .map(|loaded| loaded.sessions.provider())
        .expect("loaded model");
    assert_eq!(provider, TransformersExecutionProvider::Cpu);
    let warnings = lock_std_mutex(&warnings);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("using ORT CPU instead"));
}
