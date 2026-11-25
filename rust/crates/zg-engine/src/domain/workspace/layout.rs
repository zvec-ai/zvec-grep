use std::path::{Path, PathBuf};

use crate::{
    EngineError, EngineResult,
    domain::ContentKind,
    utils::{directory_exists, file_exists},
};

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

/// Paths within one published, staged, or backup index directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexLayout<'a> {
    root: &'a Path,
}

impl<'a> IndexLayout<'a> {
    pub(crate) fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub(crate) fn root(self) -> &'a Path {
        self.root
    }

    pub(crate) fn files_dir(self) -> PathBuf {
        self.root.join("files")
    }

    pub(crate) fn directories_dir(self) -> PathBuf {
        self.root.join("directories")
    }

    pub(crate) fn entities_dir(self) -> PathBuf {
        self.root.join("entities")
    }

    pub(crate) fn file_ids_path(self) -> PathBuf {
        self.root.join("file-ids.json")
    }

    pub(crate) fn schema_path(self) -> PathBuf {
        self.root.join("schema.json")
    }

    pub(crate) fn lock_path(self) -> PathBuf {
        self.root.join("storage.lock")
    }

    pub(crate) fn optimize_marker_path(self) -> PathBuf {
        self.root.join("optimize.pending")
    }

    pub(crate) fn content_dir(self, kind: ContentKind) -> PathBuf {
        self.root.join(kind.as_str())
    }

    pub(crate) fn graph_dir(self) -> PathBuf {
        self.root.join("graph")
    }

    pub(crate) fn graph_db_path(self) -> PathBuf {
        self.graph_dir().join("graph.sqlite")
    }
}
