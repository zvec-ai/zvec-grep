use std::sync::Arc;

use super::{
    catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    compute::ModelComputeRuntime,
    llama_cpp::LlamaCppEmbeddingModel,
    model2vec::Model2VecEmbeddingModel,
    qwen::QwenEmbeddingModel,
    spi::{EmbeddingModel, ModelError},
    transformers::TransformersEmbeddingModel,
};
use crate::domain::model::ModelConfig;

/// Creates a catalog-backed embedding model.
///
/// `None` for `options` is the Rust equivalent of omitting the optional
/// TypeScript options object.
///
/// # Errors
///
/// Returns an error for unknown references or invalid backend options.
pub fn create_embedding_model(
    reference: &str,
    options: Option<ModelConfig>,
    compute_runtime: ModelComputeRuntime,
) -> Result<Arc<dyn EmbeddingModel>, ModelError> {
    let entry = get_embedding_model_catalog_entry(reference).ok_or_else(|| {
        ModelError::new(
            crate::EngineError::NOT_FOUND,
            "Embedding model is not in the zvec-grep catalog",
            Some(format!("embedding={reference}")),
        )
    })?;
    let options = options.unwrap_or_default();
    match entry {
        EmbeddingCatalogEntry::Model2Vec(config) => Ok(Arc::new(Model2VecEmbeddingModel::new(
            config,
            options,
            compute_runtime,
        ))),
        EmbeddingCatalogEntry::Qwen(config) => {
            Ok(Arc::new(QwenEmbeddingModel::new(config, options)?))
        }
        EmbeddingCatalogEntry::TransformersJs(config) => Ok(Arc::new(
            TransformersEmbeddingModel::new(config, options, compute_runtime),
        )),
        EmbeddingCatalogEntry::LlamaCpp(config) => Ok(Arc::new(LlamaCppEmbeddingModel::new(
            config,
            options,
            compute_runtime,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::create_embedding_model;

    #[test]
    fn all_catalog_backends_produce_valid_model_info() {
        for entry in crate::models::catalog::list_embedding_models() {
            let options = matches!(entry, super::EmbeddingCatalogEntry::Qwen(_)).then(|| {
                super::ModelConfig {
                    api_key: Some("test".to_owned()),
                    endpoint: Some("https://models.example.test/embeddings".to_owned()),
                    ..super::ModelConfig::default()
                }
            });
            let expected_endpoint = options
                .as_ref()
                .and_then(|options| options.endpoint.clone());
            let model = create_embedding_model(
                entry.reference(),
                options,
                crate::models::compute::ModelComputeRuntime::shared(),
            )
            .expect("catalog backend should construct without loading model assets");
            model.info().validate().expect("valid catalog model info");
            assert_eq!(model.info().model.reference(), entry.reference());
            assert_eq!(model.info().dimension, entry.dimension());
            assert_eq!(model.info().model.endpoint, expected_endpoint);
        }

        let unknown = create_embedding_model(
            "missing",
            None,
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .err()
        .expect("unknown model should fail catalog lookup");
        assert_eq!(unknown.code(), crate::EngineError::NOT_FOUND);
    }
}
