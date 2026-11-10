use sha2::{Digest, Sha256};

use super::*;
use crate::models::{
    artifacts::{ArtifactConfig, ArtifactDownloadConfig, ArtifactSourceConfig, model_is_cached},
    catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
};

struct Fixture {
    root: tempfile::TempDir,
    entry: BekkoConfig,
}

impl Fixture {
    fn new(valid_graph: bool, valid_tokenizer: bool) -> Self {
        let root = tempfile::tempdir().expect("isolated cache");
        let source = ArtifactSourceConfig {
            repo: "fixture/bekko",
            revision: "pinned",
        };
        let snapshot = root.path().join(source.repo).join(source.revision);
        std::fs::create_dir_all(snapshot.join("onnx")).expect("snapshot");
        let graph = if valid_graph {
            super::super::onnx::tests::fixture()
        } else {
            vec![0]
        };
        let tokenizer = if valid_tokenizer {
            serde_json::to_vec(&serde_json::json!({
            "version":"1.0", "truncation":{"direction":"Right","max_length":1,"strategy":"LongestFirst","stride":0},
            "padding":null, "added_tokens":[], "normalizer":{"type":"BertNormalizer","clean_text":true,"handle_chinese_chars":false,"strip_accents":false,"lowercase":false},
            "pre_tokenizer":{"type":"Whitespace"},
            "post_processor":{"type":"BertProcessing","sep":["negative",2],"cls":["negative",2]},
            "decoder":null,
            "model":{"type":"WordPiece","unk_token":"[UNK]","continuing_subword_prefix":"##",
                "max_input_chars_per_word":100,"vocab":{"[UNK]":0,"known":1,"negative":2}}
        })).expect("tokenizer JSON")
        } else {
            b"not JSON".to_vec()
        };
        let artifacts = [("onnx/model.onnx", graph), ("tokenizer.json", tokenizer)]
            .into_iter()
            .map(|(path, bytes)| {
                std::fs::write(snapshot.join(path), &bytes).expect("cached artifact");
                ArtifactConfig {
                    path,
                    size: bytes.len().try_into().expect("size"),
                    sha256: Box::leak(hex::encode(Sha256::digest(bytes)).into_boxed_str()),
                }
            })
            .collect::<Vec<_>>();
        let download = Box::leak(Box::new(ArtifactDownloadConfig {
            hugging_face: source,
            model_scope: None,
            artifacts: Box::leak(artifacts.into_boxed_slice()),
        }));
        Self {
            root,
            entry: BekkoConfig {
                reference: "local/bekko-fixture",
                provider: "local",
                model: "bekko-fixture",
                download,
                model_file: "onnx/model.onnx",
                vocabulary_size: 3,
                dimension: 2,
                metric: crate::domain::model::EmbeddingMetric::Cosine,
                max_input_tokens: 2,
                max_batch_size: 8,
            },
        }
    }

    fn model(&self) -> BekkoEmbeddingModel {
        BekkoEmbeddingModel::new(
            self.entry,
            ModelConfig {
                cache_dir: Some(self.root.path().to_path_buf()),
                ..ModelConfig::default()
            },
            ModelComputeRuntime::shared(),
        )
    }
}

#[test]
fn bekko_catalog_preserves_pinned_assets_and_public_limits() {
    let EmbeddingCatalogEntry::Bekko(entry) =
        get_embedding_model_catalog_entry("local/bekko-256-int8").expect("Bekko")
    else {
        panic!("native Bekko backend");
    };
    assert_eq!(entry.model_file, "onnx/model_int8_dim256.onnx");
    assert_eq!(
        (
            entry.vocabulary_size,
            entry.dimension,
            entry.max_batch_size,
            entry.max_input_tokens
        ),
        (200_000, 256, 32, 32_768)
    );
    assert_eq!(
        entry.download.hugging_face.revision,
        "c18167b8f244c1bc6de21ce9b710bac8a29c4615"
    );
    assert!(entry.download.model_scope.is_none());
    let model =
        BekkoEmbeddingModel::new(entry, ModelConfig::default(), ModelComputeRuntime::shared());
    assert_eq!(model.concurrency_defaults().initial, 1);
}

#[tokio::test]
async fn cached_artifacts_prepare_once_without_ort_and_keep_token_semantics() {
    let fixture = Fixture::new(true, true);
    assert!(
        model_is_cached(
            EmbeddingCatalogEntry::Bekko(fixture.entry),
            Some(fixture.root.path())
        )
        .await
    );
    let model = fixture.model();
    let (one, two) = tokio::join!(
        model.ensure_loaded(None, None),
        model.ensure_loaded(None, None)
    );
    assert!(Arc::ptr_eq(&one.expect("prepared"), &two.expect("reused")));
    std::fs::remove_dir_all(fixture.root.path().join("fixture"))
        .expect("remove source after loading");
    for purpose in [
        crate::domain::model::EmbeddingPurpose::Query,
        crate::domain::model::EmbeddingPurpose::Document,
    ] {
        let result = model
            .embed(
                &[
                    vec![Content::Text("mystery".into())],
                    vec![Content::Text("\0".into())],
                    vec![Content::Code("known known negative".into())],
                    vec![
                        Content::Text("known".into()),
                        Content::Code("negative".into()),
                    ],
                    vec![Content::Text("known mystery".into())],
                ],
                EmbeddingOptions {
                    purpose,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("direct inference");
        assert_eq!(result.vectors[0], [0.6, 0.8]); // UNK is not discarded.
        assert_eq!(result.vectors[1], [0.0, 0.0]);
        assert_eq!(result.vectors[2], [0.0, 1.0]); // Limit overrides serialized limit=1.
        assert_eq!(result.truncated, [2]);
        assert!((result.vectors[3][0] + std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-7);
        assert!(result.vectors[4][0] > 0.5); // Mixed known/UNK, no CLS/SEP/prefix.
    }
    assert!(model.embed(&[], EmbeddingOptions::default()).await.is_err());
    assert!(
        model
            .embed(
                &[vec![Content::Text("known".into())]],
                EmbeddingOptions::default()
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn parallel_batches_match_single_inputs_and_keep_truncation_indices() {
    let fixture = Fixture::new(true, true);
    let model = fixture.model();
    let inputs = [
        "known",
        "known known negative",
        "mystery",
        "negative known mystery",
        "\0",
        "known negative",
        "mystery mystery known",
        "negative",
    ]
    .map(|text| vec![Content::Text(text.into())]);
    let mut expected = Vec::new();
    for input in &inputs {
        expected.push(
            model
                .embed(std::slice::from_ref(input), EmbeddingOptions::default())
                .await
                .expect("single input")
                .vectors
                .remove(0),
        );
    }
    let batches = futures_util::future::join_all(
        (0..4).map(|_| model.embed(&inputs, EmbeddingOptions::default())),
    )
    .await;
    for batch in batches {
        let batch = batch.expect("concurrent batch");
        assert_eq!(batch.truncated, [1, 3, 6]);
        assert_eq!(batch.vectors.len(), expected.len());
        for (actual, expected) in batch.vectors.iter().zip(&expected) {
            assert_eq!(
                actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn cancellation_before_loading_while_waiting_and_after_loading_is_recoverable() {
    let fixture = Fixture::new(true, true);
    let model = fixture.model();
    let signal = CancellationToken::new();
    signal.cancel();
    let error = model
        .prepare(EmbeddingPrepareOptions {
            signal: Some(signal.clone()),
            ..EmbeddingPrepareOptions::default()
        })
        .await
        .expect_err("cancelled");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert!(model.state.lock().await.is_none());
    let held = model.state.lock().await;
    let waiting_signal = CancellationToken::new();
    let future = model.ensure_loaded(None, Some(&waiting_signal));
    tokio::pin!(future);
    assert!(futures_util::poll!(&mut future).is_pending());
    waiting_signal.cancel();
    assert_eq!(
        future.await.err().expect("cancel waiting caller").code(),
        crate::EngineError::CANCELLED
    );
    drop(held);
    let cancel_on_ready = CancellationToken::new();
    let callback_signal = cancel_on_ready.clone();
    let result = model
        .prepare(EmbeddingPrepareOptions {
            signal: Some(cancel_on_ready),
            on_progress: Some(Arc::new(move |p| {
                if matches!(p, ModelProgress::Ready { .. }) {
                    callback_signal.cancel();
                }
            })),
            ..EmbeddingPrepareOptions::default()
        })
        .await;
    assert_eq!(
        result.expect_err("cancelled before publication").code(),
        crate::EngineError::CANCELLED
    );
    assert!(model.state.lock().await.is_none());
    model
        .prepare(EmbeddingPrepareOptions::default())
        .await
        .expect("retry preparation");
    let inputs = [vec![Content::Text("known".into())]];
    assert_eq!(
        model
            .embed(
                &inputs,
                EmbeddingOptions {
                    signal: Some(signal),
                    ..EmbeddingOptions::default()
                }
            )
            .await
            .expect_err("cancel prepared request")
            .code(),
        crate::EngineError::CANCELLED
    );
    assert_eq!(
        model
            .embed(&inputs, EmbeddingOptions::default())
            .await
            .expect("recovered")
            .vectors[0],
        [0.0, 1.0]
    );
}

#[tokio::test]
async fn malformed_verified_artifacts_fail_without_publishing_partial_state() {
    for (graph, tokenizer) in [(false, true), (true, false)] {
        let fixture = Fixture::new(graph, tokenizer);
        let model = fixture.model();
        for _ in 0..2 {
            let error = model
                .prepare(EmbeddingPrepareOptions::default())
                .await
                .expect_err("malformed artifact");
            assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
            assert!(error.should_fail_fast());
            assert!(model.state.lock().await.is_none());
        }
    }
}
