use super::ModelInfo;
use crate::domain::ContentKind;
use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingModelInfo {
    pub model: ModelInfo,
    pub space: EmbeddingSpace,
    pub retrieval: EmbeddingRetrieval,
    pub dimension: usize,
    pub metric: EmbeddingMetric,
    /// Maximum number of inputs accepted by one embedding request.
    pub max_batch_size: usize,
    /// Positive token limit when applicable and known.
    pub max_input_tokens: Option<usize>,
    /// Positive image size limit in bytes when applicable and known.
    pub max_image_bytes: Option<usize>,
}

impl EmbeddingModelInfo {
    pub(crate) fn query_encoding(&self, input: ContentKind, target: ContentKind) -> String {
        let instruction = if input != ContentKind::Image && target == ContentKind::Code {
            self.space.code_query_instruction.as_deref().unwrap_or("")
        } else {
            ""
        };
        crate::utils::sha256_hex_parts([
            self.space.encoding_fingerprint.as_bytes(),
            instruction.as_bytes(),
        ])
    }

    pub(crate) fn supports_retrieval(&self, input: ContentKind, target: ContentKind) -> bool {
        self.model.supports_content(input)
            && self.model.supports_content(target)
            && match self.retrieval {
                EmbeddingRetrieval::Text => {
                    input != ContentKind::Image && target != ContentKind::Image
                }
                EmbeddingRetrieval::TextImage => true,
            }
    }

    /// Vector scores are comparable only for the same query and a pinned space.
    pub(crate) fn can_compare_vectors(&self, other: &Self) -> bool {
        self.space.revision.is_some() && self.ensure_index_compatible(other).is_ok()
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        for (field, value) in [
            ("dimension", Some(self.dimension)),
            ("max_batch_size", Some(self.max_batch_size)),
            ("max_input_tokens", self.max_input_tokens),
            ("max_image_bytes", self.max_image_bytes),
        ] {
            if value == Some(0) {
                return Err(EngineError::invalid_argument(format!(
                    "embedding model {field} must be greater than zero",
                )));
            }
        }
        Ok(())
    }

    /// Check the fields that determine whether an existing index can be reused.
    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        if self.model != other.model
            || self.space != other.space
            || self.retrieval != other.retrieval
            || self.dimension != other.dimension
            || self.metric != other.metric
            || self.max_input_tokens != other.max_input_tokens
            || self.max_image_bytes != other.max_image_bytes
        {
            return Err(EngineError::invalid_argument(
                "existing index uses a different embedding model; rebuild the index",
            ));
        }
        Ok(())
    }
}

/// Immutable weights and the adapter's complete encoding recipe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingSpace {
    /// None means the service exposes only a mutable model alias.
    pub revision: Option<String>,
    pub encoding_fingerprint: String,
    /// A distinct query task for code search, when required by the adapter.
    pub code_query_instruction: Option<String>,
}

impl EmbeddingSpace {
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            revision: Some("fixture-v1".to_owned()),
            encoding_fingerprint: "fixture-v1".to_owned(),
            code_query_instruction: None,
        }
    }
}

/// Retrieval tasks verified for the catalog model and its implemented adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmbeddingRetrieval {
    Text,
    TextImage,
}

/// Distance or similarity measure for embedding vectors.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmbeddingMetric {
    Cosine,
    DotProduct,
    Euclidean,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmbeddingPurpose {
    #[default]
    Document,
    Query,
}

/// Result of a batch embedding request.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingResult {
    /// One vector per input, in input order, each with the model's declared dimension.
    pub vectors: Vec<Vec<f32>>,
    /// Zero-based indices of inputs that were truncated before producing their vectors.
    pub truncated: Vec<usize>,
}
