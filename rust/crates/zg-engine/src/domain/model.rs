//! Model info exposes limits that pipelines can use to adjust their work,
//! such as batch size.
//! Limits that only determine whether an input is accepted are checked
//! by the backend and are not exposed in model info.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod info;
pub(crate) use info::ModelInfo;

/// Connection and execution settings for a model runtime.
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
        model_ref: String,
    },
    Downloading {
        model_ref: String,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
    },
    Warning {
        model_ref: String,
        message: String,
    },
    Ready {
        model_ref: String,
    },
}

mod embedding;
pub(crate) use embedding::EmbeddingMetric;
pub(crate) use embedding::EmbeddingModelInfo;
pub(crate) use embedding::EmbeddingPurpose;
pub(crate) use embedding::EmbeddingResult;
pub(crate) use embedding::{EmbeddingRetrieval, EmbeddingSpace};

mod reranking;
#[allow(unused_imports)] // Reserved for the first reranking backend.
pub(crate) use reranking::RerankScore;
