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
        self.scan.validate()?;
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

impl ScanRules {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.max_file_size_bytes == Some(0) {
            return Err(EngineError::invalid_argument(
                "max_file_size_bytes must be greater than zero",
            ));
        }
        Ok(())
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    pub embeddings: Vec<EmbeddingModelInfo>,
    /// Model used when a content kind has no explicit route.
    pub default_model_ref: String,
    /// Explicit content-kind overrides; each reference must support its kind.
    pub routes: BTreeMap<ContentKind, String>,
    pub fts: FtsConfig,
}

impl IndexDescriptor {
    /// Use one default model for every content kind it supports.
    #[cfg(test)]
    pub(crate) fn single(embedding: EmbeddingModelInfo) -> Self {
        Self {
            default_model_ref: embedding.model.reference(),
            embeddings: vec![embedding],
            routes: BTreeMap::new(),
            fts: FTS_CONFIG,
        }
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.embeddings.is_empty() {
            return Err(EngineError::invalid_argument(
                "enabled index requires at least one embedding model",
            ));
        }
        let mut references = Vec::with_capacity(self.embeddings.len());
        for embedding in &self.embeddings {
            embedding.validate()?;
            let reference = embedding.model.reference();
            if references.contains(&reference) {
                return Err(EngineError::invalid_argument(format!(
                    "duplicate workspace embedding model: {reference}",
                )));
            }
            references.push(reference);
        }
        self.default_model()?;
        for kind in self.routes.keys() {
            self.model_for(*kind)?;
        }
        Ok(())
    }

    pub(crate) fn default_model(&self) -> EngineResult<&EmbeddingModelInfo> {
        self.embedding_by_ref(&self.default_model_ref)
    }

    /// An unsupported default yields no model; an invalid explicit route is an error.
    pub(crate) fn model_for(&self, kind: ContentKind) -> EngineResult<Option<&EmbeddingModelInfo>> {
        if let Some(reference) = self.routes.get(&kind) {
            let embedding = self.embedding_by_ref(reference)?;
            if !embedding.model.supports_content(kind) {
                return Err(EngineError::invalid_argument(format!(
                    "embedding route for {} refers to model {reference}, which does not support this content kind",
                    kind.as_str(),
                )));
            }
            return Ok(Some(embedding));
        }
        let embedding = self.default_model()?;
        Ok(embedding.model.supports_content(kind).then_some(embedding))
    }

    fn embedding_by_ref(&self, reference: &str) -> EngineResult<&EmbeddingModelInfo> {
        self.embeddings
            .iter()
            .find(|embedding| embedding.model.reference() == reference)
            .ok_or_else(|| {
                EngineError::invalid_argument(format!(
                    "workspace embedding model is missing: {reference}",
                ))
            })
    }

    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        self.validate()?;
        other.validate()?;
        if self.default_model_ref != other.default_model_ref
            || self.routes != other.routes
            || self.fts != other.fts
            || self.embeddings.len() != other.embeddings.len()
        {
            return Err(EngineError::invalid_argument(
                "existing index uses different embedding models, routing or FTS configuration; rebuild the index",
            ));
        }
        for embedding in &self.embeddings {
            let reference = embedding.model.reference();
            let other_embedding = other.embeddings.iter()
                .find(|candidate| candidate.model.reference() == reference)
                .ok_or_else(|| EngineError::invalid_argument(format!(
                    "existing index uses a different embedding model {reference}; rebuild the index",
                )))?;
            embedding.ensure_index_compatible(other_embedding)?;
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
