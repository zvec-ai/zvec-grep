use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    EngineError,
    api::{
        index::options::{Device, DiscoveryOptions, RootPath},
        info::result::{WorkspaceIndexEmbedding, WorkspaceIndexInfo, WorkspaceIndexPolicy},
    },
};

pub(crate) const WORKSPACE_MANIFEST_FILE: &str = "manifest.json";
pub(crate) const CURRENT_MANIFEST_VERSION: u32 = 1;
const WORKSPACE_DIRECTORY_MODE: u32 = 0o700;
const WORKSPACE_MANIFEST_MODE: u32 = 0o600;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingRuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub root_paths: Vec<ManifestRootPath>,
    pub index_policy: WorkspaceIndexPolicy,
    pub embedding: Option<WorkspaceIndexEmbedding>,
    pub index_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub created_time: u64,
    pub updated_time: u64,
    pub embedding_runtime: EmbeddingRuntimeConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "this persisted DTO mirrors main's root discovery schema"
)]
pub(crate) struct ManifestRootPath {
    pub absolute_path: PathBuf,
    pub recursive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insensitive_globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_file_types: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_ignore: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_files: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub follow: bool,
}

impl WorkspaceManifest {
    pub(crate) fn new(
        info: WorkspaceIndexInfo,
        embedding_runtime: EmbeddingRuntimeConfig,
    ) -> Result<Self, EngineError> {
        let manifest = Self {
            manifest_version: CURRENT_MANIFEST_VERSION,
            id: info.id,
            name: info.name,
            path: info.path,
            root_paths: info.roots.into_iter().map(Into::into).collect(),
            index_policy: info.policy,
            embedding: info.embedding,
            index_version: info.index_version,
            generation: info.generation,
            created_time: info.created_epoch_ms,
            updated_time: info.updated_epoch_ms,
            embedding_runtime,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn index_info(&self) -> WorkspaceIndexInfo {
        WorkspaceIndexInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            path: self.path.clone(),
            roots: self.root_paths.iter().cloned().map(Into::into).collect(),
            policy: self.index_policy,
            embedding: self.embedding.clone(),
            index_version: self.index_version,
            generation: self.generation,
            created_epoch_ms: self.created_time,
            updated_epoch_ms: self.updated_time,
        }
    }

    fn validate(&self) -> Result<(), EngineError> {
        if self.manifest_version != CURRENT_MANIFEST_VERSION {
            return Err(invalid_manifest(format!(
                "unsupported manifestVersion {}",
                self.manifest_version
            )));
        }
        if self.id.is_empty() || self.name.is_empty() || self.path.as_os_str().is_empty() {
            return Err(invalid_manifest("id, name, and path must be non-empty"));
        }
        if self.root_paths.is_empty()
            || self
                .root_paths
                .iter()
                .any(|root| root.absolute_path.as_os_str().is_empty())
        {
            return Err(invalid_manifest(
                "rootPaths must contain non-empty absolutePath values",
            ));
        }
        if self.index_policy == WorkspaceIndexPolicy::Undecided {
            return Err(invalid_manifest("indexPolicy must be enabled or disabled"));
        }
        if let Some(embedding) = &self.embedding
            && (embedding.provider.is_empty()
                || embedding.model.is_empty()
                || embedding.dimension == 0
                || !matches!(embedding.metric.as_str(), "cosine" | "dot" | "euclidean"))
        {
            return Err(invalid_manifest("embedding schema is invalid"));
        }
        Ok(())
    }
}

impl From<RootPath> for ManifestRootPath {
    fn from(root: RootPath) -> Self {
        let discovery = root.discovery;
        Self {
            absolute_path: root.path,
            recursive: root.recursive,
            include: discovery.include_paths,
            exclude: discovery.exclude_paths,
            globs: discovery.globs,
            insensitive_globs: discovery.insensitive_globs,
            file_types: discovery.file_types,
            excluded_file_types: discovery.excluded_file_types,
            hidden: discovery.hidden,
            no_ignore: discovery.no_ignore,
            ignore_files: discovery.ignore_files,
            max_depth: discovery.max_depth,
            max_file_size_bytes: discovery.max_file_size_bytes,
            follow: discovery.follow,
        }
    }
}

impl From<ManifestRootPath> for RootPath {
    fn from(root: ManifestRootPath) -> Self {
        Self {
            path: root.absolute_path,
            recursive: root.recursive,
            discovery: DiscoveryOptions {
                include_paths: root.include,
                exclude_paths: root.exclude,
                globs: root.globs,
                insensitive_globs: root.insensitive_globs,
                file_types: root.file_types,
                excluded_file_types: root.excluded_file_types,
                hidden: root.hidden,
                no_ignore: root.no_ignore,
                ignore_files: root.ignore_files,
                max_depth: root.max_depth,
                max_file_size_bytes: root.max_file_size_bytes,
                follow: root.follow,
            },
        }
    }
}

pub(crate) fn workspace_manifest_path(home: &Path) -> PathBuf {
    home.join(WORKSPACE_MANIFEST_FILE)
}

pub(crate) fn read_workspace_manifest(
    home: &Path,
) -> Result<Option<WorkspaceManifest>, EngineError> {
    let path = workspace_manifest_path(home);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(manifest_io("read", &path, &error)),
    };
    let manifest: WorkspaceManifest = serde_json::from_str(&text)
        .map_err(|error| invalid_manifest(format!("path={} cause={error}", path.display())))?;
    manifest.validate()?;
    Ok(Some(manifest))
}

pub(crate) fn write_workspace_manifest(
    home: &Path,
    manifest: &WorkspaceManifest,
) -> Result<(), EngineError> {
    manifest.validate()?;
    create_workspace_directory(home)?;
    let path = workspace_manifest_path(home);
    let temporary_path = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = write_manifest_file(&temporary_path, manifest).and_then(|()| {
        fs::rename(&temporary_path, &path).map_err(|error| manifest_io("rename", &path, &error))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub(crate) fn delete_workspace_manifest(home: &Path) -> Result<(), EngineError> {
    let path = workspace_manifest_path(home);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(manifest_io("delete", &path, &error)),
    }
}

fn create_workspace_directory(home: &Path) -> Result<(), EngineError> {
    fs::create_dir_all(home).map_err(|error| manifest_io("create directory", home, &error))?;
    set_mode(home, WORKSPACE_DIRECTORY_MODE)
        .map_err(|error| manifest_io("set directory permissions", home, &error))
}

fn write_manifest_file(path: &Path, manifest: &WorkspaceManifest) -> Result<(), EngineError> {
    let file = create_private_file(path).map_err(|error| manifest_io("create", path, &error))?;
    set_mode(path, WORKSPACE_MANIFEST_MODE)
        .map_err(|error| manifest_io("set file permissions", path, &error))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, manifest).map_err(|error| {
        EngineError::internal(format!("failed to encode workspace manifest: {error}"))
    })?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|error| manifest_io("write", path, &error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| manifest_io("sync", path, &error))
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(WORKSPACE_MANIFEST_MODE);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive references"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[track_caller]
fn invalid_manifest(message: impl Into<String>) -> EngineError {
    EngineError::storage_failure(format!("invalid workspace manifest: {}", message.into()))
}

#[track_caller]
fn manifest_io(operation: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!(
            "failed to {operation} workspace manifest {}",
            path.display()
        ),
        error,
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn fixture_manifest(home: &Path) -> WorkspaceManifest {
        WorkspaceManifest::new(
            WorkspaceIndexInfo {
                id: "workspace-id".to_owned(),
                name: "fixture".to_owned(),
                path: home.to_path_buf(),
                roots: vec![RootPath {
                    path: home.parent().expect("workspace root").to_path_buf(),
                    recursive: true,
                    discovery: DiscoveryOptions {
                        globs: vec!["*.rs".to_owned()],
                        hidden: true,
                        ..DiscoveryOptions::default()
                    },
                }],
                policy: WorkspaceIndexPolicy::Enabled,
                embedding: Some(WorkspaceIndexEmbedding {
                    provider: "local".to_owned(),
                    model: "minilm".to_owned(),
                    dimension: 384,
                    metric: "cosine".to_owned(),
                }),
                index_version: Some(1),
                generation: Some(7),
                created_epoch_ms: 10,
                updated_epoch_ms: 20,
            },
            EmbeddingRuntimeConfig {
                device: Some(Device::Cpu),
                cache_dir: Some(home.join("models")),
                ..EmbeddingRuntimeConfig::default()
            },
        )
        .expect("fixture manifest")
    }

    #[test]
    fn writes_and_reads_the_main_manifest_schema() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let manifest = fixture_manifest(&home);

        write_workspace_manifest(&home, &manifest).expect("write manifest");
        let text = fs::read_to_string(workspace_manifest_path(&home)).expect("manifest text");
        let json: serde_json::Value = serde_json::from_str(&text).expect("manifest json");

        assert_eq!(json["manifestVersion"], 1);
        assert_eq!(
            json["rootPaths"][0]["absolutePath"],
            directory.path().to_string_lossy().as_ref()
        );
        assert_eq!(json["rootPaths"][0]["globs"][0], "*.rs");
        assert_eq!(json["embeddingRuntime"]["device"], "cpu");
        assert_eq!(json["generation"], 7);
        assert_eq!(
            json["embeddingRuntime"]["cacheDir"],
            home.join("models").to_string_lossy().as_ref()
        );
        assert_eq!(manifest.index_info().generation, Some(7));
        assert_eq!(
            read_workspace_manifest(&home).expect("read manifest"),
            Some(manifest)
        );
        let mut legacy = json;
        legacy
            .as_object_mut()
            .expect("manifest object")
            .remove("generation");
        legacy["embeddingRuntime"]
            .as_object_mut()
            .expect("runtime object")
            .remove("cacheDir");
        let legacy: WorkspaceManifest = serde_json::from_value(legacy).expect("legacy manifest");
        assert_eq!(legacy.generation, None);
        assert_eq!(legacy.embedding_runtime.cache_dir, None);
    }

    #[test]
    fn rejects_invalid_or_unsupported_manifests() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(workspace_manifest_path(&home), r#"{"manifestVersion":2}"#)
            .expect("invalid manifest");

        assert!(read_workspace_manifest(&home).is_err());
    }

    #[test]
    fn deleting_a_missing_manifest_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        assert!(delete_workspace_manifest(directory.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_and_workspace_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        write_workspace_manifest(&home, &fixture_manifest(&home)).expect("write manifest");

        let directory_mode = fs::metadata(&home)
            .expect("workspace metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(workspace_manifest_path(&home))
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
