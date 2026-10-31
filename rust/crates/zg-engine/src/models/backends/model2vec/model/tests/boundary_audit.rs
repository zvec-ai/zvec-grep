//! Local CPU audit: copy the three pinned `Model2Vec` caches into an isolated
//! `ZVEC_GREP_MODEL_CACHE`, then run `cargo +1.98.0 test -p zg-engine
//! model2vec_local_boundary_audit --lib -- --ignored --nocapture`.
//! No download is needed: artifacts must exist and match the catalog hashes.
//!
//! Token counts are audit observations, not fields of `EmbeddingResult`. The
//! public result's `truncated` indices and actual vectors are checked separately.
//! The pinned Potion Code model card describes static embeddings without a
//! positional limit; 1024 is this application's budget, retained from catalog
//! change f247f7d (which deliberately replaced the earlier 8192 setting).
//! <https://huggingface.co/minishlab/potion-code-16M-v2/blob/e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b/README.md>
use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

#[tokio::test]
#[ignore = "requires an isolated local cache of all three pinned Model2Vec models; CPU only"]
async fn model2vec_local_boundary_audit() {
    let cache = std::env::var_os("ZVEC_GREP_MODEL_CACHE")
        .map(std::path::PathBuf::from)
        .expect("isolated local cache");
    let mut passed = true;
    for reference in [
        "local/potion-code-16m-v2",
        "local/potion-retrieval-32m",
        "local/potion-multilingual-128m",
    ] {
        let entry = crate::models::catalog::get_embedding_model_catalog_entry(reference)
            .and_then(crate::models::catalog::EmbeddingCatalogEntry::model2vec_config)
            .expect("Model2Vec catalog entry");
        let root = cache
            .join("model2vec")
            .join(entry.download.hugging_face.repo.replace('/', "--"))
            .join(entry.download.hugging_face.revision);
        let artifacts = verify_local_artifacts(&root, entry).await;
        let tokenizer_path = root.join("tokenizer/tokenizer.json");
        let mut original = Tokenizer::from_file(&tokenizer_path).expect("artifact tokenizer");
        let embedded_truncation = original.get_truncation().cloned();
        let serialized_length = original
            .encode("hello ".repeat(1025), false)
            .expect("serialized behavior")
            .len();
        original
            .with_truncation(None)
            .expect("unlimited reference tokenizer");
        original.with_padding(None);
        let hello = original
            .encode("hello", false)
            .expect("one token")
            .get_ids()
            .to_vec();
        let tail = original
            .encode("world", false)
            .expect("one distinct token")
            .get_ids()
            .to_vec();
        assert_eq!(hello.len(), 1);
        assert_eq!(tail.len(), 1);
        assert_ne!(hello, tail);
        let model = Model2VecEmbeddingModel::new(
            entry,
            ModelConfig {
                cache_dir: Some(cache.clone()),
                ..Default::default()
            },
            crate::models::runtime::ModelComputeRuntime::shared(),
        );
        model
            .prepare(EmbeddingPrepareOptions::default())
            .await
            .expect("prepare local CPU model");
        let limit = model
            .info()
            .max_input_tokens()
            .expect("declared token limit");
        let mut observations = Vec::new();
        for purpose in [EmbeddingPurpose::Document, EmbeddingPurpose::Query] {
            for target in [513, limit - 1, limit, limit + 1] {
                let (matches, observation) =
                    audit_boundary(&model, &original, (hello[0], tail[0]), purpose, target).await;
                passed &= matches;
                observations.push(observation);
            }
            let suffix_changes_vector = suffix_changes_vector(&model, purpose).await;
            passed &= suffix_changes_vector;
            observations.push(json!({"purpose":format!("{purpose:?}"),"suffix_513_to_1024_changes_vector":suffix_changes_vector}));
        }
        println!(
            "MODEL2VEC_BOUNDARY_AUDIT={}",
            json!({"model":reference,"revision":entry.revision,"limit":limit,"embedded_truncation":embedded_truncation,"serialized_tokenizer_length_for_1025":serialized_length,"artifacts":artifacts,"observations":observations})
        );
    }
    assert!(
        passed,
        "all model2vec length contracts and suffix vectors must match"
    );
}

async fn verify_local_artifacts(root: &Path, entry: Model2VecConfig) -> Vec<serde_json::Value> {
    let mut artifacts = Vec::new();
    for artifact in entry.download.artifacts {
        let path = root.join(if artifact.path == entry.tokenizer_file {
            "tokenizer/tokenizer.json"
        } else {
            artifact.path
        });
        let bytes = tokio::fs::read(&path)
            .await
            .expect("pinned local artifact must exist before prepare");
        let digest = hex::encode(Sha256::digest(&bytes));
        assert_eq!(digest, artifact.sha256);
        assert_eq!(
            u64::try_from(bytes.len()).expect("file length"),
            artifact.size
        );
        artifacts.push(json!({"path":path,"sha256":digest}));
    }
    artifacts
}

async fn audit_boundary(
    model: &Model2VecEmbeddingModel,
    original: &Tokenizer,
    tokens: (u32, u32),
    purpose: EmbeddingPurpose,
    target: usize,
) -> (bool, serde_json::Value) {
    let loaded = model
        .state
        .lock()
        .await
        .loaded
        .clone()
        .expect("loaded model");
    let limit = model.info().max_input_tokens().expect("declared limit");
    let text = "hello ".repeat(512) + &"world ".repeat(target - 512);
    let expected_ids = [vec![tokens.0; 512], vec![tokens.1; target - 512]].concat();
    assert_eq!(
        original
            .encode(text.as_str(), false)
            .expect("independent full count")
            .get_ids(),
        expected_ids
    );
    let actual_ids = loaded.tokenizer.encode(&text).expect("runtime tokenizer");
    let effective = actual_ids
        .iter()
        .take(limit)
        .filter(|id| Some(**id) != loaded.tokenizer.unknown_token_id())
        .count();
    let expected = super::super::embed_static_token_list(
        &expected_ids[..target.min(limit)],
        &loaded.table,
        model.entry.normalize,
    )
    .expect("reference pooling");
    let result = model
        .embed(
            &[vec![Content::Code(text)]],
            EmbeddingOptions {
                purpose,
                ..Default::default()
            },
        )
        .await
        .expect("boundary embedding");
    let vector_matches = result.vectors[0]
        .iter()
        .zip(&expected)
        .all(|(a, b)| (a - b).abs() < 1e-7);
    let expected_truncated = if target > limit { vec![0] } else { vec![] };
    let matches = actual_ids == expected_ids
        && effective == target.min(limit)
        && result.truncated == expected_truncated
        && vector_matches;
    (
        matches,
        json!({"purpose":format!("{purpose:?}"),"target":target,"tokenizer_returned_tokens":actual_ids.len(),"effective":effective,"truncated":result.truncated,"vector_matches":vector_matches,"passed":matches}),
    )
}

async fn suffix_changes_vector(model: &Model2VecEmbeddingModel, purpose: EmbeddingPurpose) -> bool {
    let base = model
        .embed(
            &[vec![Content::Text("hello ".repeat(512))]],
            EmbeddingOptions {
                purpose,
                ..Default::default()
            },
        )
        .await
        .expect("prefix vector");
    let extended = model
        .embed(
            &[vec![Content::Text(
                "hello ".repeat(512) + &"world ".repeat(512),
            )]],
            EmbeddingOptions {
                purpose,
                ..Default::default()
            },
        )
        .await
        .expect("suffix vector");
    base.vectors[0]
        .iter()
        .zip(&extended.vectors[0])
        .any(|(a, b)| (a - b).abs() > 1e-5)
}
