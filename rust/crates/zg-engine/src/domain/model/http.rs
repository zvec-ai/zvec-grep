//! Persistable, credential-free description of a runtime HTTP embedding model.

use super::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo};
use crate::{EngineError, EngineResult, domain::ContentKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpAuth {
    None,
    Bearer,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HttpEmbeddingConfig {
    pub model: String,
    pub dimension: usize,
    #[serde(default = "default_metric")]
    pub metric: EmbeddingMetric,
    pub max_input_tokens: usize,
    #[serde(default = "default_batch_size")]
    pub max_batch_size: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    pub auth: HttpAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default)]
    pub send_dimensions: bool,
    #[serde(default)]
    pub send_encoding_format: bool,
    #[serde(default)]
    pub single_input: bool,
}

const fn default_metric() -> EmbeddingMetric {
    EmbeddingMetric::Cosine
}
const fn default_batch_size() -> usize {
    1
}
const fn default_timeout() -> u64 {
    60_000
}

impl HttpEmbeddingConfig {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.model.trim().is_empty() || self.model.chars().any(char::is_control) {
            return Err(EngineError::invalid_argument(
                "HTTP embedding model must be non-empty and contain no control characters",
            ));
        }
        if self.dimension == 0
            || self.max_input_tokens == 0
            || self.max_batch_size == 0
            || self.timeout_ms == 0
        {
            return Err(EngineError::invalid_argument(
                "HTTP embedding dimension, maxInputTokens, maxBatchSize and timeoutMs must be greater than zero",
            ));
        }
        match (self.auth, self.api_key_env.as_deref()) {
            (HttpAuth::None, None) => {}
            (HttpAuth::Bearer, Some(name)) if valid_env_name(name) => {}
            (HttpAuth::None, Some(_)) => {
                return Err(EngineError::invalid_argument(
                    "HTTP auth none must not specify apiKeyEnv",
                ));
            }
            _ => {
                return Err(EngineError::invalid_argument(
                    "HTTP bearer auth requires apiKeyEnv containing an environment variable name",
                ));
            }
        }
        if self.single_input && self.max_batch_size != 1 {
            return Err(EngineError::invalid_argument(
                "HTTP singleInput requires maxBatchSize 1",
            ));
        }
        if self.input_token_limit() == 0 {
            return Err(EngineError::invalid_argument(
                "HTTP prefixes exhaust maxInputTokens",
            ));
        }
        Ok(())
    }

    /// Reserve one token per UTF-8 prefix byte before the existing chunk policy.
    /// The tokenizer is server-owned, so use the larger of both purpose prefixes.
    pub(crate) fn input_token_limit(&self) -> usize {
        self.max_input_tokens
            .saturating_sub(self.query_prefix.len().max(self.document_prefix.len()))
    }

    pub(crate) fn model_info(&self, reference: &str) -> EngineResult<EmbeddingModelInfo> {
        self.validate()?;
        let alias = reference
            .strip_prefix("http/")
            .filter(|alias| {
                !alias.is_empty()
                    && alias
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
            .ok_or_else(|| {
                EngineError::invalid_argument("HTTP model reference must be http/<alias>")
            })?;
        EmbeddingModelInfo::new(
            ModelInfo::new(
                "http",
                alias,
                [ContentKind::Text, ContentKind::Code],
                self.max_batch_size,
            )?,
            self.dimension,
            self.metric,
            Some(self.input_token_limit()),
            None,
        )
    }

    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        if self.model != other.model
            || self.dimension != other.dimension
            || self.metric != other.metric
            || self.query_prefix != other.query_prefix
            || self.document_prefix != other.document_prefix
        {
            return Err(EngineError::invalid_argument(
                "HTTP embedding definition changed; rebuild the index",
            ));
        }
        Ok(())
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_prefix_budget_and_space_compatibility() {
        let config: HttpEmbeddingConfig = serde_json::from_value(serde_json::json!({
            "model": "served/model", "dimension": 4, "maxInputTokens": 100, "auth": "none",
            "queryPrefix": "query: ", "documentPrefix": "文: "
        }))
        .expect("valid test fixture");
        let info = config.model_info("http/alias").expect("valid test fixture");
        assert_eq!(info.model().provider(), "http");
        assert_eq!(info.model().name(), "alias");
        assert_eq!(info.max_input_tokens(), Some(93));
        for field in [
            "model",
            "dimension",
            "metric",
            "queryPrefix",
            "documentPrefix",
        ] {
            let mut value = serde_json::to_value(&config).expect("valid test fixture");
            value[field] = match field {
                "dimension" => serde_json::json!(8),
                "metric" => serde_json::json!("dot_product"),
                _ => serde_json::json!("changed"),
            };
            let changed = serde_json::from_value(value).expect("valid test fixture");
            assert!(config.ensure_index_compatible(&changed).is_err(), "{field}");
        }
        let mut query_prefix = config.clone();
        query_prefix.query_prefix = "문장🙂".into();
        query_prefix.document_prefix.clear();
        assert_eq!(
            query_prefix
                .model_info("http/alias")
                .expect("model")
                .max_input_tokens(),
            Some(90)
        );
        query_prefix.max_input_tokens = 10;
        assert!(query_prefix.validate().is_err());
        let mut exhausted = config.clone();
        exhausted.max_input_tokens = 7;
        assert!(exhausted.validate().is_err());
        let mut transport = config.clone();
        transport.timeout_ms = 1;
        config
            .ensure_index_compatible(&transport)
            .expect("valid test fixture");
    }
}
