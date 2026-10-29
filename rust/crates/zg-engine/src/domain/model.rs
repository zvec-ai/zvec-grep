//! Model info exposes limits that pipelines can use to adjust their work,
//! such as batch size.
//! Limits that only determine whether an input is accepted are checked
//! by the backend and are not exposed in model info.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod info;
pub(crate) use info::ModelInfo;

/// Connection and execution settings.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    bound(deserialize = "Key: Deserialize<'de>")
)]
pub(crate) struct ModelConfig<Key = String> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<Key>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

impl<Key> Default for ModelConfig<Key> {
    fn default() -> Self {
        Self {
            api_key: None,
            endpoint: None,
            device: None,
            cache_dir: None,
        }
    }
}

impl<Key> std::fmt::Debug for ModelConfig<Key> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelConfig")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("endpoint", &self.endpoint)
            .field("device", &self.device)
            .field("cache_dir", &self.cache_dir)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EncryptedApiKey {
    #[serde(with = "crate::utils::base64_bytes")]
    pub(crate) nonce: Vec<u8>,
    #[serde(with = "crate::utils::base64_bytes")]
    pub(crate) ciphertext: Vec<u8>,
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

mod reranking;
#[allow(unused_imports)] // Reserved for the first reranking backend.
pub(crate) use reranking::RerankScore;

#[cfg(test)]
mod tests {
    use super::ModelConfig;

    #[test]
    fn debug_redacts_sensitive_fields() {
        let config = ModelConfig {
            api_key: Some("private-token".to_owned()),
            endpoint: Some("https://example.test/embeddings".to_owned()),
            ..ModelConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("private-token"));
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains("https://example.test/embeddings"));
    }
}
