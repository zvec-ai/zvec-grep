use crate::EngineResult;
use crate::domain::ContentKind;
use crate::domain::model::{EmbeddingMetric, ModelInfo};
use crate::models::artifacts::{ArtifactConfig, ArtifactDownloadConfig, ArtifactSourceConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmbeddingCatalogEntry {
    EmbeddingGemma2(EmbeddingGemma2Config),
    LlamaCpp(LlamaCppConfig),
    Qwen(QwenConfig),
    Transformers(TransformersConfig),
    Model2Vec(Model2VecConfig),
}

impl EmbeddingCatalogEntry {
    /// Describes the input contract implemented by the catalog's model adapter.
    pub(crate) fn model_info(self) -> EngineResult<ModelInfo> {
        let (provider, name) = match self {
            Self::EmbeddingGemma2(entry) => (entry.provider, entry.model),
            Self::LlamaCpp(entry) => (entry.provider, entry.model),
            Self::Qwen(entry) => (entry.provider, entry.model),
            Self::Transformers(entry) => (entry.provider, entry.model),
            Self::Model2Vec(entry) => (entry.provider, entry.model),
        };
        if matches!(self, Self::EmbeddingGemma2(_))
            || matches!(self, Self::Qwen(entry) if entry.kind == "multimodal")
        {
            ModelInfo::new(
                provider,
                name,
                [ContentKind::Text, ContentKind::Code, ContentKind::Image],
            )
        } else {
            ModelInfo::new(provider, name, [ContentKind::Text, ContentKind::Code])
        }
    }

    #[cfg(test)]
    pub(crate) const fn backend(self) -> &'static str {
        match self {
            Self::EmbeddingGemma2(_) => "embeddinggemma2-onnx",
            Self::LlamaCpp(_) => "llama-cpp",
            Self::Qwen(_) => "qwen",
            Self::Transformers(_) => "transformers",
            Self::Model2Vec(_) => "model2vec",
        }
    }

    pub(crate) const fn reference(self) -> &'static str {
        match self {
            Self::EmbeddingGemma2(entry) => entry.reference,
            Self::LlamaCpp(entry) => entry.reference,
            Self::Qwen(entry) => entry.reference,
            Self::Transformers(entry) => entry.reference,
            Self::Model2Vec(entry) => entry.reference,
        }
    }

    #[cfg(test)]
    pub(crate) const fn dimension(self) -> usize {
        match self {
            Self::EmbeddingGemma2(entry) => entry.dimension,
            Self::LlamaCpp(entry) => entry.dimension,
            Self::Qwen(entry) => entry.dimension,
            Self::Transformers(entry) => entry.dimension,
            Self::Model2Vec(entry) => entry.dimension,
        }
    }

    #[cfg(test)]
    pub(crate) const fn llama_cpp_config(self) -> Option<LlamaCppConfig> {
        if let Self::LlamaCpp(entry) = self {
            Some(entry)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) const fn transformers_config(self) -> Option<TransformersConfig> {
        if let Self::Transformers(entry) = self {
            Some(entry)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) const fn model2vec_config(self) -> Option<Model2VecConfig> {
        if let Self::Model2Vec(entry) = self {
            Some(entry)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LlamaCppConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) cache_file: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) format: &'static str,
    pub(crate) context_size: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QwenConfig {
    pub(crate) kind: &'static str,
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) default_endpoint: &'static str,
    pub(crate) max_batch_size: usize,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_image_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransformersConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) dtype: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) pooling: &'static str,
    pub(crate) normalize: bool,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Model2VecConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) model_file: &'static str,
    pub(crate) embedding_tensor: &'static str,
    pub(crate) tokenizer_file: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) normalize: bool,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
    pub(crate) default_concurrency: usize,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EmbeddingGemma2Config {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) source: ArtifactSourceConfig,
    pub(crate) artifacts: &'static [ArtifactConfig],
    pub(crate) dimension: usize,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_image_bytes: usize,
}
