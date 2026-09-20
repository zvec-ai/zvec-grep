use super::{Metric, ModelInfo};
use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingModelInfo {
    pub model: ModelInfo,
    pub dimension: usize,
    pub metric: Metric,
    /// Maximum number of inputs accepted by one embedding request.
    pub max_batch_size: usize,
    /// Positive token limit when applicable and known.
    pub max_input_tokens: Option<usize>,
    /// Positive image size limit in bytes when applicable and known.
    pub max_image_bytes: Option<usize>,
}

impl EmbeddingModelInfo {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        for (field, value) in [
            ("provider", self.model.provider.as_str()),
            ("name", self.model.name.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "embedding model {field} must be non-empty",
                )));
            }
        }
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
        if self.model.provider != other.model.provider
            || self.model.name != other.model.name
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
