use std::{env, path::PathBuf};

use crate::{
    domain::{
        Content,
        model::{Device, EmbeddingPurpose, ModelConfig},
    },
    models::{
        backends::create_embedding_model,
        runtime::ModelComputeRuntime,
        spi::{EmbeddingOptions, EmbeddingPrepareOptions},
    },
};

const CI_MODELS: [&str; 3] = [
    "local/potion-code-16m-v2",
    "local/all-minilm-l6-v2",
    "local/embeddinggemma-300m",
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
        device => panic!("ZVEC_GREP_SMOKE_DEVICE must be unset or cuda, got {device:?}"),
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
        assert_eq!(vector.len(), model.info().dimension);
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm = vector
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            .sqrt();
        assert!(norm > 0.0 && norm.is_finite());
    }
}
