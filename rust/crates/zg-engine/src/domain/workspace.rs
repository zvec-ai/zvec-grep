use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{ContentKind, GlobRule, SourcePath, model::EmbeddingModelInfo};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    pub name: String,
    pub root: PathBuf,
    pub scan: ScanRules,
    pub index: IndexState,
    pub created_epoch_ms: u64,
    pub updated_epoch_ms: u64,
}

impl Workspace {
    /// Validate names without normalizing their case or whitespace.
    pub(crate) fn validate_name(name: &str) -> EngineResult<()> {
        if name.is_empty()
            || name.trim() != name
            || matches!(name, "." | "..")
            || name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(EngineError::invalid_argument(format!(
                "invalid workspace name {name:?}",
            )));
        }
        Ok(())
    }

    #[allow(clippy::unnecessary_debug_formatting)] // Keep control characters in paths escaped.
    pub(crate) fn validate(&self) -> EngineResult<()> {
        Self::validate_name(&self.name)?;
        if !self.root.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace root {:?} must be an absolute path",
                self.root,
            )));
        }
        if let IndexState::Enabled(index) = &self.index {
            index.validate()?;
        }
        Ok(())
    }

    pub(crate) fn source_path(&self, relative: &SourcePath) -> PathBuf {
        self.root.join(relative)
    }

    pub(crate) fn index_enabled(&self) -> bool {
        matches!(self.index, IndexState::Enabled(_))
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    pub embeddings: Vec<EmbeddingModelInfo>,
    /// Each content kind has exactly one owning embedding model reference.
    pub routes: BTreeMap<ContentKind, String>,
    pub fts: FtsConfig,
}

impl IndexDescriptor {
    /// Normalize the legacy single-model selection into explicit content routes.
    pub(crate) fn single(embedding: EmbeddingModelInfo) -> Self {
        let reference = embedding.model.reference();
        let mut routes = BTreeMap::from([
            (ContentKind::Text, reference.clone()),
            (ContentKind::Table, reference.clone()),
        ]);
        if embedding.max_image_bytes.is_some() {
            routes.insert(ContentKind::Image, reference);
        }
        Self {
            embeddings: vec![embedding],
            routes,
            fts: FTS_CONFIG,
        }
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.embeddings.is_empty() || self.routes.is_empty() {
            return Err(EngineError::invalid_argument(
                "enabled index requires embedding models and content routes",
            ));
        }
        let mut references = BTreeSet::new();
        for embedding in &self.embeddings {
            embedding.validate()?;
            if !references.insert(embedding.model.reference()) {
                return Err(EngineError::invalid_argument(
                    "embedding model references must be unique",
                ));
            }
        }
        for (kind, reference) in &self.routes {
            let model = self
                .embeddings
                .iter()
                .find(|embedding| embedding.model.reference() == *reference)
                .ok_or_else(|| {
                    EngineError::invalid_argument(format!(
                        "content route {kind:?} references an unconfigured model: {reference}"
                    ))
                })?;
            if *kind == ContentKind::Image && model.max_image_bytes.is_none() {
                return Err(EngineError::invalid_argument(format!(
                    "image content route requires an image-capable embedding model: {reference}"
                )));
            }
        }
        if references
            .iter()
            .any(|reference| !self.routes.values().any(|route| route == reference))
        {
            return Err(EngineError::invalid_argument(
                "every embedding model must own at least one content route",
            ));
        }
        Ok(())
    }

    pub(crate) fn model_for(&self, kind: ContentKind) -> EngineResult<&EmbeddingModelInfo> {
        let reference = self.routes.get(&kind).ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "no embedding model configured for {kind:?} content"
            ))
        })?;
        self.embeddings
            .iter()
            .find(|embedding| embedding.model.reference() == *reference)
            .ok_or_else(|| {
                EngineError::internal(format!(
                    "content route refers to missing model: {reference}"
                ))
            })
    }

    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        if self.routes != other.routes
            || self.fts != other.fts
            || self.embeddings.len() != other.embeddings.len()
        {
            return Err(EngineError::invalid_argument(
                "existing index uses different embedding models, content routes or FTS configuration; rebuild the index",
            ));
        }
        for embedding in &self.embeddings {
            let other = other
                .embeddings
                .iter()
                .find(|candidate| candidate.model.reference() == embedding.model.reference())
                .ok_or_else(|| {
                    EngineError::invalid_argument(
                        "existing index uses different embedding models; rebuild the index",
                    )
                })?;
            embedding.ensure_index_compatible(other)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IndexState {
    Uninitialized,
    Disabled,
    Enabled(IndexDescriptor),
}

impl IndexState {
    pub(crate) fn descriptor(&self) -> Option<&IndexDescriptor> {
        match self {
            Self::Enabled(index) => Some(index),
            Self::Uninitialized | Self::Disabled => None,
        }
    }
}

/// Persistent rules for discovering and admitting workspace files to the index.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanRules {
    /// Ordered path rules relative to the workspace root.
    pub globs: Vec<GlobRule>,
    pub hidden: bool,
    pub follow_symlinks: bool,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,

    pub no_ignore: bool,
    pub ignore_files: Vec<PathBuf>,
    /// Traverse child Git repositories, including submodules and worktrees.
    pub nested_git: bool,
}

impl Default for ScanRules {
    fn default() -> Self {
        Self {
            globs: Vec::new(),
            hidden: false,
            follow_symlinks: false,
            max_depth: None,
            max_file_size_bytes: None,
            no_ignore: false,
            ignore_files: Vec::new(),
            nested_git: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Metric, ModelInfo};

    fn model(name: &str, image: bool) -> EmbeddingModelInfo {
        EmbeddingModelInfo {
            model: ModelInfo {
                provider: "test".into(),
                name: name.into(),
                endpoint: None,
            },
            dimension: 16,
            metric: Metric::Cosine,
            max_batch_size: 8,
            max_input_tokens: Some(512),
            max_image_bytes: image.then_some(1024),
        }
    }

    #[test]
    fn content_routes_have_one_owner_and_shared_models_are_stored_once() {
        let text = model("text", false);
        let image = model("vl", true);
        let mut index = IndexDescriptor::single(text.clone());
        index.embeddings.push(image.clone());
        index
            .routes
            .insert(ContentKind::Image, image.model.reference());
        index.validate().expect("valid partition");
        assert_eq!(
            index.model_for(ContentKind::Text).expect("text route"),
            &text
        );
        assert_eq!(
            index.model_for(ContentKind::Table).expect("table route"),
            &text
        );
        assert_eq!(
            index.model_for(ContentKind::Image).expect("image route"),
            &image
        );
        assert_eq!(index.embeddings.len(), 2);

        index
            .routes
            .insert(ContentKind::Image, text.model.reference());
        assert!(
            index.validate().is_err(),
            "text-only model cannot own images"
        );
        index
            .routes
            .insert(ContentKind::Image, "missing/model".into());
        assert!(index.validate().is_err(), "every route must resolve");
    }

    #[test]
    fn routing_and_chunk_limits_require_rebuild_but_runtime_changes_do_not() {
        let index = IndexDescriptor::single(model("vl", true));
        let mut changed = index.clone();
        changed.embeddings[0].model.endpoint = Some("https://other.example.test".into());
        changed.embeddings[0].max_batch_size = 64;
        index
            .ensure_index_compatible(&changed)
            .expect("runtime configuration");
        changed.routes.remove(&ContentKind::Table);
        assert!(index.ensure_index_compatible(&changed).is_err());
        changed = index.clone();
        changed.embeddings[0].max_input_tokens = Some(256);
        assert!(index.ensure_index_compatible(&changed).is_err());
        changed = index.clone();
        changed.embeddings.push(model("new", false));
        assert!(index.ensure_index_compatible(&changed).is_err());
    }
}
