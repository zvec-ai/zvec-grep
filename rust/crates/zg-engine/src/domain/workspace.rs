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

/// Resolved model configuration; effective retrieval tables are derived by content kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    pub default_model: EmbeddingModelInfo,
    pub routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
    pub fts: FtsConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IndexTable {
    pub kind: ContentKind,
    pub embedding: EmbeddingModelInfo,
}

impl IndexDescriptor {
    #[cfg(test)]
    pub(crate) fn single(embedding: EmbeddingModelInfo) -> Self {
        Self {
            default_model: embedding,
            routes: BTreeMap::new(),
            fts: FTS_CONFIG,
        }
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        self.default_model.validate()?;
        for (kind, embedding) in &self.routes {
            embedding.validate()?;
            self.model_for(*kind)?;
            for other in std::iter::once(&self.default_model).chain(self.routes.values()) {
                if embedding.model.reference() == other.model.reference() && embedding != other {
                    return Err(EngineError::invalid_argument(format!(
                        "conflicting configurations for embedding model {}",
                        embedding.model.reference(),
                    )));
                }
            }
        }
        Ok(())
    }

    /// An unsupported default yields no table; an invalid explicit route is an error.
    pub(crate) fn model_for(&self, kind: ContentKind) -> EngineResult<Option<&EmbeddingModelInfo>> {
        if let Some(embedding) = self.routes.get(&kind) {
            if !embedding.model.supports_content(kind) {
                return Err(EngineError::invalid_argument(format!(
                    "embedding route for {} refers to model {}, which does not support this content kind",
                    kind.as_str(),
                    embedding.model.reference(),
                )));
            }
            return Ok(Some(embedding));
        }
        Ok(self
            .default_model
            .model
            .supports_content(kind)
            .then_some(&self.default_model))
    }

    pub(crate) fn tables(&self) -> EngineResult<Vec<IndexTable>> {
        self.validate()?;
        let mut tables = Vec::new();
        for kind in [ContentKind::Text, ContentKind::Code, ContentKind::Image] {
            if let Some(embedding) = self.model_for(kind)? {
                tables.push(IndexTable {
                    kind,
                    embedding: embedding.clone(),
                });
            }
        }
        Ok(tables)
    }

    /// One runtime per distinct model, even when several kinds share it.
    pub(crate) fn embeddings(&self) -> Vec<&EmbeddingModelInfo> {
        let mut models = BTreeMap::new();
        for embedding in std::iter::once(&self.default_model).chain(self.routes.values()) {
            models
                .entry(embedding.model.reference())
                .or_insert(embedding);
        }
        models.into_values().collect()
    }

    pub(crate) fn ensure_index_compatible(&self, other: &Self) -> EngineResult<()> {
        let tables = self.tables()?;
        let other_tables = other.tables()?;
        if self.fts != other.fts || tables.len() != other_tables.len() {
            return Err(EngineError::invalid_argument(
                "existing index uses different content kinds or FTS configuration; rebuild the index",
            ));
        }
        for (table, other_table) in tables.iter().zip(&other_tables) {
            if table.kind != other_table.kind {
                return Err(EngineError::invalid_argument(
                    "existing index uses different content kinds; rebuild the index",
                ));
            }
            table
                .embedding
                .ensure_index_compatible(&other_table.embedding)?;
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
