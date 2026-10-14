use super::EmbeddingCatalogEntry;
use crate::{
    EngineResult,
    domain::model::{EmbeddingMetric, EmbeddingModelInfo},
};

impl EmbeddingCatalogEntry {
    pub(crate) fn embedding_model_info(self) -> EngineResult<EmbeddingModelInfo> {
        let (dimension, metric, max_input_tokens, max_image_bytes) = match self {
            Self::EmbeddingGemma2(entry) => (
                entry.dimension,
                EmbeddingMetric::Cosine,
                entry.max_input_tokens,
                Some(entry.max_image_bytes),
            ),
            Self::LlamaCpp(entry) => (entry.dimension, entry.metric, entry.context_size, None),
            Self::Qwen(entry) => (
                entry.dimension,
                entry.metric,
                entry.max_input_tokens,
                entry.max_image_bytes,
            ),
            Self::Transformers(entry) => {
                (entry.dimension, entry.metric, entry.max_input_tokens, None)
            }
            Self::Model2Vec(entry) => (entry.dimension, entry.metric, entry.max_input_tokens, None),
        };
        EmbeddingModelInfo::new(
            self.model_info()?,
            dimension,
            metric,
            Some(max_input_tokens),
            max_image_bytes,
        )
    }
}
