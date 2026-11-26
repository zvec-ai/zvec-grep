use super::ModelInfo;
use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingModelInfo {
    model: ModelInfo,
    dimension: usize,
    metric: EmbeddingMetric,
    max_input_tokens: Option<usize>,
    max_image_bytes: Option<usize>,
}

impl EmbeddingModelInfo {
    pub(crate) fn new(
        model: ModelInfo,
        dimension: usize,
        metric: EmbeddingMetric,
        max_input_tokens: Option<usize>,
        max_image_bytes: Option<usize>,
    ) -> EngineResult<Self> {
        for (field, value) in [
            ("dimension", Some(dimension)),
            ("max_input_tokens", max_input_tokens),
            ("max_image_bytes", max_image_bytes),
        ] {
            if value == Some(0) {
                return Err(EngineError::invalid_argument(format!(
                    "embedding model {} {field} must be greater than zero",
                    model.reference(),
                )));
            }
        }
        Ok(Self {
            model,
            dimension,
            metric,
            max_input_tokens,
            max_image_bytes,
        })
    }

    pub(crate) fn model(&self) -> &ModelInfo {
        &self.model
    }

    pub(crate) fn dimension(&self) -> usize {
        self.dimension
    }

    pub(crate) fn metric(&self) -> EmbeddingMetric {
        self.metric
    }

    pub(crate) fn max_input_tokens(&self) -> Option<usize> {
        self.max_input_tokens
    }

    pub(crate) fn max_image_bytes(&self) -> Option<usize> {
        self.max_image_bytes
    }

    /// Check whether two embedding models use compatible embedding spaces.
    pub(crate) fn ensure_index_compatible(&self, requested: &Self) -> EngineResult<()> {
        let message = if self.model.provider() != requested.model.provider()
            || self.model.name() != requested.model.name()
        {
            format!(
                "embedding model would change from {} to {}",
                self.model.reference(),
                requested.model.reference(),
            )
        } else if self.dimension != requested.dimension {
            format!(
                "vector dimension for {} would change from {} to {}",
                self.model.reference(),
                self.dimension,
                requested.dimension,
            )
        } else if self.metric != requested.metric {
            format!(
                "distance metric for {} would change from {:?} to {:?}",
                self.model.reference(),
                self.metric,
                requested.metric,
            )
        } else {
            return Ok(());
        };
        Err(EngineError::invalid_argument(message))
    }
}

impl<'de> Deserialize<'de> for EmbeddingModelInfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fields {
            model: ModelInfo,
            dimension: usize,
            metric: EmbeddingMetric,
            max_input_tokens: Option<usize>,
            max_image_bytes: Option<usize>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(
            fields.model,
            fields.dimension,
            fields.metric,
            fields.max_input_tokens,
            fields.max_image_bytes,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Distance or similarity measure for embedding vectors.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
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
