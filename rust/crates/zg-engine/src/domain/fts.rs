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
    /// Returns an error for an unsupported or inconsistent analysis configuration.
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
    /// Validates the supported native analysis options before index mutation.
    /// # Errors
    /// Returns an error for unknown names, invalid values, or inapplicable parameters.
    pub fn validate(&self) -> EngineResult<()> {
        if !matches!(
            self.tokenizer.as_str(),
            "jieba" | "standard" | "ngram" | "whitespace"
        ) {
            return Err(invalid(
                "tokenizer must be jieba, standard, ngram, or whitespace",
            ));
        }
        for filter in &self.filters {
            if !matches!(filter.as_str(), "lowercase" | "ascii_folding" | "stemmer") {
                return Err(invalid(format!(
                    "unknown filter {filter:?}; expected lowercase, ascii_folding, or stemmer"
                )));
            }
        }
        for (key, value) in &self.extra_params {
            let applicable = match key.as_str() {
                "stemmer_lang" => {
                    let language = value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| invalid("stemmer_lang must be a nonempty string"))?;
                    if !STEMMER_LANGUAGES.contains(&language) {
                        return Err(invalid(format!(
                            "unsupported stemmer_lang {language:?}; use a documented Snowball language name"
                        )));
                    }
                    self.filters.iter().any(|filter| filter == "stemmer")
                }
                "max_token_length" => {
                    positive_integer(key, value, 1_048_576)?;
                    self.tokenizer == "standard"
                }
                "ngram_min" | "ngram_max" => {
                    positive_integer(key, value, u64::from(u32::MAX))?;
                    self.tokenizer == "ngram"
                }
                "token_chars" => {
                    let values = value
                        .as_array()
                        .ok_or_else(|| invalid("token_chars must be an array"))?;
                    if values.iter().any(|value| {
                        !matches!(
                            value.as_str(),
                            Some("letter" | "digit" | "whitespace" | "punctuation" | "symbol")
                        )
                    }) {
                        return Err(invalid(
                            "token_chars accepts letter, digit, whitespace, punctuation, or symbol",
                        ));
                    }
                    self.tokenizer == "ngram"
                }
                "cut_mode" => {
                    if !matches!(value.as_str(), Some("search" | "mix" | "full" | "hmm")) {
                        return Err(invalid("cut_mode must be search, mix, full, or hmm"));
                    }
                    self.tokenizer == "jieba"
                }
                "jieba_dict_dir" | "user_dict_path" => {
                    if !value.as_str().is_some_and(|path| {
                        std::path::Path::new(path).is_absolute() && !path.contains('\0')
                    }) {
                        return Err(invalid(format!("{key} must be an absolute path")));
                    }
                    self.tokenizer == "jieba"
                }
                _ => return Err(invalid(format!("unknown extra parameter {key:?}"))),
            };
            if !applicable {
                return Err(invalid(format!(
                    "extra parameter {key:?} does not apply to the selected tokenizer and filters"
                )));
            }
        }
        if self.tokenizer == "ngram" {
            let min = self
                .extra_params
                .get("ngram_min")
                .and_then(Value::as_u64)
                .unwrap_or(2);
            let max = self
                .extra_params
                .get("ngram_max")
                .and_then(Value::as_u64)
                .unwrap_or(2);
            if max < min || max - min > 1 {
                return Err(invalid(
                    "ngram_min must be <= ngram_max, with a difference of at most 1",
                ));
            }
        }
        Ok(())
    }
}

fn positive_integer(key: &str, value: &Value, max: u64) -> EngineResult<()> {
    if value
        .as_u64()
        .is_some_and(|value| (1..=max).contains(&value))
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "{key} must be an integer between 1 and {max}"
        )))
    }
}

fn invalid(message: impl std::fmt::Display) -> EngineError {
    EngineError::invalid_argument(format!("invalid FTS configuration: {message}"))
}

// Canonical Snowball names supported by the native tokenizer; aliases are
// deliberately excluded so persisted configurations have predictable spelling.
const STEMMER_LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "catalan",
    "czech",
    "danish",
    "dutch",
    "dutch_porter",
    "english",
    "esperanto",
    "estonian",
    "finnish",
    "french",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "lithuanian",
    "nepali",
    "norwegian",
    "persian",
    "polish",
    "porter",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "sesotho",
    "spanish",
    "swedish",
    "tamil",
    "turkish",
    "yiddish",
];

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
    fn fts_configuration_rejects_unknown_or_inapplicable_options() {
        for patch in [
            json!({"tokenizer": "missing"}),
            json!({"filters": ["stopwords"]}),
            json!({"extraParams": {"stemmer_lang": "russian"}}),
            json!({"filters": ["stemmer"], "extraParams": {"stemmer_lang": "missing"}}),
            json!({"extraParams": {"cut_mode": "invalid"}}),
            json!({"extraParams": {"unknown": true}}),
            json!({"tokenizer": "standard", "extraParams": {"max_token_length": 0}}),
            json!({"tokenizer": "standard", "extraParams": {"max_token_length": "10"}}),
            json!({"tokenizer": "ngram", "extraParams": {"ngram_min": 4, "ngram_max": 2}}),
            json!({"tokenizer": "ngram", "extraParams": {"ngram_min": 1, "ngram_max": 4}}),
            json!({"tokenizer": "ngram", "extraParams": {"token_chars": ["unknown"]}}),
        ] {
            let update: FtsConfigUpdate = serde_json::from_value(patch.clone()).expect("update");
            assert!(update.apply(&FtsConfig::default()).is_err(), "{patch}");
        }
    }
}
