use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{ContentKind, model::EmbeddingModelInfo};

#[derive(Clone, Debug, Eq, PartialEq)]
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    default_embedding_model: EmbeddingModelInfo,
    embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
    fts: FtsConfig,
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
        Ok(Self {
            default_embedding_model,
            embedding_routes,
            fts,
        })
    }

    pub(crate) fn default_embedding_model(&self) -> &EmbeddingModelInfo {
        &self.default_embedding_model
    }

    pub(crate) fn embedding_routes(&self) -> &BTreeMap<ContentKind, EmbeddingModelInfo> {
        &self.embedding_routes
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
