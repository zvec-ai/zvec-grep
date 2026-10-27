#[cfg(test)]
use crate::domain::model::EmbeddingMetric;
use crate::{
    EngineError,
    domain::{
        ContentKind, IndexDescriptor, IndexState, ScanRules, Workspace, WorkspaceLayout,
        model::{EmbeddingModelInfo, EncryptedApiKey, ModelConfig},
    },
    utils::{atomic_write, ensure_directory, reject_symlink, sync_directory},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

// Serialized policy is a disk concern; domain enabled state always has a descriptor.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum IndexPolicy {
    Uninitialized,
    Enabled,
    Disabled,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestData {
    format: String,
    source_root: String,
    #[serde(default)]
    scan: StoredScanRules,
    index_policy: IndexPolicy,
    #[serde(default)]
    default_embedding_model: Option<EmbeddingModelInfo>,
    #[serde(default)]
    embedding_routes: BTreeMap<ContentKind, EmbeddingModelInfo>,
    index_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage_generation: Option<String>,
    created_time: u64,
    updated_time: u64,
    #[serde(default)]
    embedding_runtimes: BTreeMap<String, ModelConfig<EncryptedApiKey>>,
}

impl ManifestData {
    fn into_workspace(
        self,
        layout: &WorkspaceLayout,
        ignore_files: Option<&[PathBuf]>,
    ) -> Result<Workspace, String> {
        if self.format != "zg-workspace" {
            return Err("unsupported workspace manifest format".into());
        }
        if self.source_root != ".." {
            return Err(
                "sourceRoot must point to the parent of the default workspace directory".into(),
            );
        }
        if !matches!(self.index_policy, IndexPolicy::Enabled) && !self.embedding_runtimes.is_empty()
        {
            return Err("an unbuilt workspace cannot contain embedding configurations".into());
        }
        let index = match self.index_policy {
            IndexPolicy::Disabled => IndexState::Disabled,
            IndexPolicy::Uninitialized => IndexState::Uninitialized,
            IndexPolicy::Enabled => IndexState::Enabled(Box::new(
                IndexDescriptor::new(
                    self.default_embedding_model.ok_or_else(|| {
                        "enabled workspace requires defaultEmbeddingModel".to_owned()
                    })?,
                    self.embedding_routes,
                    self.embedding_runtimes,
                    crate::domain::FTS_CONFIG,
                )
                .map_err(|error| error.to_string())?,
            )),
        };
        let workspace = Workspace::new(
            layout.clone(),
            self.scan
                .into_rules(ignore_files)
                .map_err(|error| error.to_string())?,
            self.index_version,
            self.storage_generation,
            self.created_time,
            self.updated_time,
            index,
        )
        .map_err(|error| error.to_string())?;
        workspace
            .validate_published()
            .map_err(|error| error.to_string())?;
        Ok(workspace)
    }
}

impl TryFrom<&Workspace> for ManifestData {
    type Error = EngineError;
    fn try_from(workspace: &Workspace) -> Result<Self, Self::Error> {
        let index = workspace.index().descriptor();
        Ok(Self {
            format: "zg-workspace".into(),
            source_root: "..".into(),
            scan: StoredScanRules::from_rules(workspace.layout().source_root(), workspace.scan())?,
            index_policy: match workspace.index() {
                IndexState::Uninitialized => IndexPolicy::Uninitialized,
                IndexState::Disabled => IndexPolicy::Disabled,
                IndexState::Enabled(_) => IndexPolicy::Enabled,
            },
            default_embedding_model: index.map(|index| index.default_embedding_model().clone()),
            embedding_routes: index
                .map_or_else(BTreeMap::new, |index| index.embedding_routes().clone()),
            index_version: workspace.index_version(),
            storage_generation: workspace.storage_generation().map(str::to_owned),
            created_time: workspace.created_epoch_ms(),
            updated_time: workspace.updated_epoch_ms(),
            embedding_runtimes: index
                .map_or_else(BTreeMap::new, |index| index.embedding_configs().clone()),
        })
    }
}

impl Serialize for Workspace {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ManifestData::try_from(self)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

/// Inspect the format header without asking a current-format decoder to read an old index.
#[derive(Debug)]
pub(crate) enum ManifestState {
    Missing,
    Current(Box<Workspace>),
    RebuildRequired {
        actual_version: Option<u32>,
        reason: String,
    },
}

impl ManifestState {
    /// Explicit rebuild uses current configuration only when it is readable.
    /// Incompatible data is never decoded or migrated to recover old settings.
    pub(crate) fn into_workspace(self, rebuild: bool) -> Result<Option<Workspace>, EngineError> {
        match self {
            Self::Missing => Ok(None),
            Self::Current(manifest) => Ok(Some(*manifest)),
            Self::RebuildRequired { .. } if rebuild => Ok(None),
            Self::RebuildRequired { reason, .. } => Err(rebuild_required(reason)),
        }
    }
}

pub(crate) fn require_current_index_version(version: Option<u32>) -> Result<(), EngineError> {
    if version != Some(super::CURRENT_INDEX_VERSION) {
        return Err(rebuild_required(version_mismatch(version)));
    }
    Ok(())
}

fn version_mismatch(version: Option<u32>) -> String {
    let actual = version.map_or_else(|| "missing".to_owned(), |value| value.to_string());
    format!(
        "unsupported index version {actual}; expected {}",
        super::CURRENT_INDEX_VERSION,
    )
}

fn rebuild_required(reason: impl std::fmt::Display) -> EngineError {
    EngineError::storage_failure(format!(
        "{reason}; rebuild the index with `zg --index --rebuild`"
    ))
}

pub(crate) fn inspect_workspace_manifest(
    layout: &WorkspaceLayout,
) -> Result<ManifestState, EngineError> {
    inspect_workspace_manifest_with_ignore_files(layout, None)
}

pub(crate) fn inspect_workspace_manifest_with_ignore_files(
    layout: &WorkspaceLayout,
    ignore_files: Option<&[PathBuf]>,
) -> Result<ManifestState, EngineError> {
    let path = layout.manifest_path();
    reject_symlink(path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestState::Missing);
        }
        Err(error) => return Err(manifest_io("read", path, &error)),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: None,
                reason: format!("invalid workspace manifest: {error}"),
            });
        }
    };
    let version = value
        .get("indexVersion")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok());
    // Only an explicitly unbuilt workspace may omit the version. An enabled index
    // with a missing version must never be silently replaced by ordinary indexing.
    let unbuilt = value
        .get("indexVersion")
        .is_none_or(serde_json::Value::is_null)
        && matches!(
            value.get("indexPolicy").and_then(serde_json::Value::as_str),
            Some("disabled" | "uninitialized")
        )
        && value
            .get("storageGeneration")
            .is_none_or(serde_json::Value::is_null);
    if !unbuilt && version != Some(super::CURRENT_INDEX_VERSION) {
        return Ok(ManifestState::RebuildRequired {
            actual_version: version,
            reason: version_mismatch(version),
        });
    }
    if value["embeddingRuntimes"]
        .as_object()
        .is_some_and(|runtimes| {
            runtimes
                .values()
                .any(|runtime| runtime["apiKey"].is_string())
        })
    {
        return Ok(ManifestState::RebuildRequired {
            actual_version: version,
            reason: "unsupported plaintext workspace credentials".into(),
        });
    }
    let data = match serde_json::from_value::<ManifestData>(value) {
        Ok(data) => data,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: version,
                reason: format!("invalid workspace manifest: {error}"),
            });
        }
    };
    data.scan.check_dependencies(ignore_files)?;
    let manifest = match data.into_workspace(layout, ignore_files) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: version,
                reason: format!("invalid workspace manifest: {error}"),
            });
        }
    };
    Ok(ManifestState::Current(Box::new(manifest)))
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct StoredScanRules {
    globs: Vec<crate::domain::GlobRule>,
    included_formats: Vec<crate::domain::FileFormat>,
    excluded_formats: Vec<crate::domain::FileFormat>,
    include_hidden: bool,
    follow_symlinks: bool,
    max_depth: Option<usize>,
    max_file_size_bytes: Option<u64>,
    use_default_ignores: bool,
    ignore_files: Vec<StoredIgnoreFile>,
    nested_git: bool,
}

impl Default for StoredScanRules {
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

#[derive(Deserialize, Serialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
enum StoredIgnoreFile {
    Workspace { path: crate::storage::PathRecord },
    External { platform: String, path: String },
}

fn path_platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}

impl StoredScanRules {
    fn from_rules(root: &Path, scan: &ScanRules) -> Result<Self, EngineError> {
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| EngineError::from_io("resolve workspace root", &error))?;
        let ignore_files = scan.ignore_files().iter().map(|path| {
            // Keep source-relative aliases so replacing a symlink remains observable.
            let relative = if path.is_relative() { Some(path.as_path()) } else {
                path.strip_prefix(root).or_else(|_| path.strip_prefix(&canonical_root)).ok()
            };
            if let Some(relative) = relative
                && let Ok(path) = crate::storage::PathRecord::from_path(relative) {
                return Ok(StoredIgnoreFile::Workspace { path });
            }
            let absolute = canonical_root.join(path);
            let absolute = match fs::canonicalize(&absolute) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => absolute,
                Err(error) => return Err(EngineError::from_io("resolve ignore file path", &error)),
            };
            if let Ok(relative) = absolute.strip_prefix(&canonical_root) {
                return crate::storage::PathRecord::from_path(relative)
                    .map(|path| StoredIgnoreFile::Workspace { path });
            }
            let path = absolute.to_str().ok_or_else(|| EngineError::invalid_argument(
                "external ignore file path must be Unicode to persist it as a platform-specific dependency",
            ))?;
            Ok(StoredIgnoreFile::External { platform: path_platform().into(), path: path.into() })
        }).collect::<Result<Vec<_>, EngineError>>()?;
        Ok(Self {
            globs: scan.globs().to_vec(),
            included_formats: scan.included_formats().to_vec(),
            excluded_formats: scan.excluded_formats().to_vec(),
            include_hidden: scan.include_hidden(),
            follow_symlinks: scan.follow_symlinks(),
            max_depth: scan.max_depth(),
            max_file_size_bytes: scan.max_file_size_bytes(),
            use_default_ignores: scan.use_default_ignores(),
            ignore_files,
            nested_git: scan.nested_git(),
        })
    }

    fn check_dependencies(&self, override_files: Option<&[PathBuf]>) -> Result<(), EngineError> {
        if override_files.is_some() {
            return Ok(());
        }
        for file in &self.ignore_files {
            let StoredIgnoreFile::External { platform, path } = file else {
                continue;
            };
            if platform != path_platform() || !Path::new(path).is_absolute() {
                return Err(EngineError::invalid_argument(format!(
                    "external ignore file {path:?} ({platform}) is incompatible with this machine; use --reset-paths or --ignore-file to replace its scan configuration",
                )));
            }
            if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
                || fs::File::open(path).is_err()
            {
                return Err(EngineError::invalid_argument(format!(
                    "external ignore file {path:?} is unavailable; restore it or use --reset-paths or --ignore-file to replace its scan configuration",
                )));
            }
        }
        Ok(())
    }

    fn into_rules(self, override_files: Option<&[PathBuf]>) -> Result<ScanRules, EngineError> {
        self.check_dependencies(override_files)?;
        let files = if override_files.is_some() {
            Vec::new()
        } else {
            self.ignore_files
        };
        let paths = files
            .into_iter()
            .map(|file| match file {
                StoredIgnoreFile::Workspace { path } => path.into_path(),
                StoredIgnoreFile::External { path, .. } => Ok(PathBuf::from(path)),
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        ScanRules::new(
            self.globs,
            self.included_formats,
            self.excluded_formats,
            self.include_hidden,
            self.follow_symlinks,
            self.max_depth,
            self.max_file_size_bytes,
            self.use_default_ignores,
            override_files.map_or(paths, <[PathBuf]>::to_vec),
            self.nested_git,
        )
    }
}

pub(crate) fn read_workspace_manifest(
    layout: &WorkspaceLayout,
) -> Result<Option<Workspace>, EngineError> {
    inspect_workspace_manifest(layout)?.into_workspace(false)
}

pub(crate) fn write_workspace_manifest(
    path: &Path,
    manifest: &Workspace,
) -> Result<(), EngineError> {
    manifest.validate_published()?;
    if manifest.index_enabled() {
        require_current_index_version(manifest.index_version())?;
    }
    let parent_dir = manifest_home(path)?;
    ensure_directory(parent_dir)?;
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        EngineError::internal(format!("failed to encode workspace manifest: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)?;
    if parent_dir.file_name().is_some() {
        sync_directory(parent_dir.parent().unwrap_or(parent_dir))?;
    }
    Ok(())
}

pub(crate) fn delete_workspace_manifest(layout: &WorkspaceLayout) -> Result<(), EngineError> {
    let path = layout.manifest_path();
    reject_symlink(path)?;
    match fs::remove_file(path) {
        Ok(()) => sync_directory(layout.storage_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(manifest_io("delete", path, &error)),
    }
}

fn manifest_home(path: &Path) -> Result<&Path, EngineError> {
    path.parent()
        .ok_or_else(|| invalid_manifest("manifest path must have a parent directory"))
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

    fn decode_manifest(
        value: serde_json::Value,
        layout: &WorkspaceLayout,
    ) -> Result<Workspace, String> {
        serde_json::from_value::<ManifestData>(value)
            .map_err(|error| error.to_string())?
            .into_workspace(layout, None)
    }

    fn fixture_manifest(layout: &WorkspaceLayout) -> Workspace {
        let mut manifest = Workspace::new(
            layout.clone(),
            crate::api::index::options::ScanRulesUpdate {
                included_formats: Some(vec![crate::domain::FileFormat::Rust]),
                excluded_formats: Some(vec![crate::domain::FileFormat::Cpp]),
                globs: Some(vec!["*.rs".into()]),
                ..crate::api::index::options::ScanRulesUpdate::default()
            }
            .apply(&ScanRules::default())
            .expect("scan rules"),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            None,
            10,
            20,
            IndexState::Enabled(Box::new(
                IndexDescriptor::new(
                    EmbeddingModelInfo::new(
                        crate::domain::model::ModelInfo::new(
                            "local",
                            "minilm",
                            [
                                crate::domain::ContentKind::Text,
                                crate::domain::ContentKind::Code,
                                crate::domain::ContentKind::Image,
                            ],
                            32,
                        )
                        .expect("valid model metadata"),
                        384,
                        EmbeddingMetric::Cosine,
                        Some(8192),
                        Some(1_048_576),
                    )
                    .expect("valid embedding metadata"),
                    std::collections::BTreeMap::new(),
                    BTreeMap::from([("local/minilm".into(), ModelConfig::default())]),
                    crate::domain::FTS_CONFIG,
                )
                .expect("index descriptor"),
            )),
        )
        .expect("fixture manifest");
        manifest
            .set_storage_generation(uuid::Uuid::new_v4().to_string())
            .expect("generation");
        manifest
    }

    #[test]
    fn saved_model_configs_belong_to_the_index_without_changing_compatibility() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let workspace = fixture_manifest(&layout);
        let mut value = serde_json::to_value(&workspace).expect("manifest");
        value["embeddingRuntimes"]["local/minilm"]["endpoint"] =
            serde_json::json!("https://example.test/embeddings");
        let updated = decode_manifest(value.clone(), &layout).expect("updated connection");
        workspace
            .index()
            .descriptor()
            .expect("index")
            .ensure_index_compatible(updated.index().descriptor().expect("index"))
            .expect("connection settings do not change index compatibility");
        value["embeddingRuntimes"]["remote/unused"] = serde_json::json!({});
        assert!(
            decode_manifest(value.clone(), &layout)
                .expect_err("unknown model configuration")
                .contains("not used by the index")
        );
        value["indexPolicy"] = serde_json::json!("uninitialized");
        value["indexVersion"] = serde_json::Value::Null;
        value["storageGeneration"] = serde_json::Value::Null;
        assert!(
            decode_manifest(value, &layout)
                .expect_err("unbuilt configuration")
                .contains("unbuilt workspace")
        );
    }

    #[test]
    fn credentials_follow_the_workspace_without_exposing_plaintext() {
        let directory = tempfile::tempdir().expect("directory");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("source root");
        let original_layout = WorkspaceLayout::new(original.clone()).expect("layout");
        fs::create_dir(original_layout.storage_dir()).expect("storage directory");
        let stored = crate::domain::model::ModelConfig {
            api_key: Some(
                crate::workspace::credentials::encrypt_api_key(
                    &original_layout,
                    "remote/model",
                    "example-secret",
                )
                .expect("encrypted key"),
            ),
            endpoint: Some("https://example.com".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&stored).expect("JSON");
        assert!(!json.contains("example-secret"));
        assert!(!json.contains("device"));
        assert!(!json.contains("cacheDir"));
        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move");
        let moved_layout = WorkspaceLayout::new(moved).expect("layout");
        assert_eq!(
            stored
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    &moved_layout,
                    "remote/model"
                ))
                .transpose()
                .expect("decrypt"),
            Some("example-secret".into())
        );
        assert!(
            stored
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    &moved_layout,
                    "remote/other"
                ))
                .transpose()
                .is_err()
        );
    }

    #[test]
    fn preparing_a_replacement_does_not_rotate_the_existing_key() {
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        let original = crate::domain::model::ModelConfig {
            api_key: Some(
                crate::workspace::credentials::encrypt_api_key(
                    &layout,
                    "remote/model",
                    "old-secret",
                )
                .expect("encrypted key"),
            ),
            endpoint: None,
            ..Default::default()
        };
        let key = fs::read(layout.credentials_key_path()).expect("key");
        let replacement = crate::domain::model::ModelConfig {
            api_key: Some(
                crate::workspace::credentials::encrypt_api_key(
                    &layout,
                    "remote/model",
                    "new-secret",
                )
                .expect("encrypted key"),
            ),
            endpoint: None,
            ..Default::default()
        };
        assert_eq!(
            fs::read(layout.credentials_key_path()).expect("same key"),
            key
        );
        assert_eq!(
            original
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    &layout,
                    "remote/model"
                ))
                .transpose()
                .expect("old key"),
            Some("old-secret".into())
        );
        assert_eq!(
            replacement
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    &layout,
                    "remote/model"
                ))
                .transpose()
                .expect("new key"),
            Some("new-secret".into())
        );
    }

    #[test]
    fn enabled_manifests_require_a_generation_at_persistence_boundaries() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        let mut draft = fixture_manifest(&layout);
        draft = crate::domain::Workspace::new(
            draft.layout().clone(),
            draft.scan().clone(),
            draft.index_version(),
            None,
            draft.created_epoch_ms(),
            draft.updated_epoch_ms(),
            draft.index().clone(),
        )
        .expect("workspace");

        let error = write_workspace_manifest(layout.manifest_path(), &draft)
            .expect_err("draft must not be published without a generation");
        assert!(error.message().contains("storageGeneration"));
        assert!(!layout.manifest_path().exists());
        let value = serde_json::to_value(&draft).expect("draft JSON");
        assert!(value.get("storageGeneration").is_none());
        for null_generation in [false, true] {
            let mut missing_generation = value.clone();
            if null_generation {
                missing_generation["storageGeneration"] = serde_json::Value::Null;
            }
            let error = decode_manifest(missing_generation, draft.layout())
                .expect_err("enabled manifest requires a generation");
            assert!(error.contains("storageGeneration"));
        }
        fs::create_dir(&storage_dir).expect("workspace home");
        let build = crate::workspace::index::prepare_build(draft).expect("allocate generation");
        assert!(build.storage_generation().is_some());
        assert!(build.layout().staged_index_dir().is_dir());
        write_workspace_manifest(layout.manifest_path(), &build)
            .expect("publish allocated generation");
        assert_eq!(
            read_workspace_manifest(&layout).expect("read manifest"),
            Some(build)
        );
    }

    #[test]
    fn manifest_requires_current_fields_without_migration_aliases() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        let value = serde_json::to_value(&manifest).expect("manifest");
        for (field, extra) in [
            ("id", serde_json::json!(uuid::Uuid::new_v4())),
            ("name", serde_json::json!("docs")),
            (
                "rootPaths",
                serde_json::json!([{"absolutePath": directory.path(), "recursive": true}]),
            ),
            ("embedding", value["defaultEmbeddingModel"].clone()),
            (
                "embeddingRuntime",
                value["embeddingRuntimes"]["local/minilm"].clone(),
            ),
        ] {
            let mut unsupported = value.clone();
            unsupported[field] = extra;
            let error = decode_manifest(unsupported, manifest.layout())
                .expect_err("unsupported manifest field");
            assert!(error.contains(field));
        }
        let mut missing_root = value;
        missing_root
            .as_object_mut()
            .expect("manifest object")
            .remove("sourceRoot");
        let error =
            decode_manifest(missing_root, manifest.layout()).expect_err("sourceRoot is required");
        assert!(error.contains("sourceRoot"));
    }

    #[test]
    fn manifest_validates_content_routes_and_shared_model_definitions() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        let value = serde_json::to_value(&manifest).expect("manifest");
        let mut routed = value.clone();
        let mut second = value["defaultEmbeddingModel"].clone();
        second["model"]["name"] = serde_json::json!("second-model");
        routed["embeddingRoutes"]["image"] = second;
        let routed: Workspace =
            decode_manifest(routed, manifest.layout()).expect("typed image route");
        assert_eq!(
            routed
                .index()
                .descriptor()
                .expect("index")
                .embeddings()
                .len(),
            2
        );
        assert_eq!(
            routed
                .index()
                .descriptor()
                .expect("index")
                .model_for(ContentKind::Image)
                .expect("image route")
                .model()
                .reference(),
            "local/second-model",
        );
        for kind in ["code", "image"] {
            let mut unsupported = value.clone();
            let mut embedding = value["defaultEmbeddingModel"].clone();
            embedding["model"]["contentKinds"] = serde_json::json!(["text"]);
            unsupported["embeddingRoutes"][kind] = embedding;
            assert!(decode_manifest(unsupported, manifest.layout()).is_err());
        }
        let mut conflicting = value.clone();
        let mut embedding = value["defaultEmbeddingModel"].clone();
        embedding["dimension"] = serde_json::json!(32);
        conflicting["embeddingRoutes"]["code"] = embedding;
        assert!(decode_manifest(conflicting, manifest.layout()).is_err());
    }

    #[test]
    fn enabled_manifest_requires_a_descriptor_and_does_not_persist_runtime_status() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        let mut json = serde_json::to_value(&manifest).expect("serialize");
        json["defaultEmbeddingModel"] = serde_json::Value::Null;
        assert!(decode_manifest(json, manifest.layout()).is_err());
        for index in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            manifest.index().clone(),
        ] {
            let mut manifest = manifest.clone();
            manifest = {
                crate::domain::Workspace::new(
                    manifest.layout().clone(),
                    manifest.scan().clone(),
                    if matches!(index, crate::domain::IndexState::Enabled(_)) {
                        manifest
                            .index_version()
                            .or(Some(crate::workspace::CURRENT_INDEX_VERSION))
                    } else {
                        None
                    },
                    if matches!(index, crate::domain::IndexState::Enabled(_)) {
                        manifest.storage_generation().map(str::to_owned)
                    } else {
                        None
                    },
                    manifest.created_epoch_ms(),
                    manifest.updated_epoch_ms(),
                    index,
                )
                .expect("workspace")
            };
            if !manifest.index_enabled() {
                manifest = crate::domain::Workspace::new(
                    manifest.layout().clone(),
                    manifest.scan().clone(),
                    manifest.index_version(),
                    None,
                    manifest.created_epoch_ms(),
                    manifest.updated_epoch_ms(),
                    manifest.index().clone(),
                )
                .expect("workspace");
            }
            let json = serde_json::to_value(&manifest).expect("serialize");
            assert!(json.get("status").is_none());
            assert!(json.get("indexStatus").is_none());
            assert_eq!(
                decode_manifest(json, manifest.layout()).expect("deserialize"),
                manifest
            );
        }
    }

    #[test]
    fn writes_and_reads_a_single_workspace_root() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let manifest = fixture_manifest(&layout);

        write_workspace_manifest(layout.manifest_path(), &manifest).expect("write manifest");
        let text = fs::read_to_string(layout.manifest_path()).expect("manifest text");
        let json: serde_json::Value = serde_json::from_str(&text).expect("manifest json");

        assert!(json.get("manifestVersion").is_none());
        assert_eq!(
            json["indexVersion"],
            crate::workspace::CURRENT_INDEX_VERSION
        );
        assert!(json.get("id").is_none());
        assert!(json.get("rootPaths").is_none());
        assert!(json.get("root").is_none());
        assert!(json.get("path").is_none());
        assert_eq!(json["sourceRoot"], "..");
        assert_eq!(json["format"], "zg-workspace");
        assert_eq!(json["scan"]["globs"][0]["pattern"], "*.rs");
        assert_eq!(
            json["scan"]["included_formats"],
            serde_json::json!(["rust"])
        );
        assert_eq!(json["scan"]["excluded_formats"], serde_json::json!(["cpp"]));
        assert!(
            json["embeddingRuntimes"]["local/minilm"]
                .get("device")
                .is_none()
        );
        assert!(
            json["embeddingRuntimes"]["local/minilm"]
                .get("cacheDir")
                .is_none()
        );
        assert!(json.get("generation").is_none());
        assert_eq!(
            read_workspace_manifest(&layout).expect("read manifest"),
            Some(manifest)
        );
    }

    #[test]
    fn removed_selection_fields_are_rejected_instead_of_widening_scan_scope() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        let value = serde_json::to_value(&manifest).expect("manifest JSON");
        let mut split_filter = value.clone();
        split_filter["filter"] = serde_json::json!({"globs": ["*.rs"]});
        assert!(decode_manifest(split_filter, manifest.layout()).is_err());
        let mut query_constraint = value;
        query_constraint["scan"]["categories"] = serde_json::json!(["code"]);
        assert!(decode_manifest(query_constraint, manifest.layout()).is_err());
    }

    #[test]
    fn reading_a_moved_workspace_rebases_only_its_location() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        fs::create_dir(&original).expect("original workspace");
        let mut manifest =
            fixture_manifest(&WorkspaceLayout::new(original.clone()).expect("layout"));
        write_workspace_manifest(manifest.layout().manifest_path(), &manifest)
            .expect("original manifest");
        fs::rename(&original, &moved).expect("move workspace");
        let relocated =
            read_workspace_manifest(&WorkspaceLayout::new(moved.clone()).expect("layout"))
                .expect("relocated manifest read")
                .expect("manifest");
        manifest = Workspace::new(
            WorkspaceLayout::new(moved).expect("layout"),
            manifest.scan().clone(),
            manifest.index_version(),
            manifest.storage_generation().map(str::to_owned),
            manifest.created_epoch_ms(),
            manifest.updated_epoch_ms(),
            manifest.index().clone(),
        )
        .expect("workspace");
        assert_eq!(relocated, manifest);
    }

    #[test]
    fn internal_ignore_paths_move_with_the_source() {
        let directory = tempdir().expect("directory");
        let original = directory.path().join("original");
        fs::create_dir_all(original.join("config")).expect("source");
        fs::write(original.join("config/ignore"), "*.log\n").expect("ignore");
        let mut manifest =
            fixture_manifest(&WorkspaceLayout::new(original.clone()).expect("layout"));
        manifest = crate::domain::Workspace::new(
            manifest.layout().clone(),
            crate::api::index::options::ScanRulesUpdate {
                ignore_files: Some(vec![original.join("config/ignore")]),
                ..Default::default()
            }
            .apply(manifest.scan())
            .expect("scan"),
            manifest.index_version(),
            manifest.storage_generation().map(str::to_owned),
            manifest.created_epoch_ms(),
            manifest.updated_epoch_ms(),
            manifest.index().clone(),
        )
        .expect("workspace");
        write_workspace_manifest(manifest.layout().manifest_path(), &manifest).expect("write");
        let json = serde_json::to_value(&manifest).expect("JSON");
        assert_eq!(json["scan"]["ignore_files"][0]["location"], "workspace");
        assert!(
            !serde_json::to_string(&json)
                .expect("JSON string")
                .contains(original.to_string_lossy().as_ref())
        );
        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move");
        let loaded = read_workspace_manifest(&WorkspaceLayout::new(moved.clone()).expect("layout"))
            .expect("read")
            .expect("manifest");
        assert_eq!(
            loaded.scan().ignore_files(),
            &[PathBuf::from("config/ignore")]
        );
        assert!(
            loaded
                .layout()
                .source_root()
                .join(&loaded.scan().ignore_files()[0])
                .is_file()
        );
        assert_eq!(
            loaded.scan().included_formats(),
            manifest.scan().included_formats()
        );
        assert_eq!(
            loaded.scan().excluded_formats(),
            manifest.scan().excluded_formats()
        );
    }

    #[test]
    fn unavailable_external_ignores_require_an_explicit_replacement() {
        let directory = tempdir().expect("directory");
        let root = directory.path().join("source");
        fs::create_dir(&root).expect("source");
        let external = directory.path().join("external.ignore");
        fs::write(&external, "*.log\n").expect("external ignore");
        let mut manifest = fixture_manifest(&WorkspaceLayout::new(root).expect("layout"));
        manifest = crate::domain::Workspace::new(
            manifest.layout().clone(),
            crate::api::index::options::ScanRulesUpdate {
                ignore_files: Some(vec![external.clone()]),
                ..Default::default()
            }
            .apply(manifest.scan())
            .expect("scan"),
            manifest.index_version(),
            manifest.storage_generation().map(str::to_owned),
            manifest.created_epoch_ms(),
            manifest.updated_epoch_ms(),
            manifest.index().clone(),
        )
        .expect("workspace");
        write_workspace_manifest(manifest.layout().manifest_path(), &manifest).expect("write");
        read_workspace_manifest(manifest.layout()).expect("available dependency");
        fs::remove_file(&external).expect("remove external dependency");
        assert!(
            read_workspace_manifest(manifest.layout())
                .expect_err("dependency required")
                .message()
                .contains("--ignore-file")
        );
        let loaded = inspect_workspace_manifest_with_ignore_files(manifest.layout(), Some(&[]))
            .expect("explicit reset")
            .into_workspace(false)
            .expect("valid")
            .expect("manifest");
        assert!(loaded.scan().ignore_files().is_empty());
        let mut json = serde_json::to_value(&manifest).expect("JSON");
        json["scan"]["ignore_files"][0]["platform"] = serde_json::json!("another-platform");
        fs::write(
            manifest.layout().manifest_path(),
            serde_json::to_vec(&json).expect("JSON"),
        )
        .expect("write foreign dependency");
        assert!(read_workspace_manifest(manifest.layout()).is_err());
        assert!(inspect_workspace_manifest_with_ignore_files(manifest.layout(), Some(&[])).is_ok());
    }

    #[test]
    fn local_runtime_fields_and_plaintext_keys_are_not_accepted() {
        let directory = tempdir().expect("directory");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        for (field, value) in [
            ("device", serde_json::json!("cpu")),
            ("cacheDir", serde_json::json!("/old/models")),
            ("apiKey", serde_json::json!("plaintext-secret")),
        ] {
            let mut json = serde_json::to_value(&manifest).expect("JSON");
            json["embeddingRuntimes"]["local/minilm"][field] = value;
            assert!(decode_manifest(json, manifest.layout()).is_err(), "{field}");
        }
    }

    #[test]
    fn rejects_other_index_versions_before_decoding_the_manifest_schema() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        fs::create_dir_all(&storage_dir).expect("workspace home");
        fs::write(layout.manifest_path(), r#"{"indexVersion":2}"#).expect("invalid manifest");
        assert!(read_workspace_manifest(&layout).is_err());
        let current = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &current).expect("current manifest");
        let bytes = fs::read(layout.manifest_path()).expect("persisted manifest");
        for version in [0, 1, 3, 4, 5, u32::MAX] {
            let mut unsupported = current.clone();
            unsupported = crate::domain::Workspace::new(
                unsupported.layout().clone(),
                unsupported.scan().clone(),
                Some(version),
                unsupported.storage_generation().map(str::to_owned),
                unsupported.created_epoch_ms(),
                unsupported.updated_epoch_ms(),
                unsupported.index().clone(),
            )
            .expect("workspace");
            assert!(write_workspace_manifest(layout.manifest_path(), &unsupported).is_err());
            assert_eq!(
                fs::read(layout.manifest_path()).expect("preserved manifest"),
                bytes
            );
            fs::write(
                layout.manifest_path(),
                // Deliberately omit all current-format fields. The version header
                // must win over schema errors, including Node.js format version 1.
                serde_json::to_vec(&serde_json::json!({
                    "indexVersion": version,
                    "manifestVersion": 1,
                    "rootPaths": [],
                }))
                .expect("unsupported manifest JSON"),
            )
            .expect("unsupported manifest");
            let error = read_workspace_manifest(&layout).expect_err("unsupported index version");
            assert!(
                error
                    .message()
                    .contains(&format!("unsupported index version {version}"))
            );
            assert!(error.message().contains("expected 2"));
            assert!(error.message().contains("zg --index --rebuild"));
            fs::write(layout.manifest_path(), &bytes).expect("restore current manifest");
        }
    }

    #[test]
    fn enabled_indexes_cannot_omit_the_version_and_rebuild_can_replace_invalid_metadata() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("current manifest");
        let mut value = serde_json::to_value(&manifest).expect("manifest JSON");
        value["indexVersion"] = serde_json::Value::Null;
        for missing in [false, true] {
            let mut invalid = value.clone();
            if missing {
                invalid
                    .as_object_mut()
                    .expect("object")
                    .remove("indexVersion");
            }
            fs::write(layout.manifest_path(), invalid.to_string()).expect("raw manifest");
            assert!(
                read_workspace_manifest(&layout)
                    .expect_err("version required")
                    .message()
                    .contains("rebuild")
            );
            assert!(
                inspect_workspace_manifest(&layout)
                    .expect("inspect")
                    .into_workspace(true)
                    .expect("rebuild bypasses old data")
                    .is_none()
            );
        }
        fs::write(layout.manifest_path(), "{broken").expect("corrupt JSON");
        assert!(read_workspace_manifest(&layout).is_err());
        assert!(
            inspect_workspace_manifest(&layout)
                .expect("inspect")
                .into_workspace(true)
                .expect("rebuild bypasses corrupt data")
                .is_none()
        );
    }

    #[test]
    fn rejects_invalid_embedding_limits_read_from_manifest() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout"),
        );
        for field in ["maxBatchSize", "maxInputTokens", "maxImageBytes"] {
            let mut json = serde_json::to_value(&manifest).expect("manifest JSON");
            if field == "maxBatchSize" {
                json["defaultEmbeddingModel"]["model"][field] = serde_json::json!(0);
            } else {
                json["defaultEmbeddingModel"][field] = serde_json::json!(0);
            }
            assert!(decode_manifest(json, manifest.layout()).is_err(), "{field}");
        }
    }

    #[test]
    fn deleting_a_missing_manifest_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("workspace directory");
        assert!(delete_workspace_manifest(&layout).is_ok());
        let missing = WorkspaceLayout::new(directory.path().join("missing")).expect("layout");
        assert!(delete_workspace_manifest(&missing).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_operations_ignore_unrelated_storage_paths() {
        let directory = tempdir().expect("workspace");
        let external = tempdir().expect("external data");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        fs::create_dir(&storage_dir).expect("workspace home");
        for name in ["index", "temp"] {
            std::os::unix::fs::symlink(external.path(), storage_dir.join(name))
                .expect("storage alias");
        }
        fs::write(external.path().join("keep"), b"unchanged").expect("external file");

        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("write metadata");
        assert_eq!(
            read_workspace_manifest(&layout).expect("read metadata"),
            Some(manifest)
        );
        delete_workspace_manifest(&layout).expect("delete metadata");
        assert!(!layout.manifest_path().exists());
        assert_eq!(
            fs::read(external.path().join("keep")).expect("external file preserved"),
            b"unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_operations_reject_a_symlinked_manifest() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("initial manifest");
        let target = directory.path().join("saved-manifest.json");
        let path = layout.manifest_path().to_path_buf();
        fs::rename(&path, &target).expect("move manifest");
        let original = fs::read(&target).expect("saved bytes");
        std::os::unix::fs::symlink(&target, &path).expect("manifest alias");

        assert!(read_workspace_manifest(&layout).is_err());
        assert!(write_workspace_manifest(layout.manifest_path(), &manifest).is_err());
        assert!(delete_workspace_manifest(&layout).is_err());
        assert_eq!(fs::read(&target).expect("preserved bytes"), original);
        assert!(fs::symlink_metadata(&path).expect("alias").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn staged_manifest_writes_reject_a_symlinked_temporary_directory() {
        let directory = tempdir().expect("workspace");
        let external = tempdir().expect("external data");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("workspace storage directory");
        std::os::unix::fs::symlink(external.path(), layout.temp_dir()).expect("temporary alias");
        let external_manifest = external.path().join("manifest.json");
        fs::write(&external_manifest, b"external data").expect("external manifest");
        let manifest = fixture_manifest(&layout);

        assert!(write_workspace_manifest(layout.staged_manifest_path(), &manifest).is_err());
        assert_eq!(
            fs::read(external_manifest).expect("preserved external manifest"),
            b"external data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_uses_default_directory_permissions_and_manifest_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        let reference_dir = directory.path().join("reference");
        fs::create_dir(&reference_dir).expect("directory with default permissions");
        write_workspace_manifest(layout.manifest_path(), &fixture_manifest(&layout))
            .expect("write manifest");

        let directory_mode = fs::metadata(&storage_dir)
            .expect("workspace metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(layout.manifest_path())
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        let default_mode = fs::metadata(reference_dir)
            .expect("reference metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, default_mode);
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_manifest_preserves_file_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        let mut manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("initial manifest");
        let path = layout.manifest_path().to_path_buf();
        fs::set_permissions(&storage_dir, fs::Permissions::from_mode(0o750))
            .expect("custom workspace permissions");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("custom manifest permissions");

        manifest.record_update(30);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("replace manifest");

        assert_eq!(
            fs::metadata(&storage_dir)
                .expect("workspace metadata")
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("manifest metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            read_workspace_manifest(&layout).expect("updated manifest"),
            Some(manifest)
        );
    }
}
