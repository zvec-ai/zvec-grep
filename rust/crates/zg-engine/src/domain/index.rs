use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{
    ContentKind,
    model::{EmbeddingModelInfo, EncryptedApiKey, ModelConfig},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "state",
    content = "descriptor",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum IndexState {
    Uninitialized,
    Disabled,
    Enabled(Box<IndexDescriptor>),
}

impl IndexState {
    pub(crate) fn descriptor(&self) -> Option<&IndexDescriptor> {
        match self {
            Self::Enabled(index) => Some(index),
            Self::Uninitialized | Self::Disabled => None,
        }
    }

    pub(crate) fn invalidate_runtime_settings(&mut self) {
        if let Self::Enabled(index) = self {
            for config in index.embedding_configs.values_mut() {
                config.device = None;
                config.cache_dir = None;
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndexDescriptor {
    default_embedding_model: EmbeddingModelInfo,
    embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
    embedding_configs: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
    graph_indexing: bool,
    #[serde(skip)]
    fts: FtsConfig,
}

impl IndexDescriptor {
    pub(crate) fn new(
        default_embedding_model: EmbeddingModelInfo,
        embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
        embedding_configs: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
        graph_indexing: bool,
        fts: FtsConfig,
    ) -> EngineResult<Self> {
        for (kind, embedding) in &embedding_routes {
            if !embedding.model().supports_content(*kind) {
                return Err(EngineError::invalid_argument(format!(
                    "embedding route for {} refers to model {}, which does not support this content kind",
                    kind.as_str(),
                    embedding.model().reference(),
                )));
            }
            for other in std::iter::once(&default_embedding_model).chain(embedding_routes.values())
            {
                if embedding.model().reference() == other.model().reference() && embedding != other
                {
                    return Err(EngineError::invalid_argument(format!(
                        "conflicting configurations for embedding model {}",
                        embedding.model().reference(),
                    )));
                }
            }
        }
        for reference in embedding_configs.keys() {
            if !std::iter::once(&default_embedding_model)
                .chain(embedding_routes.values())
                .any(|model| model.model().reference() == *reference)
            {
                return Err(EngineError::invalid_argument(format!(
                    "inconsistent index metadata: configuration exists for model {reference}, but the model is not referenced by defaultEmbeddingModel or embeddingRoutes"
                )));
            }
        }
        for model in std::iter::once(&default_embedding_model).chain(embedding_routes.values()) {
            let reference = model.model().reference();
            if model.model().provider() == "http" {
                let config = embedding_configs.get(&reference).ok_or_else(|| {
                    EngineError::invalid_argument(
                        "HTTP model descriptor is missing from index metadata",
                    )
                })?;
                let http = config.http.as_ref().ok_or_else(|| {
                    EngineError::invalid_argument(
                        "HTTP model descriptor is missing from index metadata",
                    )
                })?;
                if http.auth == crate::domain::model::HttpAuth::None && config.api_key.is_some() {
                    return Err(EngineError::invalid_argument(
                        "HTTP model with auth none cannot store an API key",
                    ));
                }
                let schema = http.model_info(&reference)?;
                schema.ensure_index_compatible(model)?;
                if schema.model().content_kinds() != model.model().content_kinds() {
                    return Err(EngineError::invalid_argument(
                        "HTTP model content kinds differ from its descriptor",
                    ));
                }
                let endpoint = config.endpoint.as_deref().ok_or_else(|| {
                    EngineError::invalid_argument(
                        "HTTP model endpoint is missing from index metadata",
                    )
                })?;
                crate::authorization::canonical_remote_endpoint(&reference, endpoint)?;
            } else if embedding_configs
                .get(&reference)
                .is_some_and(|config| config.http.is_some())
            {
                return Err(EngineError::invalid_argument(
                    "HTTP descriptor requires an http model reference",
                ));
            }
        }
        Ok(Self {
            default_embedding_model,
            embedding_routes,
            embedding_configs,
            graph_indexing,
            fts,
        })
    }

    pub(crate) fn default_embedding_model(&self) -> &EmbeddingModelInfo {
        &self.default_embedding_model
    }

    pub(crate) fn embedding_routes(&self) -> &BTreeMap<ContentKind, EmbeddingModelInfo> {
        &self.embedding_routes
    }

    pub(crate) fn embedding_configs(&self) -> &BTreeMap<String, ModelConfig<EncryptedApiKey>> {
        &self.embedding_configs
    }

    pub(crate) fn graph_indexing(&self) -> bool {
        self.graph_indexing
    }

    #[cfg(test)]
    pub(crate) fn set_graph_indexing(&mut self, enabled: bool) {
        self.graph_indexing = enabled;
    }

    pub(crate) fn fts(&self) -> FtsConfig {
        self.fts
    }

    pub(crate) fn model_for(&self, kind: ContentKind) -> Option<&EmbeddingModelInfo> {
        let default = &self.default_embedding_model;
        self.embedding_routes
            .get(&kind)
            .or_else(|| default.model().supports_content(kind).then_some(default))
    }

    pub(crate) fn content_indexes(&self) -> Vec<ContentIndex> {
        let kinds: BTreeSet<_> = self
            .default_embedding_model
            .model()
            .content_kinds()
            .iter()
            .chain(self.embedding_routes.keys())
            .copied()
            .collect();
        kinds
            .into_iter()
            .filter_map(|kind| {
                self.model_for(kind).map(|embedding| ContentIndex {
                    kind,
                    embedding: embedding.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn embeddings(&self) -> Vec<&EmbeddingModelInfo> {
        let mut models = BTreeMap::new();
        for embedding in
            std::iter::once(&self.default_embedding_model).chain(self.embedding_routes.values())
        {
            models
                .entry(embedding.model().reference())
                .or_insert(embedding);
        }
        models.into_values().collect()
    }

    pub(crate) fn ensure_index_compatible(&self, requested: &Self) -> EngineResult<()> {
        let stored_indexes = self.content_indexes();
        let requested_indexes = requested.content_indexes();
        let kind_names = |indexes: &[ContentIndex]| {
            indexes
                .iter()
                .map(|index| index.kind.as_str())
                .collect::<Vec<_>>()
        };
        let stored_kinds = kind_names(&stored_indexes);
        let requested_kinds = kind_names(&requested_indexes);
        if stored_kinds != requested_kinds {
            return Err(EngineError::invalid_argument(format!(
                "indexed content kinds would change from [{}] to [{}]",
                stored_kinds.join(", "),
                requested_kinds.join(", "),
            )));
        }
        for (stored, next) in stored_indexes.iter().zip(&requested_indexes) {
            stored
                .embedding
                .ensure_index_compatible(&next.embedding)
                .map_err(|error| {
                    EngineError::invalid_argument(format!(
                        "{} {}",
                        stored.kind.as_str(),
                        error.message(),
                    ))
                })?;
            if stored.embedding.model().provider() == "http" {
                let reference = stored.embedding.model().reference();
                let saved_config = &self.embedding_configs[&reference];
                let next_config = &requested.embedding_configs[&reference];
                saved_config
                    .http
                    .as_ref()
                    .expect("validated HTTP descriptor")
                    .ensure_index_compatible(
                        next_config
                            .http
                            .as_ref()
                            .expect("validated HTTP descriptor"),
                    )?;
                let canonical_endpoint = |config: &ModelConfig<EncryptedApiKey>| {
                    crate::authorization::canonical_remote_endpoint(
                        &reference,
                        config.endpoint.as_deref().expect("validated HTTP endpoint"),
                    )
                };
                if canonical_endpoint(saved_config)? != canonical_endpoint(next_config)? {
                    return Err(EngineError::invalid_argument(
                        "HTTP embedding endpoint changed; rebuild the index",
                    ));
                }
            }
        }

        if self.graph_indexing != requested.graph_indexing {
            return Err(EngineError::invalid_argument(if requested.graph_indexing {
                "code graph indexing would change from disabled to enabled"
            } else {
                "code graph indexing would change from enabled to disabled"
            }));
        }

        if self.fts.tokenizer != requested.fts.tokenizer {
            return Err(EngineError::invalid_argument(format!(
                "full-text tokenizer would change from {} to {}",
                self.fts.tokenizer, requested.fts.tokenizer,
            )));
        }
        if self.fts.filters != requested.fts.filters {
            return Err(EngineError::invalid_argument(format!(
                "full-text filters would change from [{}] to [{}]",
                self.fts.filters.join(", "),
                requested.fts.filters.join(", "),
            )));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for IndexDescriptor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fields {
            default_embedding_model: EmbeddingModelInfo,
            #[serde(default, with = "serde_with::rust::maps_duplicate_key_is_error")]
            embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
            #[serde(default, deserialize_with = "deserialize_embedding_configs")]
            embedding_configs: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
            #[serde(default)]
            graph_indexing: bool,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(
            fields.default_embedding_model,
            fields.embedding_routes,
            fields.embedding_configs,
            fields.graph_indexing,
            FTS_CONFIG,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn deserialize_embedding_configs<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ModelConfig<EncryptedApiKey>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_with::rust::maps_duplicate_key_is_error::deserialize(deserializer)
        .map_err(|_| serde::de::Error::custom("invalid embeddingConfigs in index manifest"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContentIndex {
    pub kind: ContentKind,
    pub embedding: EmbeddingModelInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FtsConfig {
    pub tokenizer: &'static str,
    pub filters: &'static [&'static str],
}

/// Fixed FTS configuration for the current physical index format.
pub(crate) const FTS_CONFIG: FtsConfig = FtsConfig {
    tokenizer: "jieba",
    filters: &["lowercase"],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{EmbeddingMetric, ModelInfo};

    fn embedding(name: &str, kinds: impl IntoIterator<Item = ContentKind>) -> EmbeddingModelInfo {
        EmbeddingModelInfo::new(
            ModelInfo::new("local", name, kinds, 8).expect("model"),
            8,
            EmbeddingMetric::Cosine,
            Some(128),
            None,
        )
        .expect("embedding")
    }

    fn descriptor() -> IndexDescriptor {
        IndexDescriptor::new(
            embedding("test", [ContentKind::Text, ContentKind::Code]),
            BTreeMap::new(),
            BTreeMap::new(),
            false,
            FTS_CONFIG,
        )
        .expect("index descriptor")
    }

    #[test]
    fn content_indexes_resolve_models_by_kind() {
        use ContentKind::{Code, Image, Text};

        let base = descriptor();
        assert_eq!(base.model_for(Image), None);

        let default = base.default_embedding_model;
        let media = embedding("media", [Code, Image]);
        let index = IndexDescriptor::new(
            default.clone(),
            BTreeMap::from([(Code, media.clone()), (Image, media.clone())]),
            BTreeMap::new(),
            false,
            FTS_CONFIG,
        )
        .expect("valid routes");
        let expected = [(Text, default), (Code, media.clone()), (Image, media)];
        assert_eq!(
            index.content_indexes(),
            expected.map(|(kind, embedding)| ContentIndex { kind, embedding })
        );
    }

    #[test]
    fn index_states_round_trip_with_validated_descriptors() {
        for state in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            IndexState::Enabled(Box::new(IndexDescriptor {
                graph_indexing: true,
                ..descriptor()
            })),
        ] {
            let json = serde_json::to_string(&state).expect("encode index");
            assert_eq!(
                serde_json::from_str::<IndexState>(&json).expect("decode index"),
                state
            );
            assert!(!json.contains("fts"));
        }
        for json in [
            r#"{"state":"enabled"}"#,
            r#"{"state":"disabled","descriptor":{}}"#,
            r#"{"state":"uninitialized","descriptor":{}}"#,
            r#"{"state":"disabled","unexpected":true}"#,
        ] {
            assert!(serde_json::from_str::<IndexState>(json).is_err(), "{json}");
        }

        let enabled = IndexState::Enabled(Box::new(descriptor()));
        let mut json = serde_json::to_value(&enabled).expect("encode enabled index");
        let fields = json["descriptor"]
            .as_object_mut()
            .expect("descriptor object");
        assert_eq!(fields.remove("graphIndexing"), Some(false.into()));
        let restored: IndexState =
            serde_json::from_value(json.clone()).expect("descriptor defaults");
        assert_eq!(restored, enabled);
        json["descriptor"]["graphIndexing"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<IndexState>(json).is_err());
    }

    #[test]
    fn incompatible_content_kinds_report_both_sets_even_when_the_counts_match() {
        let stored = descriptor();
        let mut value = serde_json::to_value(&stored).expect("descriptor");
        value["defaultEmbeddingModel"]["model"]["contentKinds"] =
            serde_json::json!(["text", "image"]);
        let requested = serde_json::from_value(value).expect("image instead of code");
        let error = stored
            .ensure_index_compatible(&requested)
            .expect_err("kind mismatch");
        assert_eq!(
            error.message(),
            "indexed content kinds would change from [text, code] to [text, image]"
        );
    }

    #[test]
    fn index_settings_compatibility_reports_the_first_change() {
        let base = descriptor();
        let mut tokenizer = base.clone();
        tokenizer.fts.tokenizer = "simple";
        let mut filters = base.clone();
        filters.fts.filters = &[];
        let mut graph_and_tokenizer = tokenizer.clone();
        graph_and_tokenizer.graph_indexing = true;

        base.ensure_index_compatible(&base)
            .expect("matching settings");
        for (stored, requested, message) in [
            (
                &base,
                &tokenizer,
                "full-text tokenizer would change from jieba to simple",
            ),
            (
                &base,
                &filters,
                "full-text filters would change from [lowercase] to []",
            ),
            (
                &base,
                &graph_and_tokenizer,
                "code graph indexing would change from disabled to enabled",
            ),
            (
                &graph_and_tokenizer,
                &base,
                "code graph indexing would change from enabled to disabled",
            ),
        ] {
            let error = stored
                .ensure_index_compatible(requested)
                .expect_err("settings changed");
            assert_eq!(error.message(), message);
        }
    }

    #[test]
    fn index_metadata_rejects_duplicate_routes_and_configs_without_exposing_credentials() {
        let json = serde_json::to_string(&descriptor()).expect("descriptor JSON");
        let model =
            serde_json::to_string(descriptor().default_embedding_model()).expect("model JSON");
        let duplicate_route = json.replace(
            "\"embeddingRoutes\":{}",
            &format!(r#""embeddingRoutes":{{"text":{model},"text":{model}}}"#),
        );
        let duplicate_config = json.replace(
            "\"embeddingConfigs\":{}",
            r#""embeddingConfigs":{"local/test":{},"local/test":{}}"#,
        );
        assert!(serde_json::from_str::<IndexDescriptor>(&duplicate_route).is_err());
        let plaintext = json.replace(
            "\"embeddingConfigs\":{}",
            r#""embeddingConfigs":{"local/test":{"apiKey":"private-token"}}"#,
        );
        for value in [duplicate_config, plaintext] {
            let error = serde_json::from_str::<IndexDescriptor>(&value)
                .expect_err("invalid embedding config")
                .to_string();
            assert!(error.contains("invalid embeddingConfigs in index manifest"));
            assert!(!error.contains("private-token"));
        }
    }
}
