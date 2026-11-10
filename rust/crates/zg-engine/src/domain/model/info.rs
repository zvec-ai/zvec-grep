use serde::{Deserialize, Serialize};

use crate::domain::ContentKind;
use crate::{EngineError, EngineResult};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelInfo {
    provider: String,
    name: String,
    content_kinds: Vec<ContentKind>,
    max_batch_size: usize,
}

impl ModelInfo {
    #[track_caller]
    pub(crate) fn new(
        provider: impl Into<String>,
        name: impl Into<String>,
        content_kinds: impl IntoIterator<Item = ContentKind>,
        max_batch_size: usize,
    ) -> EngineResult<Self> {
        let provider = provider.into();
        let name = name.into();
        for (field, value) in [("provider", provider.as_str()), ("name", name.as_str())] {
            if value.trim().is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "model {field} must be non-empty",
                )));
            }
        }
        let mut content_kinds: Vec<_> = content_kinds.into_iter().collect();
        if content_kinds.is_empty() {
            return Err(EngineError::invalid_argument(format!(
                "model {provider}/{name} content kinds must be non-empty",
            )));
        }
        if max_batch_size == 0 {
            return Err(EngineError::invalid_argument(format!(
                "model {provider}/{name} max_batch_size must be greater than zero",
            )));
        }
        content_kinds.sort_unstable();
        content_kinds.dedup();
        Ok(Self {
            provider,
            name,
            content_kinds,
            max_batch_size,
        })
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.name)
    }

    pub(crate) fn content_kinds(&self) -> &[ContentKind] {
        &self.content_kinds
    }

    pub(crate) fn supports_content(&self, kind: ContentKind) -> bool {
        self.content_kinds().contains(&kind)
    }

    pub(crate) fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }
}

impl<'de> Deserialize<'de> for ModelInfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fields {
            provider: String,
            name: String,
            content_kinds: Vec<ContentKind>,
            max_batch_size: usize,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(
            fields.provider,
            fields.name,
            fields.content_kinds,
            fields.max_batch_size,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_serializes_and_deserializes_valid_model_info() {
        let model = ModelInfo::new(
            "local",
            "test-model",
            [ContentKind::Text, ContentKind::Code],
            8,
        )
        .expect("model info");
        assert_eq!(model.provider(), "local");
        assert_eq!(model.name(), "test-model");
        assert_eq!(model.reference(), "local/test-model");
        assert_eq!(model.max_batch_size(), 8);
        assert_eq!(
            model.content_kinds(),
            [ContentKind::Text, ContentKind::Code]
        );

        let value = serde_json::to_value(&model).expect("serialize identity");
        assert_eq!(
            value,
            serde_json::json!({
                "provider": "local",
                "name": "test-model",
                "contentKinds": ["text", "code"],
                "maxBatchSize": 8,
            })
        );
        assert_eq!(
            serde_json::from_value::<ModelInfo>(value).expect("deserialize identity"),
            model
        );
    }

    #[test]
    fn rejects_invalid_construction_and_deserialization() {
        for (provider, name) in [
            ("", "test-model"),
            (" \t", "test-model"),
            ("local", ""),
            ("local", " \n"),
        ] {
            assert!(ModelInfo::new(provider, name, [ContentKind::Text], 1).is_err());
            let invalid = serde_json::json!({
                "provider": provider,
                "name": name,
                "contentKinds": ["text"],
                "maxBatchSize": 1,
            });
            assert!(serde_json::from_value::<ModelInfo>(invalid).is_err());
        }
        assert!(ModelInfo::new("local", "test-model", [], 1).is_err());
        assert!(ModelInfo::new("local", "test-model", [ContentKind::Text], 0).is_err());

        let value = serde_json::json!({
            "provider": "local",
            "name": "test-model",
            "contentKinds": ["text", "code"],
            "maxBatchSize": 1,
        });
        let mut empty_kinds = value.clone();
        empty_kinds["contentKinds"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ModelInfo>(empty_kinds).is_err());
        let mut zero_batch_size = value.clone();
        zero_batch_size["maxBatchSize"] = serde_json::json!(0);
        assert!(serde_json::from_value::<ModelInfo>(zero_batch_size).is_err());
        for field in ["provider", "name", "contentKinds", "maxBatchSize"] {
            let mut missing = value.clone();
            missing
                .as_object_mut()
                .expect("model info object")
                .remove(field);
            assert!(serde_json::from_value::<ModelInfo>(missing).is_err());
        }
    }
}
