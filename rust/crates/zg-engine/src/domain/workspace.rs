use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{GlobRule, IndexState, SourcePath};

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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanRules {
    /// Ordered path rules relative to the workspace root.
    globs: Vec<GlobRule>,
    include_hidden: bool,
    follow_symlinks: bool,
    max_depth: Option<usize>,
    max_file_size_bytes: Option<u64>,
    use_default_ignores: bool,
    ignore_files: Vec<PathBuf>,
    /// Traverse child Git repositories, including submodules and worktrees.
    nested_git: bool,
}

impl ScanRules {
    /// # Errors
    /// Returns an error if the scan rules are invalid.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    pub fn new(
        globs: Vec<GlobRule>,
        include_hidden: bool,
        follow_symlinks: bool,
        max_depth: Option<usize>,
        max_file_size_bytes: Option<u64>,
        use_default_ignores: bool,
        ignore_files: Vec<PathBuf>,
        nested_git: bool,
    ) -> EngineResult<Self> {
        if max_file_size_bytes == Some(0) {
            return Err(EngineError::invalid_argument(
                "max_file_size_bytes must be greater than zero",
            ));
        }
        Ok(Self {
            globs,
            include_hidden,
            follow_symlinks,
            max_depth,
            max_file_size_bytes,
            use_default_ignores,
            ignore_files,
            nested_git,
        })
    }

    #[must_use]
    pub fn globs(&self) -> &[GlobRule] {
        &self.globs
    }

    #[must_use]
    pub fn include_hidden(&self) -> bool {
        self.include_hidden
    }

    #[must_use]
    pub fn follow_symlinks(&self) -> bool {
        self.follow_symlinks
    }

    #[must_use]
    pub fn max_depth(&self) -> Option<usize> {
        self.max_depth
    }

    #[must_use]
    pub fn max_file_size_bytes(&self) -> Option<u64> {
        self.max_file_size_bytes
    }

    /// Whether built-in exclusions and automatically discovered ignore files apply.
    #[must_use]
    pub fn use_default_ignores(&self) -> bool {
        self.use_default_ignores
    }

    #[must_use]
    pub fn ignore_files(&self) -> &[PathBuf] {
        &self.ignore_files
    }

    #[must_use]
    pub fn nested_git(&self) -> bool {
        self.nested_git
    }
}

impl Default for ScanRules {
    fn default() -> Self {
        Self {
            globs: Vec::new(),
            include_hidden: false,
            follow_symlinks: false,
            max_depth: None,
            max_file_size_bytes: None,
            use_default_ignores: true,
            ignore_files: Vec::new(),
            nested_git: true,
        }
    }
}

impl<'de> Deserialize<'de> for ScanRules {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        #[allow(clippy::struct_excessive_bools)]
        struct Fields {
            globs: Vec<GlobRule>,
            include_hidden: bool,
            follow_symlinks: bool,
            max_depth: Option<usize>,
            max_file_size_bytes: Option<u64>,
            #[serde(default = "enabled")]
            use_default_ignores: bool,
            ignore_files: Vec<PathBuf>,
            #[serde(default = "enabled")]
            nested_git: bool,
        }

        fn enabled() -> bool {
            true
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(
            fields.globs,
            fields.include_hidden,
            fields.follow_symlinks,
            fields.max_depth,
            fields.max_file_size_bytes,
            fields.use_default_ignores,
            fields.ignore_files,
            fields.nested_git,
        )
        .map_err(serde::de::Error::custom)
    }
}
