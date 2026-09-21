use serde_json::{Value, json};

use crate::{
    domain::model::Metric,
    models::catalog::{
        ArtifactDownloadConfig, EmbeddingCatalogEntry, LlamaCppConfig, Model2VecConfig, QwenConfig,
        TransformersConfig, list_embedding_models,
    },
};

// Last commit that changed src/engine/models/catalog.ts when this fixture was refreshed.
const MAIN_CATALOG_SOURCE_REVISION: &str = "8d0d5e277f37d897e83749cdaa0e9c07f5e92efe";

#[test]
fn catalog_matches_main_typescript_field_for_field() {
    let expected: Value = serde_json::from_str(include_str!("fixtures/catalog-main-oracle.json"))
        .unwrap_or_else(|error| {
            panic!(
                "TypeScript catalog oracle from {MAIN_CATALOG_SOURCE_REVISION} must be valid JSON: {error}"
            )
        });
    let actual = actual_catalog();
    assert_eq!(actual, expected);
}

fn actual_catalog() -> Value {
    Value::Array(
        list_embedding_models()
            .into_iter()
            .map(entry_value)
            .collect(),
    )
}

#[test]
#[ignore = "manual fixture refresh helper"]
fn print_current_catalog_fixture() {
    println!(
        "{}",
        serde_json::to_string_pretty(&actual_catalog()).expect("serialize catalog")
    );
}

#[allow(clippy::too_many_lines)]
fn entry_value(entry: EmbeddingCatalogEntry) -> Value {
    match entry {
        EmbeddingCatalogEntry::LlamaCpp(LlamaCppConfig {
            reference,
            provider,
            model,
            uri,
            cache_file,
            download,
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
            "cacheFile": cache_file,
            "sources": source_value(download),
            "artifacts": artifact_value(download),
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
        EmbeddingCatalogEntry::Transformers(TransformersConfig {
            reference,
            provider,
            model,
            repo,
            revision,
            download,
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
                "sources": source_value(download),
                "artifacts": artifact_value(download),
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
            download,
            model_file,
            embedding_tensor,
            tokenizer_file,
            dimension,
            metric,
            normalize,
            max_input_tokens,
            max_batch_size,
            default_concurrency,
            query_prefix,
            document_prefix,
        }) => {
            let mut value = json!({
                "backend": "model2vec",
                "reference": reference,
                "provider": provider,
                "model": model,
                "repo": repo,
                "revision": revision,
                "sources": source_value(download),
                "artifacts": artifact_value(download),
                "modelFile": model_file,
                "embeddingTensor": embedding_tensor,
                "tokenizerFile": tokenizer_file,
                "dimension": dimension,
                "metric": metric_name(metric),
                "normalize": normalize,
                "maxInputTokens": max_input_tokens,
                "maxBatchSize": max_batch_size,
                "defaultConcurrency": default_concurrency,
            });
            let object = value.as_object_mut().expect("catalog object");
            if let Some(prefix) = query_prefix {
                object.insert("queryPrefix".to_owned(), json!(prefix));
            }
            if let Some(prefix) = document_prefix {
                object.insert("documentPrefix".to_owned(), json!(prefix));
            }
            value
        }
    }
}

fn source_value(download: &ArtifactDownloadConfig) -> Value {
    json!({
        "huggingFace": {
            "repo": download.hugging_face.repo,
            "revision": download.hugging_face.revision,
        },
        "modelScope": {
            "repo": download.model_scope.repo,
            "revision": download.model_scope.revision,
        },
    })
}

fn artifact_value(download: &ArtifactDownloadConfig) -> Value {
    Value::Array(
        download
            .artifacts
            .iter()
            .map(|artifact| {
                json!({
                    "path": artifact.path,
                    "size": artifact.size,
                    "sha256": artifact.sha256,
                })
            })
            .collect(),
    )
}

const fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::Cosine => "cosine",
        Metric::DotProduct => "dot",
        Metric::Euclidean => "euclidean",
    }
}
