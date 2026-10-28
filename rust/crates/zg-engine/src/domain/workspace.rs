use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    utils::{current_username, directory_exists, file_exists, resolve_directory},
};

use super::{FileFormat, GlobRule, IndexState, SourcePath};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspace {
    source_root: SourceRoot,
    #[serde(skip)]
    layout: WorkspaceLayout,
    scan: ScanRules,
    /// Storage format version used by the index.
    index_version: Option<u32>,
    /// Identifies an index build for publication, recovery, and caching.
    storage_generation: Option<String>,
    #[serde(rename = "createdTime")]
    created_epoch_ms: u64,
    #[serde(rename = "updatedTime")]
    updated_epoch_ms: u64,
    index: IndexState,
}

impl Workspace {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source_root: &Path,
        layout: WorkspaceLayout,
        scan: ScanRules,
        index_version: Option<u32>,
        storage_generation: Option<String>,
        created_epoch_ms: u64,
        updated_epoch_ms: u64,
        index: IndexState,
    ) -> EngineResult<Self> {
        let source_root = SourceRoot::new(source_root)?;
        let workspace = Self {
            source_root,
            layout,
            scan,
            index_version,
            storage_generation,
            created_epoch_ms,
            updated_epoch_ms,
            index,
        };
        workspace.validate()?;
        Ok(workspace)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_manifest(
        source_root: SourceRoot,
        layout: WorkspaceLayout,
        scan: ScanRules,
        index_version: Option<u32>,
        storage_generation: Option<String>,
        created_epoch_ms: u64,
        updated_epoch_ms: u64,
        index: IndexState,
    ) -> EngineResult<Self> {
        let changed = source_root.environment_changed();
        let mut workspace = Self {
            source_root,
            layout,
            scan,
            index_version,
            storage_generation,
            created_epoch_ms,
            updated_epoch_ms,
            index,
        };
        workspace.validate()?;
        if changed {
            workspace.index.invalidate_runtime_settings();
        }
        Ok(workspace)
    }

    fn validate(&self) -> EngineResult<()> {
        if !self.source_root().is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace manifest '{}': source root has not been resolved",
                self.layout.manifest_path().display(),
            )));
        }
        if self.index_enabled() {
            if self.index_version.is_none() {
                return Err(EngineError::invalid_argument(format!(
                    "workspace {}: index is enabled but indexVersion is missing",
                    self.source_root().display(),
                )));
            }
            let generation = self.storage_generation.as_deref().ok_or_else(|| {
                EngineError::invalid_argument(format!(
                    "workspace {}: index is enabled but storageGeneration is missing",
                    self.source_root().display(),
                ))
            })?;
            uuid::Uuid::parse_str(generation).map_err(|_| {
                EngineError::invalid_argument(format!(
                    "workspace {}: storageGeneration must be a UUID, got {generation:?}",
                    self.source_root().display(),
                ))
            })?;
        } else if self.index_version.is_some() || self.storage_generation.is_some() {
            return Err(EngineError::invalid_argument(format!(
                "workspace {}: an unbuilt index cannot have indexVersion or storageGeneration",
                self.source_root().display(),
            )));
        }
        Ok(())
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root.current_path
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
        self.source_root().join(relative)
    }

    pub(crate) fn index_enabled(&self) -> bool {
        matches!(self.index, IndexState::Enabled(_))
    }

    pub(crate) fn record_update(&mut self, updated_epoch_ms: u64) {
        self.updated_epoch_ms = updated_epoch_ms;
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceRoot {
    /// Current canonical source directory used at runtime; not persisted.
    #[serde(skip)]
    current_path: PathBuf,
    /// Source path recorded when saved, used to detect environment changes.
    /// Never used to locate the workspace or its files.
    recorded_path: String,
    /// Username recorded when saved, used to detect environment changes.
    /// None when the username was unavailable.
    recorded_username: Option<String>,
}

impl SourceRoot {
    #[allow(clippy::unnecessary_debug_formatting)] // Keep control characters in paths escaped.
    fn new(source_root: &Path) -> EngineResult<Self> {
        if !source_root.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace source root {source_root:?} must be an absolute path",
            )));
        }
        let source_root = resolve_directory(Some(source_root))?;
        Ok(Self {
            recorded_path: source_root.to_string_lossy().into_owned(),
            current_path: source_root,
            recorded_username: current_username(),
        })
    }

    pub(crate) fn set_current_path(&mut self, path: &Path) -> EngineResult<()> {
        self.current_path = resolve_directory(Some(path))?;
        Ok(())
    }

    fn environment_changed(&self) -> bool {
        let username = current_username();
        self.recorded_path != self.current_path.to_string_lossy().as_ref()
            || username.is_none()
            || self.recorded_username != username
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceLayout {
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
    pub(crate) fn new(storage_dir: PathBuf) -> EngineResult<Self> {
        if !storage_dir.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace storage directory {storage_dir:?} must be an absolute path",
            )));
        }
        directory_exists(&storage_dir)?;
        let temp_dir = storage_dir.join("temp");
        let locks_dir = storage_dir.join("locks");
        Ok(Self {
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
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
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
    ignore_files: Vec<IgnorePath>,
    /// Traverse child Git repositories, including submodules and worktrees.
    nested_git: bool,
}

impl ScanRules {
    /// # Errors
    /// Returns an error if the rules are invalid.
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
        let ignore_files = ignore_files
            .into_iter()
            .map(IgnorePath::from_path)
            .collect::<EngineResult<_>>()?;
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

    pub fn ignore_files(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.ignore_files.iter().map(IgnorePath::as_path)
    }

    #[must_use]
    pub fn nested_git(&self) -> bool {
        self.nested_git
    }

    pub(crate) fn inherit_ignore_files(&mut self, current: &Self) {
        self.ignore_files.clone_from(&current.ignore_files);
    }

    pub(crate) fn resolve_ignore_files(&self, root: &Path) -> Vec<PathBuf> {
        self.ignore_files
            .iter()
            .filter_map(|file| match file {
                IgnorePath::Workspace { path } => Some(root.join(path.as_path())),
                IgnorePath::External { path } => path.is_absolute().then(|| path.clone()),
            })
            .collect()
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
enum IgnorePath {
    Workspace { path: SourcePath },
    External { path: PathBuf },
}

impl IgnorePath {
    fn from_path(path: PathBuf) -> EngineResult<Self> {
        if !path.is_absolute() {
            return Ok(Self::Workspace {
                path: SourcePath::new(path)?,
            });
        }
        if path.to_str().is_none() {
            return Err(EngineError::invalid_argument(
                "external ignore file path must be Unicode to serialize it",
            ));
        }
        Ok(Self::External { path })
    }

    fn as_path(&self) -> &Path {
        match self {
            Self::Workspace { path } => path.as_path(),
            Self::External { path } => path,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use serde_json::Value;

    use super::{ScanRules, SourceRoot, Workspace, WorkspaceLayout};
    use crate::domain::{
        ContentKind, FTS_CONFIG, FileFormat, IndexDescriptor, IndexState, SourcePath,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    };

    fn workspace(root: &Path) -> Workspace {
        let scan = ScanRules::new(
            vec!["*.rs".into()],
            vec![FileFormat::Rust],
            vec![FileFormat::Cpp],
            true,
            true,
            Some(3),
            Some(1024),
            false,
            vec!["config/rules.ignore".into()],
            false,
        )
        .expect("scan rules");
        let model =
            ModelInfo::new("local", "test-model", [ContentKind::Code], 1).expect("model metadata");
        let embedding = EmbeddingModelInfo::new(model, 8, EmbeddingMetric::Cosine, None, None)
            .expect("embedding metadata");
        let index = IndexDescriptor::new(embedding, BTreeMap::new(), BTreeMap::new(), FTS_CONFIG)
            .expect("index descriptor");
        Workspace::new(
            root,
            WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout"),
            scan,
            Some(2),
            Some(uuid::Uuid::new_v4().to_string()),
            10,
            20,
            IndexState::Enabled(Box::new(index)),
        )
        .expect("workspace")
    }

    fn restore_workspace(saved: &Value, root: &Path) -> Workspace {
        let mut source: SourceRoot =
            serde_json::from_value(saved["sourceRoot"].clone()).expect("source root fields");
        source.set_current_path(root).expect("current source path");
        Workspace::from_manifest(
            source,
            WorkspaceLayout::new(root.join(".zvec-grep")).expect("current layout"),
            serde_json::from_value(saved["scan"].clone()).expect("scan fields"),
            serde_json::from_value(saved["indexVersion"].clone()).expect("index version"),
            serde_json::from_value(saved["storageGeneration"].clone()).expect("storage generation"),
            serde_json::from_value(saved["createdTime"].clone()).expect("created time"),
            serde_json::from_value(saved["updatedTime"].clone()).expect("updated time"),
            serde_json::from_value(saved["index"].clone()).expect("index fields"),
        )
        .expect("workspace from manifest")
    }

    #[test]
    fn workspace_round_trips_through_manifest() {
        let directory = tempfile::tempdir().expect("workspace directory");
        let root = directory.path().canonicalize().expect("source root");
        let workspace = workspace(&root);
        assert_eq!(workspace.source_root(), root);
        assert_eq!(workspace.layout().storage_dir(), root.join(".zvec-grep"));
        assert!(workspace.index_enabled());

        let json = serde_json::to_string(&workspace).expect("serialize workspace");
        let saved: Value = serde_json::from_str(&json).expect("deserialize manifest");
        assert!(saved.get("layout").is_none());
        assert!(saved["sourceRoot"].get("currentPath").is_none());
        assert_eq!(
            saved["sourceRoot"]["recordedPath"],
            root.to_string_lossy().as_ref()
        );
        assert_eq!(restore_workspace(&saved, &root), workspace);
    }

    #[test]
    fn workspace_restores_from_manifest_after_move() {
        let directory = tempfile::tempdir().expect("workspace directories");
        let original = directory.path().join("original");
        fs::create_dir_all(original.join(".zvec-grep")).expect("storage directory");
        fs::create_dir(original.join("config")).expect("configuration directory");
        fs::write(original.join("config/rules.ignore"), "*.tmp\n").expect("ignore file");
        fs::write(original.join("main.rs"), "fn main() {}\n").expect("source file");
        let original = original.canonicalize().expect("original source root");
        let workspace = workspace(&original);
        let saved = serde_json::to_value(&workspace).expect("serialize workspace");

        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move source and storage");
        let moved = moved.canonicalize().expect("moved source root");
        let restored = restore_workspace(&saved, &moved);
        assert_eq!(restored.source_root(), moved);
        assert_eq!(restored.layout().storage_dir(), moved.join(".zvec-grep"));
        assert!(restored.layout().storage_dir().is_dir());
        assert_eq!(
            serde_json::to_value(&restored).expect("saved metadata"),
            saved
        );
        assert_eq!(
            fs::read_to_string(
                restored.source_path(&SourcePath::new("main.rs").expect("source path"))
            )
            .expect("read moved source"),
            "fn main() {}\n"
        );
        let ignore_files = restored.scan().resolve_ignore_files(restored.source_root());
        assert_eq!(ignore_files, [moved.join("config/rules.ignore")]);
        assert_eq!(
            fs::read_to_string(&ignore_files[0]).expect("read moved rules"),
            "*.tmp\n"
        );
    }
}
