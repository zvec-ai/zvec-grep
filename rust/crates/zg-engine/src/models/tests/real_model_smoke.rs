use std::{env, path::PathBuf};

use crate::{
    domain::{
        Content,
        model::{Device, EmbeddingPurpose, ModelConfig},
    },
    models::{
        backends::create_embedding_model,
        runtime::ModelComputeRuntime,
        spi::{EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions},
    },
};

const CI_MODELS: [&str; 9] = [
    "local/bekko-256-int8",
    "local/potion-code-16m-v2",
    "local/all-minilm-l6-v2",
    "local/embeddinggemma-300m",
    "local/bge-small-en-v1.5",
    "local/multilingual-e5-small",
    "local/jina-embeddings-v2-base-code",
    "local/gte-modernbert-base",
    "local/nomic-embed-text-v1.5",
];

#[tokio::test]
#[ignore = "downloads pinned public model artifacts; run by Rust Models CI / Rust CUDA CI"]
async fn selected_local_model_downloads_loads_and_embeds() {
    let reference =
        env::var("ZVEC_GREP_SMOKE_MODEL").expect("ZVEC_GREP_SMOKE_MODEL must select the CI model");
    assert!(
        CI_MODELS.contains(&reference.as_str()),
        "unsupported real-model smoke reference: {reference}"
    );
    let cache_dir = env::var_os("ZVEC_GREP_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("ZVEC_GREP_MODEL_CACHE must isolate downloaded smoke artifacts");

    let devices = match env::var("ZVEC_GREP_SMOKE_DEVICE") {
        Ok(device) if device == "cpu" => vec![Device::Cpu],
        Ok(device) if device == "cuda" && !cfg!(feature = "cuda") => {
            panic!("CUDA smoke requires --features cuda");
        }
        Ok(device) if device == "cuda" => {
            assert!(
                reference != "local/potion-code-16m-v2",
                "model2vec has no CUDA backend"
            );
            // The CUDA workflow checks native offload / graph-placement evidence
            // with the same validator as the packaged CLI, and rejects fallback.
            tracing_subscriber::fmt()
                .with_env_filter("warn,zg_engine=debug,ort=trace,llama-cpp-2=info")
                .with_ansi(false)
                .with_writer(std::io::stderr)
                .try_init()
                .expect("CUDA smoke must retain native execution evidence");
            vec![Device::Cuda]
        }
        Err(env::VarError::NotPresent) => {
            let mut devices = vec![Device::Cpu];
            if cfg!(all(target_os = "linux", target_arch = "x86_64"))
                && reference == "local/all-minilm-l6-v2"
            {
                // Match the Node.js smoke coverage that caught accelerator
                // initialization failures hidden by a forced CPU configuration.
                devices.push(Device::Auto);
            }
            devices
        }
        device => panic!("ZVEC_GREP_SMOKE_DEVICE must be unset, cpu or cuda, got {device:?}"),
    };

    for device in devices {
        download_load_and_embed(&reference, cache_dir.clone(), device).await;
    }
}

async fn download_load_and_embed(reference: &str, cache_dir: PathBuf, device: Device) {
    let model = create_embedding_model(
        reference,
        Some(ModelConfig {
            cache_dir: Some(cache_dir),
            device: Some(device),
            ..ModelConfig::default()
        }),
        ModelComputeRuntime::shared(),
    )
    .expect("CI model must resolve through the catalog factory");

    model
        .prepare(EmbeddingPrepareOptions::default())
        .await
        .expect("CI model must download and load");
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
        .expect("CI model must run real inference");

    assert_eq!(result.vectors.len(), 2);
    assert!(result.truncated.is_empty());
    for vector in &result.vectors {
        assert_eq!(vector.len(), model.info().dimension());
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm = vector
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            .sqrt();
        assert!(norm > 0.0 && norm.is_finite());
    }
    println!(
        "MODEL_SMOKE_OK model={reference} dimension={}",
        model.info().dimension()
    );
    if reference == "local/bekko-256-int8" {
        assert_bekko_reference(model.as_ref()).await;
        super::bekko_parity::assert_onnx_parity(model.as_ref()).await;
    }
}

async fn assert_bekko_reference(model: &dyn EmbeddingModel) {
    // Pinned c18167b8 graph/tokenizer, Python onnxruntime 1.21.0 + tokenizers
    // 0.23.1: encode_batch(add_special_tokens=False), zero/right padding and
    // attention mask, sentence_embedding output, then numpy L2 normalization.
    let texts = [
        "find authentication middleware",
        "长春光华学院在哪",
        "日本で一番高い山は？",
    ];
    let expected: [[f64; 8]; 3] = [
        [
            0.020_006_887_6,
            0.083_093_427_1,
            -0.021_326_243_9,
            -0.138_030_707_8,
            0.034_431_058_9,
            0.042_632_363_7,
            -0.065_024_748_4,
            0.115_143_723_8,
        ],
        [
            -0.051_363_248_4,
            -0.112_774_357_2,
            -0.076_780_267_1,
            -0.040_918_748_8,
            -0.090_762_600_3,
            0.105_193_056_2,
            0.092_324_987_1,
            -0.107_521_846_9,
        ],
        [
            0.080_806_985_5,
            -0.093_681_342_9,
            0.081_718_646_0,
            0.011_472_640_6,
            -0.080_582_708_1,
            0.007_296_192_9,
            -0.175_629_288_0,
            -0.047_684_241_1,
        ],
    ];
    let inputs = texts.map(|text| vec![Content::Text(text.to_owned())]);
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let error = model
        .embed(
            &inputs,
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect_err("cancelled prepared model");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
        let result = model
            .embed(
                &inputs,
                EmbeddingOptions {
                    purpose,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("model remains usable after cancellation");
        for (vector, reference) in result.vectors.iter().zip(expected) {
            assert_eq!(vector.len(), 256);
            for (actual, expected) in vector.iter().zip(reference) {
                assert!(
                    (f64::from(*actual) - expected).abs() < 1e-6,
                    "Bekko differs from Python ONNX: {actual} vs {expected}"
                );
            }
            let norm = vector.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
            assert!((norm - 1.0).abs() < 1e-6);
        }
    }
}
