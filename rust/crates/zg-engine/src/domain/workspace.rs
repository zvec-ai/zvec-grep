use std::{
    fs,
    path::{Path, PathBuf},
};

use relative_path::{PathExt, RelativePathBuf};
use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    storage::PathRecord,
    utils::{directory_exists, file_exists},
};

use super::{FileFormat, GlobRule, IndexState, SourcePath};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspace {
    #[serde(skip)]
    layout: WorkspaceLayout,
    source_root: SourceRoot,
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
        mut scan: ScanRules,
        index_version: Option<u32>,
        storage_generation: Option<String>,
        created_epoch_ms: u64,
        updated_epoch_ms: u64,
        index: IndexState,
    ) -> EngineResult<Self> {
        let source_root = SourceRoot::new(source_root, &layout)?;
        scan.normalize_ignore_paths(&source_root.resolved)?;
        let workspace = Self {
            layout,
            source_root,
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
        layout: WorkspaceLayout,
        source_root: SourceRoot,
        scan: ScanRules,
        index_version: Option<u32>,
        storage_generation: Option<String>,
        created_epoch_ms: u64,
        updated_epoch_ms: u64,
        index: IndexState,
    ) -> EngineResult<Self> {
        let changed = source_root.environment_changed();
        let mut workspace = Self {
            layout,
            source_root,
            scan,
            index_version,
            storage_generation,
            created_epoch_ms,
            updated_epoch_ms,
            index,
        };
        workspace.validate()?;
        if changed {
            workspace.scan.invalidate_external_ignores();
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
            if self.storage_generation.is_none() {
                return Err(EngineError::invalid_argument(format!(
                    "workspace {}: index is enabled but storageGeneration is missing",
                    self.source_root().display(),
                )));
            }
        } else if self.index_version.is_some() || self.storage_generation.is_some() {
            return Err(EngineError::invalid_argument(format!(
                "workspace {}: an unbuilt index cannot have indexVersion or storageGeneration",
                self.source_root().display(),
            )));
        }
        if let Some(generation) = &self.storage_generation {
            validate_generation(self.source_root(), generation)?;
        }
        Ok(())
    }

    pub(crate) fn layout(&self) -> &WorkspaceLayout {
        &self.layout
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root.resolved
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
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRoot {
    #[serde(skip)]
    resolved: PathBuf,
    relative: RelativePathBuf,
    absolute: String,
    username: Option<String>,
}

impl SourceRoot {
    #[allow(clippy::unnecessary_debug_formatting)] // Keep control characters in paths escaped.
    fn new(source_root: &Path, layout: &WorkspaceLayout) -> EngineResult<Self> {
        if !source_root.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace source root {source_root:?} must be an absolute path",
            )));
        }
        let source_root = crate::utils::resolve_directory(Some(source_root))?;
        let storage_dir = match (
            layout.storage_dir().parent(),
            layout.storage_dir().file_name(),
        ) {
            (Some(parent), Some(name)) => crate::utils::resolve_directory(Some(parent))?.join(name),
            _ => crate::utils::resolve_directory(Some(layout.storage_dir()))?,
        };
        let relative = source_root
            .relative_to(&storage_dir)
            .map_err(|error| {
                EngineError::invalid_argument(format!(
                    "cannot express workspace source root relative to its storage directory: {error}",
                ))
            })?
            .normalize();
        Ok(Self {
            absolute: source_root.to_string_lossy().into_owned(),
            resolved: source_root,
            relative,
            username: current_username(),
        })
    }

    pub(crate) fn resolve(
        &mut self,
        layout: &WorkspaceLayout,
        expected_source_root: &Path,
    ) -> EngineResult<()> {
        let value = self.relative.as_str();
        if value.is_empty() || value.starts_with('/') || value.contains(['\\', ':', '\0']) {
            return Err(EngineError::invalid_argument(format!(
                "workspace manifest '{}': sourceRoot.relative must be a relative path using '/' separators, got {value:?}",
                layout.manifest_path().display(),
            )));
        }
        let resolved = crate::utils::resolve_directory(Some(
            &self.relative.to_logical_path(layout.storage_dir()),
        ))
        .map_err(|error| {
            EngineError::invalid_argument(format!(
                "workspace manifest '{}': cannot resolve sourceRoot.relative {value:?}: {error}",
                layout.manifest_path().display(),
            ))
        })?;
        let expected = crate::utils::resolve_directory(Some(expected_source_root))?;
        if resolved != expected {
            return Err(EngineError::invalid_argument(format!(
                "workspace manifest '{}': sourceRoot.relative {value:?} resolves to '{}', expected source directory '{}'",
                layout.manifest_path().display(),
                resolved.display(),
                expected.display(),
            )));
        }
        self.resolved = resolved;
        Ok(())
    }

    fn environment_changed(&self) -> bool {
        let username = current_username();
        self.absolute != self.resolved.to_string_lossy().as_ref()
            || username.is_none()
            || self.username != username
    }
}

fn current_username() -> Option<String> {
    let names = if cfg!(windows) {
        ["USERNAME", "USER"]
    } else {
        ["USER", "USERNAME"]
    };
    names.into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
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
    #[serde(deserialize_with = "deserialize_max_file_size")]
    max_file_size_bytes: Option<u64>,
    use_default_ignores: bool,
    ignore_files: Vec<StoredIgnorePath>,
    #[serde(skip)]
    invalidated_external_ignores: bool,
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
        let max_file_size_bytes = validate_max_file_size(max_file_size_bytes)?;
        let ignore_files = ignore_files
            .into_iter()
            .map(StoredIgnorePath::from_path)
            .collect();
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
            invalidated_external_ignores: false,
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
        self.ignore_files.iter().map(StoredIgnorePath::as_path)
    }

    #[must_use]
    pub fn nested_git(&self) -> bool {
        self.nested_git
    }

    pub(crate) fn normalize_ignore_paths(&mut self, root: &Path) -> EngineResult<()> {
        if self.ignore_files.is_empty() {
            return Ok(());
        }
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| EngineError::from_io("resolve workspace root", &error))?;
        self.ignore_files = self
            .resolve_ignore_files(root)?
            .into_iter()
            .map(|path| {
                let relative = path
                    .strip_prefix(root)
                    .or_else(|_| path.strip_prefix(&canonical_root))
                    .ok();
                if let Some(relative) = relative
                    && PathRecord::from_path(relative).is_ok()
                {
                    return Ok(StoredIgnorePath::from_path(relative.to_path_buf()));
                }
                let file_name = path.file_name().ok_or_else(|| {
                    EngineError::invalid_argument(format!(
                        "ignore file path must name a file: {}",
                        path.display(),
                    ))
                })?;
                // Keep the selected file alias so replacing its symlink remains observable.
                let absolute =
                    match fs::canonicalize(path.parent().expect("absolute ignore file parent")) {
                        Ok(parent) => parent.join(file_name),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
                        Err(error) => {
                            return Err(EngineError::from_io("resolve ignore file path", &error));
                        }
                    };
                let path = absolute
                    .strip_prefix(&canonical_root)
                    .map_or_else(|_| absolute.clone(), Path::to_path_buf);
                if path.is_absolute() && path.to_str().is_none() {
                    return Err(EngineError::invalid_argument(
                        "external ignore file path must be Unicode to serialize it",
                    ));
                }
                Ok(StoredIgnorePath::from_path(path))
            })
            .collect::<EngineResult<_>>()?;
        Ok(())
    }

    pub(crate) fn invalidate_external_ignores(&mut self) {
        self.invalidated_external_ignores = self
            .ignore_files
            .iter()
            .any(|file| matches!(file, StoredIgnorePath::External { .. }));
    }

    pub(crate) fn inherit_ignore_files(&mut self, current: &Self) {
        self.ignore_files.clone_from(&current.ignore_files);
        self.invalidated_external_ignores = current.invalidated_external_ignores;
    }

    pub(crate) fn resolve_ignore_files(&self, root: &Path) -> EngineResult<Vec<PathBuf>> {
        self.ignore_files
            .iter()
            .map(|file| {
                if let StoredIgnorePath::External { platform, path } = file
                    && (self.invalidated_external_ignores || platform != path_platform())
                {
                    return Err(EngineError::invalid_argument(format!(
                        "saved external ignore file {path:?} needs to be configured again for workspace {}; use --ignore-file or --reset-paths",
                        root.display(),
                    )));
                }
                Ok(root.join(file.as_path()))
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
            invalidated_external_ignores: false,
            nested_git: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
enum StoredIgnorePath {
    Workspace {
        #[serde(with = "relative_ignore_path")]
        path: PathBuf,
    },
    External {
        platform: String,
        path: PathBuf,
    },
}

impl StoredIgnorePath {
    fn from_path(path: PathBuf) -> Self {
        if PathRecord::from_path(&path).is_ok() {
            return Self::Workspace { path };
        }
        Self::External {
            platform: path_platform().into(),
            path,
        }
    }

    fn as_path(&self) -> &Path {
        match self {
            Self::Workspace { path } | Self::External { path, .. } => path,
        }
    }
}

fn path_platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}

mod relative_ignore_path {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    use crate::storage::PathRecord;

    pub(super) fn serialize<S: serde::Serializer>(
        path: &Path,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        PathRecord::from_path(path)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        PathRecord::deserialize(deserializer)?
            .into_path()
            .map_err(serde::de::Error::custom)
    }
}

fn validate_max_file_size(value: Option<u64>) -> EngineResult<Option<u64>> {
    if value == Some(0) {
        return Err(EngineError::invalid_argument(
            "max_file_size_bytes must be greater than zero",
        ));
    }
    Ok(value)
}

fn deserialize_max_file_size<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    validate_max_file_size(Option::<u64>::deserialize(deserializer)?)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ScanRules;
    use crate::api::index::options::ScanRulesUpdate;

    #[test]
    fn source_root_remains_relative_after_move() {
        use std::fs;

        use super::{SourceRoot, WorkspaceLayout};

        let directory = tempfile::tempdir().expect("directory");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("source directory");
        let source_path = fs::canonicalize(&original).expect("source path");
        #[cfg(unix)]
        let source_path = {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&source_path, &alias).expect("source alias");
            alias
        };
        let layout = WorkspaceLayout::new(source_path.join(".zvec-grep")).expect("layout");
        let source = SourceRoot::new(&source_path, &layout).expect("source root");
        fs::create_dir(layout.storage_dir()).expect("storage directory");

        let mut loaded: SourceRoot =
            serde_json::from_value(serde_json::to_value(source).expect("serialize"))
                .expect("deserialize");
        loaded
            .resolve(&layout, &source_path)
            .expect("resolve source root");
        let rebuilt = SourceRoot::new(&loaded.resolved, &layout).expect("rebuild source root");
        assert_eq!(rebuilt.relative.as_str(), "..");
        let saved = serde_json::to_value(rebuilt).expect("serialize rebuilt source root");
        assert!(saved.get("resolved").is_none());

        let moved = directory.path().join("moved");
        fs::rename(original, &moved).expect("move source and storage");
        let moved = fs::canonicalize(moved).expect("moved source");
        let moved_layout = WorkspaceLayout::new(moved.join(".zvec-grep")).expect("moved layout");
        let mut reloaded: SourceRoot =
            serde_json::from_value(saved).expect("read moved source root");
        reloaded
            .resolve(&moved_layout, &moved)
            .expect("resolve moved source root");
        assert_eq!(reloaded.resolved, moved);
    }

    #[test]
    fn external_paths_are_checked_only_when_used() {
        let foreign = if cfg!(windows) { "unix" } else { "windows" };
        let json = format!(
            r#"{{"ignore_files":[{{"location":"external","platform":"{foreign}","path":"foreign.ignore"}}]}}"#,
        );
        let scan: ScanRules = serde_json::from_str(&json).expect("read saved path");
        assert!(scan.resolve_ignore_files(Path::new("/workspace")).is_err());
        let replacement = ScanRulesUpdate {
            ignore_files: Some(vec!["local.ignore".into()]),
            ..Default::default()
        }
        .apply(&scan)
        .expect("replace saved path");
        assert!(
            replacement
                .resolve_ignore_files(Path::new("/workspace"))
                .is_ok()
        );
    }

    #[test]
    fn unrelated_updates_preserve_invalidated_paths() {
        let mut scan = ScanRules::new(
            vec![],
            vec![],
            vec![],
            false,
            false,
            None,
            None,
            true,
            vec![std::path::absolute("external.ignore").expect("absolute path")],
            true,
        )
        .expect("scan rules");
        scan.invalidate_external_ignores();
        let updated = ScanRulesUpdate {
            include_hidden: Some(true),
            ..Default::default()
        }
        .apply(&scan)
        .expect("update hidden files");
        assert!(
            updated
                .resolve_ignore_files(Path::new("/workspace"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_ignore_paths_preserve_file_aliases() {
        use std::{fs, os::unix::fs::symlink};
        let directory = tempfile::tempdir().expect("source");
        let root = fs::canonicalize(directory.path()).expect("root");
        let first = root.join("first.ignore");
        let second = root.join("second.ignore");
        fs::write(&first, "first\n").expect("first rules");
        fs::write(&second, "second\n").expect("second rules");
        let alias = root.join("rules.ignore");
        symlink(&first, &alias).expect("file alias");
        let mut scan = ScanRules::new(
            vec![],
            vec![],
            vec![],
            false,
            false,
            None,
            None,
            true,
            vec![alias.clone()],
            true,
        )
        .expect("scan rules");
        scan.normalize_ignore_paths(&root).expect("relative path");
        let saved = serde_json::to_string(&scan).expect("save path");
        let restored: ScanRules = serde_json::from_str(&saved).expect("read path");
        fs::remove_file(&alias).expect("remove alias");
        symlink(&second, &alias).expect("replace alias");
        let paths = restored
            .resolve_ignore_files(&root)
            .expect("resolve relative path");
        assert_eq!(
            fs::read_to_string(&paths[0]).expect("read selected alias"),
            "second\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_ignore_paths_preserve_non_unicode_names() {
        use std::{ffi::OsStr, fs, os::unix::ffi::OsStrExt};

        let directory = tempfile::tempdir().expect("source");
        let root = fs::canonicalize(directory.path()).expect("root");
        let relative = Path::new("config").join(OsStr::from_bytes(b"rules-\xff.ignore"));
        let mut scan = ScanRulesUpdate {
            ignore_files: Some(vec![root.join(&relative)]),
            ..Default::default()
        }
        .apply(&ScanRules::default())
        .expect("scan rules");
        scan.normalize_ignore_paths(&root).expect("relative path");
        assert_eq!(scan.ignore_files().next(), Some(relative.as_path()));
        let saved = serde_json::to_string(&scan).expect("save native path");
        let restored: ScanRules = serde_json::from_str(&saved).expect("read native path");
        assert_eq!(restored, scan);
    }
}
