//! Persisted full-text analysis and explicit, field-wise configuration updates.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{EngineError, EngineResult};

/// Analysis shared by indexing and querying one workspace's full-text fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FtsConfig {
    pub tokenizer: String,
    pub filters: Vec<String>,
    #[serde(default)]
    pub extra_params: Map<String, Value>,
}

impl Default for FtsConfig {
    fn default() -> Self {
        Self {
            tokenizer: "jieba".into(),
            filters: vec!["lowercase".into()],
            extra_params: Map::new(),
        }
    }
}

/// Omitted fields retain their base value; supplied lists and objects replace it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FtsConfigUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_params: Option<Map<String, Value>>,
}

impl FtsConfigUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokenizer.is_none() && self.filters.is_none() && self.extra_params.is_none()
    }

    /// Applies explicit overrides without merging filter lists or parameter objects.
    /// # Errors
    /// Returns an error for malformed tokenizer or filter names.
    pub fn apply(&self, base: &FtsConfig) -> EngineResult<FtsConfig> {
        let config = FtsConfig {
            tokenizer: self
                .tokenizer
                .clone()
                .unwrap_or_else(|| base.tokenizer.clone()),
            filters: self.filters.clone().unwrap_or_else(|| base.filters.clone()),
            extra_params: self
                .extra_params
                .clone()
                .unwrap_or_else(|| base.extra_params.clone()),
        };
        config.validate()?;
        Ok(config)
    }
}

impl FtsConfig {
    /// Checks that analyzer names can be passed to the native string API.
    /// # Errors
    /// Returns an error for blank names or embedded null bytes.
    pub fn validate(&self) -> EngineResult<()> {
        for name in std::iter::once(&self.tokenizer).chain(&self.filters) {
            if name.trim().is_empty() || name.contains('\0') {
                return Err(EngineError::invalid_argument(
                    "FTS tokenizer and filter names must be nonempty strings without null bytes",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fts_updates_replace_lists_and_objects_and_preserve_omitted_fields() {
        let base: FtsConfig = serde_json::from_value(json!({
            "tokenizer": "standard", "filters": ["lowercase", "stemmer"],
            "extraParams": {"stemmer_lang": "russian", "max_token_length": 100}
        }))
        .expect("base config");
        let patch: FtsConfigUpdate = serde_json::from_value(json!({
            "filters": [], "extraParams": {}
        }))
        .expect("explicit empty values");
        let config = patch.apply(&base).expect("clear filters and parameters");
        assert_eq!(config.tokenizer, "standard");
        assert!(config.filters.is_empty());
        assert!(config.extra_params.is_empty());
        assert_eq!(
            FtsConfigUpdate::default().apply(&base).expect("inherit"),
            base
        );
        let patch: FtsConfigUpdate = serde_json::from_value(json!({
            "extraParams": {"stemmer_lang": "english"}
        }))
        .expect("replace object");
        assert_eq!(patch.apply(&base).expect("replace").extra_params.len(), 1);
    }

    #[test]
    fn fts_configuration_preserves_native_names_and_parameters() {
        let value = json!({
            "tokenizer": "another-tokenizer", "filters": ["another-filter"],
            "extraParams": {"another-option": {"values": [1, 2]}}
        });
        let update: FtsConfigUpdate = serde_json::from_value(value.clone()).expect("update");
        let config = update.apply(&FtsConfig::default()).expect("pass through");
        assert_eq!(serde_json::to_value(config).expect("encode"), value);
    }
}
