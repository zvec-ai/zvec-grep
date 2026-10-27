use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    utils::{directory_exists, file_exists},
};

use super::{FileFormat, GlobRule, IndexState, SourcePath};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    layout: WorkspaceLayout,
    scan: ScanRules,
    /// Storage format version used by the index.
    index_version: Option<u32>,
    /// Identifies an index build for publication, recovery, and caching.
    storage_generation: Option<String>,
    created_epoch_ms: u64,
    updated_epoch_ms: u64,
    index: IndexState,
}

impl Workspace {
    pub(crate) fn new(
        layout: WorkspaceLayout,
        scan: ScanRules,
        index_version: Option<u32>,
        storage_generation: Option<String>,
        created_epoch_ms: u64,
        updated_epoch_ms: u64,
        index: IndexState,
    ) -> EngineResult<Self> {
        let workspace = Self {
            layout,
            scan,
            index_version,
            storage_generation,
            created_epoch_ms,
            updated_epoch_ms,
            index,
        };
        if workspace.index_enabled() {
            if workspace.index_version.is_none() {
                return Err(EngineError::invalid_argument(format!(
                    "workspace {}: index is enabled but indexVersion is missing",
                    workspace.layout.source_root().display(),
                )));
            }
            if workspace.storage_generation.is_none() {
                return Err(EngineError::invalid_argument(format!(
                    "workspace {}: index is enabled but storageGeneration is missing",
                    workspace.layout.source_root().display(),
                )));
            }
        } else if workspace.index_version.is_some() || workspace.storage_generation.is_some() {
            return Err(EngineError::invalid_argument(format!(
                "workspace {}: an unbuilt index cannot have indexVersion or storageGeneration",
                workspace.layout.source_root().display(),
            )));
        }
        if let Some(generation) = &workspace.storage_generation {
            validate_generation(workspace.layout.source_root(), generation)?;
        }
        Ok(workspace)
    }

    pub(crate) fn layout(&self) -> &WorkspaceLayout {
        &self.layout
    }

    pub(crate) fn scan(&self) -> &ScanRules {
        &self.scan
    }

    pub(crate) fn index_version(&self) -> Option<u32> {
        self.index_version
    }

    pub(crate) fn storage_generation(&self) -> Option<&str> {
        self.storage_generation.as_deref()
    }

    pub(crate) fn created_epoch_ms(&self) -> u64 {
        self.created_epoch_ms
    }

    pub(crate) fn updated_epoch_ms(&self) -> u64 {
        self.updated_epoch_ms
    }

    pub(crate) fn index(&self) -> &IndexState {
        &self.index
    }

    pub(crate) fn source_path(&self, relative: &SourcePath) -> PathBuf {
        self.layout.source_root().join(relative)
    }

    pub(crate) fn index_enabled(&self) -> bool {
        matches!(self.index, IndexState::Enabled(_))
    }

    pub(crate) fn record_update(&mut self, updated_epoch_ms: u64) {
        self.updated_epoch_ms = updated_epoch_ms;
    }
}

fn validate_generation(source_root: &Path, generation: &str) -> EngineResult<()> {
    uuid::Uuid::parse_str(generation).map(|_| ()).map_err(|_| {
        EngineError::invalid_argument(format!(
            "workspace {}: storageGeneration must be a UUID, got {generation:?}",
            source_root.display(),
        ))
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceLayout {
    /// Source tree root for scanning and relative file paths.
    source_root: PathBuf,
    /// Directory containing workspace data.
    storage_dir: PathBuf,
    /// Saved workspace settings and index metadata.
    manifest_path: PathBuf,
    /// Currently published index.
    index_dir: PathBuf,
    /// Staging and recovery data for index builds.
    temp_dir: PathBuf,
    /// Build state used to recover interrupted publication.
    build_record_path: PathBuf,
    /// Manifest prepared for the new index.
    staged_manifest_path: PathBuf,
    /// Index being built before publication.
    staged_index_dir: PathBuf,
    /// Previous index kept for rollback.
    backup_index_dir: PathBuf,
    /// Lock files coordinating index access.
    locks_dir: PathBuf,
    /// Coordinates shared reads and exclusive writes.
    workspace_lock_path: PathBuf,
    /// Blocks new readers while writers are waiting or active.
    write_intent_lock_path: PathBuf,
    /// Ensures cached index handles are closed before writes.
    read_cache_lock_path: PathBuf,
    /// Encryption key for saved model API keys.
    credentials_key_path: PathBuf,
    /// Saved remote embedding authorization grants.
    authorization_path: PathBuf,
    /// Serializes updates to authorization grants.
    authorization_lock_path: PathBuf,
}

impl WorkspaceLayout {
    #[allow(clippy::unnecessary_debug_formatting)] // Keep control characters in paths escaped.
    pub(crate) fn new(source_root: PathBuf) -> EngineResult<Self> {
        if !source_root.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace source root {source_root:?} must be an absolute path",
            )));
        }
        let storage_dir = source_root.join(".zvec-grep");
        directory_exists(&storage_dir)?;
        let temp_dir = storage_dir.join("temp");
        let locks_dir = storage_dir.join("locks");
        Ok(Self {
            source_root,
            manifest_path: storage_dir.join("manifest.json"),
            index_dir: storage_dir.join("index"),
            build_record_path: temp_dir.join("build.json"),
            staged_manifest_path: temp_dir.join("manifest.json"),
            staged_index_dir: temp_dir.join("new"),
            backup_index_dir: temp_dir.join("old"),
            workspace_lock_path: locks_dir.join("home"),
            write_intent_lock_path: locks_dir.join("write-intent"),
            read_cache_lock_path: locks_dir.join("read-cache"),
            credentials_key_path: storage_dir.join("credentials.key"),
            authorization_path: storage_dir.join("authorization.json"),
            authorization_lock_path: storage_dir.join("authorization.lock"),
            storage_dir,
            temp_dir,
            locks_dir,
        })
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub(crate) fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    pub(crate) fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub(crate) fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    pub(crate) fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    pub(crate) fn build_record_path(&self) -> &Path {
        &self.build_record_path
    }

    pub(crate) fn staged_manifest_path(&self) -> &Path {
        &self.staged_manifest_path
    }

    pub(crate) fn staged_index_dir(&self) -> &Path {
        &self.staged_index_dir
    }

    pub(crate) fn backup_index_dir(&self) -> &Path {
        &self.backup_index_dir
    }

    pub(crate) fn locks_dir(&self) -> &Path {
        &self.locks_dir
    }

    pub(crate) fn workspace_lock_path(&self) -> &Path {
        &self.workspace_lock_path
    }

    pub(crate) fn write_intent_lock_path(&self) -> &Path {
        &self.write_intent_lock_path
    }

    pub(crate) fn read_cache_lock_path(&self) -> &Path {
        &self.read_cache_lock_path
    }

    pub(crate) fn credentials_key_path(&self) -> &Path {
        &self.credentials_key_path
    }

    pub(crate) fn authorization_path(&self) -> &Path {
        &self.authorization_path
    }

    pub(crate) fn authorization_lock_path(&self) -> &Path {
        &self.authorization_lock_path
    }

    pub(crate) fn has_manifest(&self) -> EngineResult<bool> {
        file_exists(self.manifest_path())
    }

    pub(crate) fn has_build_record(&self) -> EngineResult<bool> {
        Ok(directory_exists(self.temp_dir())? && file_exists(self.build_record_path())?)
    }

    pub(crate) fn has_index_data(&self) -> EngineResult<bool> {
        let mut found = file_exists(self.manifest_path())?;
        found |= directory_exists(self.index_dir())?;
        if directory_exists(self.temp_dir())? {
            for path in [self.build_record_path(), self.staged_manifest_path()] {
                found |= file_exists(path)?;
            }
            for path in [self.staged_index_dir(), self.backup_index_dir()] {
                found |= directory_exists(path)?;
            }
        }
        Ok(found)
    }
}

/// Persistent rules for discovering and admitting workspace files to the index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanRules {
    globs: Vec<GlobRule>,
    included_formats: Vec<FileFormat>,
    excluded_formats: Vec<FileFormat>,
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
        included_formats: Vec<FileFormat>,
        excluded_formats: Vec<FileFormat>,
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
            included_formats,
            excluded_formats,
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
    pub fn included_formats(&self) -> &[FileFormat] {
        &self.included_formats
    }

    #[must_use]
    pub fn excluded_formats(&self) -> &[FileFormat] {
        &self.excluded_formats
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
            included_formats: Vec::new(),
            excluded_formats: Vec::new(),
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
            included_formats: Vec<FileFormat>,
            excluded_formats: Vec<FileFormat>,
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
            fields.included_formats,
            fields.excluded_formats,
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
