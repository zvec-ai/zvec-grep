use super::super::{DefaultModel2VecDependencies, LoadedModel};
use super::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

async fn tokenizer_fixture(root: &Path, direction: Option<&str>) {
    let bytes = serde_json::to_vec(&json!({
        "version": "1.0",
        "truncation": direction.map(|direction| json!({
            "direction": direction, "max_length": 512,
            "strategy": "LongestFirst", "stride": 0
        })),
        "padding": null,
        "added_tokens": [], "normalizer": null,
        "pre_tokenizer": {"type": "WhitespaceSplit"},
        "post_processor": null, "decoder": null,
        "model": {"type": "WordLevel", "unk_token": "[UNK]",
            "vocab": {"hello": 0, "tail": 1, "[UNK]": 2, "invalid": 3}}
    }))
    .expect("serialize tokenizer");
    tokio::fs::write(root.join("tokenizer.json"), bytes)
        .await
        .expect("write tokenizer");
}

async fn loaded_fixture(root: &Path) -> Model2VecEmbeddingModel {
    let mut entry = fixture_entry();
    entry.max_input_tokens = 1024;
    entry.query_prefix = None;
    entry.document_prefix = None;
    entry.normalize = false;
    let model = Model2VecEmbeddingModel::new(
        entry,
        ModelConfig::default(),
        crate::models::runtime::ModelComputeRuntime::shared(),
    );
    model.state.lock().await.loaded = Some(Arc::new(LoadedModel {
        tokenizer: DefaultModel2VecDependencies::new()
            .load_tokenizer(root)
            .await
            .expect("load tokenizer"),
        table: StaticEmbeddingTable {
            values: vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            dimension: 3,
            rows: 3,
        },
    }));
    model
}

async fn embed(
    model: &Model2VecEmbeddingModel,
    text: String,
    purpose: EmbeddingPurpose,
) -> crate::domain::model::EmbeddingResult {
    model
        .embed(
            &[vec![Content::Text(text)]],
            EmbeddingOptions {
                purpose,
                ..Default::default()
            },
        )
        .await
        .expect("embed fixture")
}

#[tokio::test]
async fn catalog_limit_overrides_serialized_truncation_for_both_purposes() {
    for direction in [None, Some("Right"), Some("Left")] {
        let root = TempDir::new().expect("temporary tokenizer");
        tokenizer_fixture(root.path(), direction).await;
        let original = tokio::fs::read(root.path().join("tokenizer.json"))
            .await
            .expect("original artifact");
        let model = loaded_fixture(root.path()).await;
        assert_eq!(model.info().max_input_tokens(), Some(1024));
        for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
            let mut inputs = Vec::new();
            for length in [1_usize, 511, 512, 513, 1023, 1024, 1025] {
                // The suffix must affect pooling from token 513 through 1024.
                let text =
                    "hello ".repeat(length.min(512)) + &"tail ".repeat(length.saturating_sub(512));
                inputs.push(vec![Content::Code(text.clone())]);
                let result = embed(&model, text, purpose).await;
                assert_eq!(
                    result.truncated,
                    if length > 1024 { vec![0] } else { vec![] }
                );
                let retained = u32::try_from(length.min(1024)).expect("small token count");
                let hello = f64::from(retained.min(512)) / f64::from(retained);
                assert!((f64::from(result.vectors[0][0]) - hello).abs() < 1e-7);
                assert!((f64::from(result.vectors[0][1]) - (1.0 - hello)).abs() < 1e-7);
                assert!(result.vectors[0][2].abs() < f32::EPSILON);
            }
            let batch = model
                .embed(
                    &inputs,
                    EmbeddingOptions {
                        purpose,
                        ..Default::default()
                    },
                )
                .await
                .expect("batch boundaries");
            assert_eq!(batch.truncated, [6]);
            // An extra token must be removed on the right, regardless of the artifact direction.
            let exact = "hello ".repeat(512) + &"tail ".repeat(512);
            assert_eq!(
                embed(&model, exact.clone(), purpose).await.vectors,
                embed(&model, exact + " hello", purpose).await.vectors
            );
        }
        assert_eq!(
            tokio::fs::read(root.path().join("tokenizer.json"))
                .await
                .expect("unchanged artifact"),
            original
        );
    }
}

#[tokio::test]
async fn unknowns_and_prefixes_respect_the_token_budget() {
    let root = TempDir::new().expect("temporary tokenizer");
    tokenizer_fixture(root.path(), Some("Right")).await;
    let mut model = loaded_fixture(root.path()).await;
    for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
        let short = embed(&model, "hello tail missing".to_owned(), purpose).await;
        assert_eq!(short.vectors, [vec![0.5, 0.5, 0.0]]);
        assert!(short.truncated.is_empty());
        let unknown_at_limit = embed(&model, "hello ".repeat(1023) + "missing tail", purpose).await;
        assert_eq!(unknown_at_limit.vectors, [vec![1.0, 0.0, 0.0]]);
        assert_eq!(unknown_at_limit.truncated, [0]);
    }
    model.entry.query_prefix = Some("hello ");
    model.entry.document_prefix = Some("tail ");
    for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
        assert!(
            embed(&model, "hello ".repeat(1023), purpose)
                .await
                .truncated
                .is_empty()
        );
        assert_eq!(
            embed(&model, "hello ".repeat(1024), purpose)
                .await
                .truncated,
            [0]
        );
    }
}

#[tokio::test]
async fn full_tokenization_preserves_errors_cancellation_and_reuse() {
    let root = TempDir::new().expect("temporary tokenizer");
    tokenizer_fixture(root.path(), Some("Right")).await;
    let model = loaded_fixture(root.path()).await;
    let inputs = [vec![Content::Text("hello ".repeat(512) + "invalid")]];
    let error = model
        .embed(&inputs, EmbeddingOptions::default())
        .await
        .expect_err("invalid token 513 must not be hidden");
    assert_eq!(error.code(), crate::EngineError::INTERNAL);
    let signal = CancellationToken::new();
    signal.cancel();
    let error = model
        .embed(
            &inputs,
            EmbeddingOptions {
                signal: Some(signal),
                ..Default::default()
            },
        )
        .await
        .expect_err("cancel before encoding");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    let recovered = embed(
        &model,
        "hello ".repeat(512) + "tail",
        EmbeddingPurpose::Query,
    )
    .await;
    assert!(recovered.vectors[0][1] > 0.0);
    assert!(recovered.truncated.is_empty());
}

#[tokio::test]
async fn tokenizer_load_failure_can_be_retried_after_repair() {
    let root = TempDir::new().expect("temporary tokenizer");
    let dependencies = DefaultModel2VecDependencies::new();
    for bytes in [b"{invalid".as_slice(), b"{}".as_slice()] {
        tokio::fs::write(root.path().join("tokenizer.json"), bytes)
            .await
            .expect("invalid artifact");
        let error = dependencies
            .load_tokenizer(root.path())
            .await
            .err()
            .expect("reject malformed tokenizer");
        assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
    }
    tokenizer_fixture(root.path(), Some("Right")).await;
    let tokenizer = dependencies
        .load_tokenizer(root.path())
        .await
        .expect("recover tokenizer");
    assert_eq!(
        tokenizer
            .encode(&"hello ".repeat(1025))
            .expect("full tokens")
            .len(),
        1025
    );
}

#[test]
fn cancellation_during_tokenization_discards_the_result() {
    struct CancellingTokenizer(CancellationToken);
    impl TokenizerRuntime for CancellingTokenizer {
        fn encode(&self, _: &str) -> Result<Vec<u32>, crate::models::spi::ModelError> {
            self.0.cancel();
            Ok(vec![0; 1025])
        }
        fn unknown_token_id(&self) -> Option<u32> {
            None
        }
    }
    let signal = CancellationToken::new();
    let error = super::super::embed_model2vec_texts(
        &["hello".to_owned()],
        &CancellingTokenizer(signal.clone()),
        &StaticEmbeddingTable {
            values: vec![1.0],
            dimension: 1,
            rows: 1,
        },
        1024,
        true,
        Some(&signal),
    )
    .expect_err("cancelled computation must not return partial vectors");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
}
