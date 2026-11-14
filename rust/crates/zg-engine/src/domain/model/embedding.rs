use super::ModelInfo;
use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingModelInfo {
    model: ModelInfo,
    dimension: usize,
    metric: EmbeddingMetric,
    #[serde(default, skip_serializing_if = "is_default_vector_index_quantization")]
    vector_index_quantization: VectorIndexQuantization,
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
            vector_index_quantization: VectorIndexQuantization::default(),
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

    pub(crate) fn vector_index_quantization(&self) -> VectorIndexQuantization {
        self.vector_index_quantization
    }

    pub(crate) fn with_vector_index_quantization(
        mut self,
        quantization: VectorIndexQuantization,
    ) -> Self {
        self.vector_index_quantization = quantization;
        self
    }

    pub(crate) fn max_input_tokens(&self) -> Option<usize> {
        self.max_input_tokens
    }

    pub(crate) fn max_image_bytes(&self) -> Option<usize> {
        self.max_image_bytes
    }

    /// Check whether two embedding models use compatible embedding spaces.
    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        let reason = if self.model.provider() != other.model.provider()
            || self.model.name() != other.model.name()
        {
            "model references differ".to_owned()
        } else if self.dimension != other.dimension {
            format!(
                "dimensions differ ({} vs {})",
                self.dimension, other.dimension
            )
        } else if self.metric != other.metric {
            format!("metrics differ ({:?} vs {:?})", self.metric, other.metric)
        } else {
            return Ok(());
        };
        Err(EngineError::invalid_argument(format!(
            "embedding models {} and {} are incompatible: {reason}",
            self.model.reference(),
            other.model.reference(),
        )))
    }

    /// Check whether two model descriptors can use the same physical vector index.
    pub(crate) fn ensure_storage_compatible(&self, other: &Self) -> EngineResult<()> {
        self.ensure_index_compatible(other)?;
        if self.vector_index_quantization != other.vector_index_quantization {
            return Err(EngineError::invalid_argument(format!(
                "embedding models {} and {} are incompatible: vector index quantization differs ({:?} vs {:?})",
                self.model.reference(),
                other.model.reference(),
                self.vector_index_quantization,
                other.vector_index_quantization,
            )));
        }
        Ok(())
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
            #[serde(default)]
            vector_index_quantization: VectorIndexQuantization,
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
        .map(|info| info.with_vector_index_quantization(fields.vector_index_quantization))
        .map_err(serde::de::Error::custom)
    }
}

/// Compression used by the per-model vector ANN index.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexQuantization {
    /// Store the vector field as FP16 and use an ordinary HNSW index.
    #[default]
    Fp16,
    /// Store one sign bit per document-vector component and score with float queries.
    BinarySign,
}

fn is_default_vector_index_quantization(quantization: &VectorIndexQuantization) -> bool {
    *quantization == VectorIndexQuantization::Fp16
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

#[cfg(test)]
mod tests {
    use super::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo, VectorIndexQuantization};
    use crate::domain::ContentKind;

    fn embedding(quantization: VectorIndexQuantization) -> EmbeddingModelInfo {
        EmbeddingModelInfo::new(
            ModelInfo::new("fixture", "test", [ContentKind::Text], 4).expect("model"),
            1024,
            EmbeddingMetric::Cosine,
            None,
            None,
        )
        .expect("embedding")
        .with_vector_index_quantization(quantization)
    }

    #[test]
    fn quantization_defaults_to_fp16_for_legacy_metadata_and_round_trips() {
        let fp16 = embedding(VectorIndexQuantization::Fp16);
        let mut legacy = serde_json::to_value(&fp16).expect("serialize FP16 metadata");
        legacy
            .as_object_mut()
            .expect("metadata object")
            .remove("vectorIndexQuantization");
        let decoded: EmbeddingModelInfo =
            serde_json::from_value(legacy).expect("legacy metadata defaults to FP16");
        assert_eq!(
            decoded.vector_index_quantization(),
            VectorIndexQuantization::Fp16
        );

        let binary = embedding(VectorIndexQuantization::BinarySign);
        let encoded = serde_json::to_value(&binary).expect("serialize binary metadata");
        assert_eq!(encoded["vectorIndexQuantization"], "binary_sign");
        let decoded: EmbeddingModelInfo =
            serde_json::from_value(encoded).expect("deserialize binary metadata");
        assert_eq!(decoded, binary);
        assert!(
            fp16.ensure_storage_compatible(&binary)
                .expect_err("quantizer changes the index schema")
                .to_string()
                .contains("vector index quantization differs")
        );
    }
}
