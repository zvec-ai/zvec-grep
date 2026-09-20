use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ModelInfo {
    pub provider: String,
    pub name: String,
    pub endpoint: Option<String>,
}

impl ModelInfo {
    pub(crate) fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.name)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

/// Execution device requested for a local model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Auto,
    Cpu,
    Cuda,
    Metal,
    Vulkan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelProgress {
    Preparing {
        model: String,
    },
    Downloading {
        model: String,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
    },
    Warning {
        model: String,
        message: String,
    },
    Ready {
        model: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Metric {
    Cosine,
    DotProduct,
    Euclidean,
}

mod embedding;
mod reranking;

pub(crate) use embedding::{EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult};
#[allow(unused_imports)] // Reserved for the first reranking backend.
pub(crate) use reranking::RerankScore;
