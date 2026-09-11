use std::{env, path::PathBuf};

use serde::Deserialize;
use tempfile::TempDir;

use crate::models::{
    factory::create_embedding_model,
    spi::{CreateEmbeddingModelOptions, EmbeddingInput, EmbeddingOptions, EmbeddingPurpose},
};

#[derive(Deserialize)]
struct TypeScriptOracle {
    source: String,
    reference: String,
    revision: String,
    purpose: String,
    texts: Vec<String>,
    vectors: Vec<Vec<f32>>,
    truncated: Vec<usize>,
}

#[tokio::test]
#[ignore = "downloads the pinned public Model2Vec assets when the cache is empty"]
async fn model2vec_matches_the_main_typescript_vector_bit_for_bit() {
    let oracle: TypeScriptOracle =
        serde_json::from_str(include_str!("fixtures/model2vec-main-oracle.json"))
            .expect("checked-in TypeScript oracle must be valid JSON");
    assert_eq!(oracle.source, "zg-main");
    assert_eq!(oracle.revision, "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b");
    assert_eq!(oracle.purpose, "query");

    let temporary_cache = TempDir::new().expect("temporary cache should be created");
    let cache_dir = env::var_os("ZVEC_GREP_MODEL_CACHE")
        .map_or_else(|| temporary_cache.path().to_path_buf(), PathBuf::from);
    let model = create_embedding_model(
        &oracle.reference,
        Some(CreateEmbeddingModelOptions {
            model_cache_dir: Some(cache_dir),
            ..CreateEmbeddingModelOptions::default()
        }),
    )
    .expect("pinned Model2Vec model must be available");
    let inputs = oracle
        .texts
        .iter()
        .cloned()
        .map(EmbeddingInput::text)
        .collect::<Vec<_>>();
    let result = model
        .embed(
            &inputs,
            EmbeddingOptions {
                purpose: Some(EmbeddingPurpose::Query),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect("Rust Model2Vec embedding must complete");

    assert_eq!(result.truncated, oracle.truncated);
    assert_eq!(result.vectors, oracle.vectors);
}
