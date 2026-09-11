use std::sync::Arc;

use super::{
    catalog::get_embedding_model_catalog_entry,
    llama_cpp::LlamaCppEmbeddingModel,
    model2vec::Model2VecEmbeddingModel,
    qwen::QwenEmbeddingModel,
    spi::{CreateEmbeddingModelOptions, EmbeddingModel, ModelError},
    transformers::TransformersEmbeddingModel,
};

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
    options: Option<CreateEmbeddingModelOptions>,
) -> Result<Arc<dyn EmbeddingModel>, ModelError> {
    let entry = get_embedding_model_catalog_entry(reference).ok_or_else(|| {
        ModelError::new(
            crate::EngineError::NOT_FOUND,
            "Embedding model is not in the zvec-grep catalog",
            Some(format!("embedding={reference}")),
        )
    })?;
    let options = options.unwrap_or_default();
    if let Some(config) = entry.model2vec_config() {
        return Ok(Arc::new(Model2VecEmbeddingModel::new(config, options)));
    }
    if let Some(config) = entry.qwen_config() {
        return Ok(Arc::new(QwenEmbeddingModel::new(config, options)?));
    }
    if let Some(config) = entry.transformers_config() {
        return Ok(Arc::new(TransformersEmbeddingModel::new(config, options)));
    }
    if let Some(config) = entry.llama_cpp_config() {
        return Ok(Arc::new(LlamaCppEmbeddingModel::new(config, options)));
    }
    Err(ModelError::new(
        crate::EngineError::UNSUPPORTED,
        "Embedding catalog entry is not implemented",
        Some(format!("backend={} reference={reference}", entry.backend())),
    ))
}

#[cfg(test)]
mod tests {
    use super::create_embedding_model;

    #[test]
    fn factory_exposes_implemented_backends() {
        let model = create_embedding_model("local/potion-code-16m-v2", None)
            .expect("Model2Vec backend should be implemented");
        assert_eq!(model.info().reference, "local/potion-code-16m-v2");

        let qwen = create_embedding_model(
            "qwen/text-embedding-v4",
            Some(super::CreateEmbeddingModelOptions {
                api_key: Some("test".to_owned()),
                ..super::CreateEmbeddingModelOptions::default()
            }),
        )
        .expect("Qwen backend should be implemented");
        assert_eq!(qwen.info().reference, "qwen/text-embedding-v4");

        let llama = create_embedding_model("local/embeddinggemma-300m", None)
            .expect("llama.cpp backend should be implemented");
        assert_eq!(llama.info().reference, "local/embeddinggemma-300m");

        let transformers = create_embedding_model("local/all-minilm-l6-v2", None)
            .expect("Transformers backend should be implemented");
        assert_eq!(transformers.info().reference, "local/all-minilm-l6-v2");

        let unknown = create_embedding_model("missing", None)
            .err()
            .expect("unknown model should fail catalog lookup");
        assert_eq!(unknown.code(), crate::EngineError::NOT_FOUND);
    }
}
