use serde_json::{Value, json};

use crate::{
    domain::model::Metric,
    models::catalog::{
        EmbeddingCatalogEntry, LlamaCppConfig, Model2VecConfig, QwenConfig, TransformersConfig,
        list_embedding_models,
    },
};

#[test]
fn catalog_matches_main_typescript_field_for_field() {
    let expected: Value = serde_json::from_str(include_str!("fixtures/catalog-main-oracle.json"))
        .expect("checked-in TypeScript catalog oracle must be valid JSON");
    let actual = Value::Array(
        list_embedding_models()
            .into_iter()
            .map(entry_value)
            .collect(),
    );
    assert_eq!(actual, expected);
}

#[allow(clippy::too_many_lines)]
fn entry_value(entry: EmbeddingCatalogEntry) -> Value {
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(LlamaCppConfig {
            reference,
            provider,
            model,
            uri,
            dimension,
            metric,
            format,
            context_size,
            max_batch_size,
        }) => json!({
            "backend": "llama-cpp",
            "reference": reference,
            "provider": provider,
            "model": model,
            "uri": uri,
            "dimension": dimension,
            "metric": metric_name(metric),
            "format": format,
            "contextSize": context_size,
            "maxBatchSize": max_batch_size,
        }),
        EmbeddingCatalogEntry::Qwen(QwenConfig {
            kind,
            reference,
            provider,
            model,
            dimension,
            metric,
            default_endpoint,
            max_batch_size,
            max_input_tokens,
            max_image_bytes,
        }) => {
            let mut value = json!({
                "backend": "qwen",
                "kind": kind,
                "reference": reference,
                "provider": provider,
                "model": model,
                "dimension": dimension,
                "metric": metric_name(metric),
                "defaultEndpoint": default_endpoint,
                "maxBatchSize": max_batch_size,
                "maxInputTokens": max_input_tokens,
            });
            if let Some(maximum) = max_image_bytes {
                value
                    .as_object_mut()
                    .expect("catalog JSON must be an object")
                    .insert("maxImageBytes".to_owned(), json!(maximum));
            }
            value
        }
        EmbeddingCatalogEntry::TransformersJs(TransformersConfig {
            reference,
            provider,
            model,
            repo,
            revision,
            dtype,
            dimension,
            metric,
            pooling,
            normalize,
            query_prefix,
            document_prefix,
            max_input_tokens,
            max_batch_size,
        }) => {
            let mut value = json!({
                "backend": "transformers-js",
                "reference": reference,
                "provider": provider,
                "model": model,
                "repo": repo,
                "revision": revision,
                "dtype": dtype,
                "dimension": dimension,
                "metric": metric_name(metric),
                "pooling": pooling,
                "normalize": normalize,
                "maxInputTokens": max_input_tokens,
                "maxBatchSize": max_batch_size,
            });
            let object = value
                .as_object_mut()
                .expect("catalog JSON must be an object");
            if let Some(prefix) = query_prefix {
                object.insert("queryPrefix".to_owned(), json!(prefix));
            }
            if let Some(prefix) = document_prefix {
                object.insert("documentPrefix".to_owned(), json!(prefix));
            }
            value
        }
        EmbeddingCatalogEntry::Model2Vec(Model2VecConfig {
            reference,
            provider,
            model,
            repo,
            revision,
            model_file,
            embedding_tensor,
            tokenizer_file,
            dimension,
            metric,
            normalize,
            max_input_tokens,
            max_batch_size,
            default_concurrency,
            ..
        }) => json!({
            "backend": "model2vec",
            "reference": reference,
            "provider": provider,
            "model": model,
            "repo": repo,
            "revision": revision,
            "modelFile": model_file,
            "embeddingTensor": embedding_tensor,
            "tokenizerFile": tokenizer_file,
            "dimension": dimension,
            "metric": metric_name(metric),
            "normalize": normalize,
            "maxInputTokens": max_input_tokens,
            "maxBatchSize": max_batch_size,
            "defaultConcurrency": default_concurrency,
        }),
    }
}

const fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::Cosine => "cosine",
        Metric::DotProduct => "dot",
        Metric::Euclidean => "euclidean",
    }
}
