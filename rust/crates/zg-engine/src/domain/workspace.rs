use std::{collections::BTreeMap, path::PathBuf};

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
    /// The text route references the workspace's single embedding model.
    pub routes: BTreeMap<ContentKind, String>,
    pub fts: FtsConfig,
}

impl IndexDescriptor {
    /// Configure the single text embedding model supported by this version.
    pub(crate) fn single(embedding: EmbeddingModelInfo) -> Self {
        let reference = embedding.model.reference();
        Self {
            embeddings: vec![embedding],
            routes: BTreeMap::from([(ContentKind::Text, reference)]),
            fts: FTS_CONFIG,
        }
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        let [embedding] = self.embeddings.as_slice() else {
            return Err(EngineError::invalid_argument(
                "enabled index requires exactly one text embedding model",
            ));
        };
        embedding.validate()?;
        if self.routes.len() != 1
            || self.routes.get(&ContentKind::Text) != Some(&embedding.model.reference())
        {
            return Err(EngineError::invalid_argument(
                "enabled index requires exactly one text content route referencing its embedding model",
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
        self.validate()?;
        other.validate()?;
        if self.routes != other.routes || self.fts != other.fts {
            return Err(EngineError::invalid_argument(
                "existing index uses a different embedding model or FTS configuration; rebuild the index",
            ));
        }
        self.embeddings[0].ensure_index_compatible(&other.embeddings[0])
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
    /// Ripgrep type names used by scanning and watcher admission.
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
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
            file_types: Vec::new(),
            excluded_file_types: Vec::new(),
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
    fn single_model_embeds_only_text_even_when_the_model_accepts_images() {
        let index = IndexDescriptor::single(model("vl", true));
        index.validate().expect("single text model");
        assert_eq!(index.embeddings.len(), 1);
        assert_eq!(
            index.routes,
            BTreeMap::from([(ContentKind::Text, "test/vl".into())])
        );
        assert!(index.model_for(ContentKind::Table).is_err());
        assert!(index.model_for(ContentKind::Image).is_err());
    }

    #[test]
    fn index_descriptor_requires_one_model_and_its_text_route() {
        let valid = IndexDescriptor::single(model("text", false));
        assert_eq!(
            valid.model_for(ContentKind::Text).expect("text model"),
            &valid.embeddings[0]
        );
        let mut invalid = valid.clone();
        invalid.embeddings.clear();
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.embeddings.push(model("second", false));
        assert!(invalid.validate().is_err());
        for kind in [ContentKind::Table, ContentKind::Image] {
            invalid = valid.clone();
            invalid.routes.insert(kind, "test/text".into());
            assert!(invalid.validate().is_err());
            invalid.routes.remove(&ContentKind::Text);
            assert!(invalid.validate().is_err());
        }
        invalid = valid.clone();
        invalid.routes.clear();
        assert!(invalid.validate().is_err());
        invalid
            .routes
            .insert(ContentKind::Text, "test/missing".into());
        assert!(invalid.validate().is_err());
        invalid = valid;
        invalid.embeddings[0].dimension = 0;
        assert!(invalid.validate().is_err());
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
        changed.routes.insert(ContentKind::Table, "test/vl".into());
        assert!(index.ensure_index_compatible(&changed).is_err());
        changed = index.clone();
        changed.embeddings[0].max_input_tokens = Some(256);
        assert!(index.ensure_index_compatible(&changed).is_err());
        changed = index.clone();
        changed.embeddings.push(model("new", false));
        assert!(index.ensure_index_compatible(&changed).is_err());
    }
}
