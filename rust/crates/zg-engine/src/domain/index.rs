use std::{collections::BTreeMap, fmt, marker::PhantomData};

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
    #[serde(skip)]
    fts: FtsConfig,
}

impl<'de> Deserialize<'de> for IndexDescriptor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fields {
            default_embedding_model: EmbeddingModelInfo,
            #[serde(default, deserialize_with = "deserialize_unique_map")]
            embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
            #[serde(default, deserialize_with = "deserialize_unique_map")]
            embedding_configs: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(
            fields.default_embedding_model,
            fields.embedding_routes,
            fields.embedding_configs,
            FTS_CONFIG,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn deserialize_unique_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    struct MapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> serde::de::Visitor<'de> for MapVisitor<K, V>
    where
        K: Deserialize<'de> + Ord + fmt::Debug,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an index metadata map with unique keys")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some(key) = map.next_key::<K>()? {
                let entry = match values.entry(key) {
                    std::collections::btree_map::Entry::Occupied(entry) => {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate index metadata key {:?}",
                            entry.key(),
                        )));
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => entry,
                };
                // Serde errors can include the input value, including plaintext credentials.
                let value = map.next_value::<V>().map_err(|_| {
                    <A::Error as serde::de::Error>::custom(format!(
                        "invalid index metadata for {:?}",
                        entry.key(),
                    ))
                })?;
                entry.insert(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(MapVisitor(PhantomData))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContentIndex {
    pub kind: ContentKind,
    pub embedding: EmbeddingModelInfo,
}

impl IndexDescriptor {
    pub(crate) fn new(
        default_embedding_model: EmbeddingModelInfo,
        embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
        embedding_configs: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
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
        Ok(Self {
            default_embedding_model,
            embedding_routes,
            embedding_configs,
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

    pub(crate) fn fts(&self) -> FtsConfig {
        self.fts
    }

    /// An unsupported default yields no content index.
    pub(crate) fn model_for(&self, kind: ContentKind) -> Option<&EmbeddingModelInfo> {
        if let Some(embedding) = self.embedding_routes.get(&kind) {
            return Some(embedding);
        }
        self.default_embedding_model
            .model()
            .supports_content(kind)
            .then_some(&self.default_embedding_model)
    }

    pub(crate) fn content_indexes(&self) -> Vec<ContentIndex> {
        let mut indexes = Vec::new();
        for kind in [ContentKind::Text, ContentKind::Code, ContentKind::Image] {
            if let Some(embedding) = self.model_for(kind) {
                indexes.push(ContentIndex {
                    kind,
                    embedding: embedding.clone(),
                });
            }
        }
        indexes
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

    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        let indexes = self.content_indexes();
        let other_indexes = other.content_indexes();
        if self.fts != other.fts || indexes.len() != other_indexes.len() {
            return Err(EngineError::invalid_argument(
                "existing index uses different content kinds or FTS configuration; rebuild the index",
            ));
        }
        for (index, other_index) in indexes.iter().zip(&other_indexes) {
            if index.kind != other_index.kind {
                return Err(EngineError::invalid_argument(
                    "existing index uses different content kinds; rebuild the index",
                ));
            }
            index
                .embedding
                .ensure_index_compatible(&other_index.embedding)?;
        }
        Ok(())
    }
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

    fn descriptor() -> IndexDescriptor {
        IndexDescriptor::new(
            EmbeddingModelInfo::new(
                ModelInfo::new("local", "test", [ContentKind::Text, ContentKind::Code], 8)
                    .expect("model"),
                8,
                EmbeddingMetric::Cosine,
                Some(128),
                None,
            )
            .expect("embedding"),
            BTreeMap::new(),
            BTreeMap::new(),
            FTS_CONFIG,
        )
        .expect("index descriptor")
    }

    #[test]
    fn index_states_round_trip_with_validated_descriptors() {
        for state in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            IndexState::Enabled(Box::new(descriptor())),
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
        for value in [duplicate_route, duplicate_config] {
            assert!(serde_json::from_str::<IndexDescriptor>(&value).is_err());
        }
        let plaintext = json.replace(
            "\"embeddingConfigs\":{}",
            r#""embeddingConfigs":{"local/test":{"apiKey":"private-token"}}"#,
        );
        let error = serde_json::from_str::<IndexDescriptor>(&plaintext).expect_err("plaintext key");
        assert!(!error.to_string().contains("private-token"));
    }
}
