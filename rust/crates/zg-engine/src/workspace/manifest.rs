use serde::Deserialize;
use std::{fs, path::Path};

use crate::{
    EngineError,
    domain::{IndexState, ScanRules, SourceRoot, Workspace, WorkspaceLayout},
    utils::{atomic_write, directory_exists, ensure_directory, reject_symlink, sync_directory},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestFields {
    source_root: SourceRoot,
    scan: ScanRules,
    index_version: Option<u32>,
    storage_generation: Option<String>,
    created_time: u64,
    updated_time: u64,
    index: IndexState,
}

impl ManifestFields {
    fn into_workspace(
        mut self,
        source_root: &Path,
        layout: &WorkspaceLayout,
    ) -> Result<Workspace, EngineError> {
        self.source_root.set_current_path(source_root)?;
        Workspace::from_manifest(
            self.source_root,
            layout.clone(),
            self.scan,
            self.index_version,
            self.storage_generation,
            self.created_time,
            self.updated_time,
            self.index,
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestHeader {
    source_root: Option<serde_json::Value>,
    index_version: Option<serde_json::Value>,
    storage_generation: Option<serde_json::Value>,
    index: Option<serde_json::Value>,
}

impl ManifestHeader {
    fn validate_source_root(
        &self,
        source_root: &Path,
        layout: &WorkspaceLayout,
    ) -> Result<(), EngineError> {
        let mut source: SourceRoot =
            serde_json::from_value(self.source_root.clone().unwrap_or(serde_json::Value::Null))
                .map_err(|error| {
                    rebuild_required(format!(
                        "invalid workspace manifest '{}': {error}",
                        layout.manifest_path().display(),
                    ))
                })?;
        source.set_current_path(source_root)
    }

    fn version(&self) -> Option<u32> {
        self.index_version
            .as_ref()
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u32::try_from(version).ok())
    }

    fn compatibility_error(&self) -> Option<String> {
        let unbuilt = self.index_version.is_none()
            && self.storage_generation.is_none()
            && matches!(
                self.index
                    .as_ref()
                    .and_then(|index| index["state"].as_str()),
                Some("disabled" | "uninitialized")
            );
        (!unbuilt && self.version() != Some(super::CURRENT_INDEX_VERSION))
            .then(|| version_mismatch(self.version()))
    }
}

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
    /// Explicit rebuild uses readable settings without migrating incompatible data.
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
    source_root: &Path,
    layout: &WorkspaceLayout,
) -> Result<ManifestState, EngineError> {
    let Some(bytes) = read_manifest_bytes(layout)? else {
        if directory_exists(layout.index_dir())? && !layout.has_build_record()? {
            return Ok(ManifestState::RebuildRequired {
                actual_version: None,
                reason: format!(
                    "workspace index '{}' has no manifest",
                    layout.index_dir().display(),
                ),
            });
        }
        return Ok(ManifestState::Missing);
    };
    let header: ManifestHeader = match serde_json::from_slice(&bytes) {
        Ok(header) => header,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: None,
                reason: format!(
                    "invalid workspace manifest '{}': {error}",
                    layout.manifest_path().display()
                ),
            });
        }
    };
    let version = header.version();
    if let Some(reason) = header.compatibility_error() {
        return Ok(ManifestState::RebuildRequired {
            actual_version: version,
            reason: format!(
                "workspace manifest '{}': {reason}",
                layout.manifest_path().display()
            ),
        });
    }
    let fields: ManifestFields = match serde_json::from_slice(&bytes) {
        Ok(fields) => fields,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: version,
                reason: format!(
                    "invalid workspace manifest '{}': {error}",
                    layout.manifest_path().display()
                ),
            });
        }
    };
    let workspace = match fields.into_workspace(source_root, layout) {
        Ok(workspace) => workspace,
        Err(error) if error.code() != EngineError::INVALID_ARGUMENT => return Err(error),
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: version,
                reason: format!(
                    "invalid workspace manifest '{}': {error}",
                    layout.manifest_path().display()
                ),
            });
        }
    };
    Ok(ManifestState::Current(Box::new(workspace)))
}

pub(super) fn require_compatible_manifest(
    source_root: &Path,
    layout: &WorkspaceLayout,
) -> Result<(), EngineError> {
    if let Some(bytes) = read_manifest_bytes(layout)? {
        let header: ManifestHeader = match serde_json::from_slice(&bytes) {
            Ok(header) => header,
            Err(error) => {
                return Err(rebuild_required(format!(
                    "invalid workspace manifest '{}': {error}",
                    layout.manifest_path().display()
                )));
            }
        };
        if let Some(reason) = header.compatibility_error() {
            return Err(rebuild_required(format!(
                "workspace manifest '{}': {reason}",
                layout.manifest_path().display()
            )));
        }
        header.validate_source_root(source_root, layout)?;
    }
    Ok(())
}

fn read_manifest_bytes(layout: &WorkspaceLayout) -> Result<Option<Vec<u8>>, EngineError> {
    let path = layout.manifest_path();
    reject_symlink(path)?;
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(manifest_io("read", path, &error)),
    }
}

pub(crate) fn read_workspace_manifest(
    source_root: &Path,
    layout: &WorkspaceLayout,
) -> Result<Option<Workspace>, EngineError> {
    inspect_workspace_manifest(source_root, layout)?.into_workspace(false)
}

pub(crate) fn write_workspace_manifest(
    path: &Path,
    manifest: &Workspace,
) -> Result<(), EngineError> {
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
    use zg_host_native::PathPolicy;

    use super::*;
    use crate::domain::model::{EmbeddingMetric, EmbeddingModelInfo, ModelConfig};
    use crate::domain::{ContentKind, IndexDescriptor};
    use std::collections::BTreeMap;

    fn decode_manifest(
        value: &serde_json::Value,
        layout: &WorkspaceLayout,
    ) -> Result<Workspace, String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        let fields: ManifestFields =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        fields
            .into_workspace(layout.storage_dir().parent().expect("source root"), layout)
            .map_err(|error| error.to_string())
    }

    fn fixture_manifest(layout: &WorkspaceLayout) -> Workspace {
        Workspace::new(
            layout.storage_dir().parent().expect("source root"),
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
            Some(uuid::Uuid::new_v4().to_string()),
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
        .expect("fixture manifest")
    }

    #[test]
    fn saved_model_configs_belong_to_the_index_without_changing_compatibility() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let workspace = fixture_manifest(&layout);
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        let mut value = serde_json::to_value(&workspace).expect("manifest");
        value["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]["endpoint"] =
            serde_json::json!("https://example.test/embeddings");
        let updated = decode_manifest(&value, &layout).expect("updated connection");
        workspace
            .index()
            .descriptor()
            .expect("index")
            .ensure_index_compatible(updated.index().descriptor().expect("index"))
            .expect("connection settings do not change index compatibility");
        value["index"]["descriptor"]["embeddingConfigs"]["remote/unused"] = serde_json::json!({});
        assert!(
            decode_manifest(&value, &layout)
                .expect_err("unknown model configuration")
                .contains("not referenced by defaultEmbeddingModel or embeddingRoutes")
        );
        value["index"]["state"] = serde_json::json!("uninitialized");
        value["indexVersion"] = serde_json::Value::Null;
        value["storageGeneration"] = serde_json::Value::Null;
        assert!(decode_manifest(&value, &layout).is_err());
    }

    #[test]
    fn credentials_follow_the_workspace_without_exposing_plaintext() {
        let directory = tempfile::tempdir().expect("directory");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("source root");
        let original_layout = WorkspaceLayout::new(original.join(".zvec-grep")).expect("layout");
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
        let moved_layout = WorkspaceLayout::new(moved.join(".zvec-grep")).expect("layout");
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
    fn enabled_workspaces_require_a_valid_generation() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        for generation in [None, Some("invalid".to_owned())] {
            let error = Workspace::new(
                directory.path(),
                layout.clone(),
                manifest.scan().clone(),
                manifest.index_version(),
                generation,
                manifest.created_epoch_ms(),
                manifest.updated_epoch_ms(),
                manifest.index().clone(),
            )
            .expect_err("enabled workspace requires a valid generation");
            assert!(error.message().contains("storageGeneration"));
        }
        assert!(!layout.manifest_path().exists());

        write_workspace_manifest(layout.manifest_path(), &manifest).expect("write manifest");
        for generation in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!("invalid")),
        ] {
            let mut value = serde_json::to_value(&manifest).expect("manifest JSON");
            if let Some(generation) = generation {
                value["storageGeneration"] = generation;
            } else {
                value
                    .as_object_mut()
                    .expect("manifest object")
                    .remove("storageGeneration");
            }
            fs::write(
                layout.manifest_path(),
                serde_json::to_vec(&value).expect("JSON"),
            )
            .expect("write invalid manifest");
            let error = read_workspace_manifest(directory.path(), &layout)
                .expect_err("enabled manifest requires a valid generation");
            assert!(error.message().contains("storageGeneration"));
        }
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("write valid manifest");
        assert_eq!(
            read_workspace_manifest(directory.path(), &layout).expect("read manifest"),
            Some(manifest)
        );
    }

    #[test]
    fn manifest_requires_current_fields_without_migration_aliases() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        let value = serde_json::to_value(&manifest).expect("manifest");
        for (field, extra) in [
            ("id", serde_json::json!(uuid::Uuid::new_v4())),
            ("name", serde_json::json!("docs")),
            (
                "rootPaths",
                serde_json::json!([{"absolutePath": directory.path(), "recursive": true}]),
            ),
            (
                "embedding",
                value["index"]["descriptor"]["defaultEmbeddingModel"].clone(),
            ),
            (
                "embeddingRuntime",
                value["index"]["descriptor"]["embeddingConfigs"]["local/minilm"].clone(),
            ),
        ] {
            let mut unsupported = value.clone();
            unsupported[field] = extra;
            let error = decode_manifest(&unsupported, manifest.layout())
                .expect_err("unsupported manifest field");
            assert!(error.contains(field));
        }
        let mut missing_root = value;
        missing_root
            .as_object_mut()
            .expect("manifest object")
            .remove("sourceRoot");
        let error =
            decode_manifest(&missing_root, manifest.layout()).expect_err("sourceRoot is required");
        assert!(error.contains("sourceRoot"));
    }

    #[test]
    fn manifest_validates_content_routes_and_shared_model_definitions() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        fs::create_dir(manifest.layout().storage_dir()).expect("storage directory");
        let value = serde_json::to_value(&manifest).expect("manifest");
        let mut routed = value.clone();
        let mut second = value["index"]["descriptor"]["defaultEmbeddingModel"].clone();
        second["model"]["name"] = serde_json::json!("second-model");
        routed["index"]["descriptor"]["embeddingRoutes"]["image"] = second;
        let routed: Workspace =
            decode_manifest(&routed, manifest.layout()).expect("typed image route");
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
            let mut embedding = value["index"]["descriptor"]["defaultEmbeddingModel"].clone();
            embedding["model"]["contentKinds"] = serde_json::json!(["text"]);
            unsupported["index"]["descriptor"]["embeddingRoutes"][kind] = embedding;
            assert!(decode_manifest(&unsupported, manifest.layout()).is_err());
        }
        let mut conflicting = value.clone();
        let mut embedding = value["index"]["descriptor"]["defaultEmbeddingModel"].clone();
        embedding["dimension"] = serde_json::json!(32);
        conflicting["index"]["descriptor"]["embeddingRoutes"]["code"] = embedding;
        assert!(decode_manifest(&conflicting, manifest.layout()).is_err());
    }

    #[test]
    fn enabled_manifest_requires_a_descriptor_and_does_not_persist_runtime_status() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        fs::create_dir(manifest.layout().storage_dir()).expect("storage directory");
        let mut json = serde_json::to_value(&manifest).expect("serialize");
        json["index"]["descriptor"]["defaultEmbeddingModel"] = serde_json::Value::Null;
        assert!(decode_manifest(&json, manifest.layout()).is_err());
        for index in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            manifest.index().clone(),
        ] {
            let mut manifest = manifest.clone();
            manifest = {
                crate::domain::Workspace::new(
                    directory.path(),
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
                    directory.path(),
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
                decode_manifest(&json, manifest.layout()).expect("deserialize"),
                manifest
            );
        }
    }

    #[test]
    fn writes_and_reads_a_single_workspace_root() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
        assert!(json["sourceRoot"].get("relative").is_none());
        assert!(json["sourceRoot"].get("currentPath").is_none());
        assert_eq!(
            json["sourceRoot"]["recordedPath"],
            manifest.source_root().to_string_lossy().as_ref()
        );
        assert!(json.get("format").is_none());
        let fields = [
            "sourceRoot",
            "scan",
            "indexVersion",
            "storageGeneration",
            "createdTime",
            "updatedTime",
            "index",
        ];
        let positions: Vec<_> = fields
            .iter()
            .map(|field| text.find(&format!("\"{field}\":")).expect("manifest field"))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(json["scan"]["globs"][0]["pattern"], "*.rs");
        assert_eq!(
            json["scan"]["included_formats"],
            serde_json::json!(["rust"])
        );
        assert_eq!(json["scan"]["excluded_formats"], serde_json::json!(["cpp"]));
        assert!(
            json["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]
                .get("device")
                .is_none()
        );
        assert!(
            json["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]
                .get("cacheDir")
                .is_none()
        );
        assert!(json.get("generation").is_none());
        assert_eq!(
            read_workspace_manifest(directory.path(), &layout).expect("read manifest"),
            Some(manifest)
        );
    }

    #[test]
    fn removed_selection_fields_are_rejected_instead_of_widening_scan_scope() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        let value = serde_json::to_value(&manifest).expect("manifest JSON");
        let mut split_filter = value.clone();
        split_filter["filter"] = serde_json::json!({"globs": ["*.rs"]});
        assert!(decode_manifest(&split_filter, manifest.layout()).is_err());
        let mut query_constraint = value;
        query_constraint["scan"]["categories"] = serde_json::json!(["code"]);
        assert!(decode_manifest(&query_constraint, manifest.layout()).is_err());
    }

    #[test]
    fn moving_a_workspace_preserves_the_recorded_location() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        fs::create_dir(&original).expect("original workspace");
        let manifest =
            fixture_manifest(&WorkspaceLayout::new(original.join(".zvec-grep")).expect("layout"));
        write_workspace_manifest(manifest.layout().manifest_path(), &manifest).expect("manifest");
        let saved = fs::read(manifest.layout().manifest_path()).expect("saved bytes");
        fs::rename(&original, &moved).expect("move workspace");
        let relocated = read_workspace_manifest(
            &moved,
            &WorkspaceLayout::new(moved.join(".zvec-grep")).expect("layout"),
        )
        .expect("read moved workspace")
        .expect("manifest");
        assert_eq!(
            relocated.source_root(),
            moved.canonicalize().expect("moved root")
        );
        assert_eq!(relocated.index(), manifest.index());
        assert_eq!(relocated.scan(), manifest.scan());
        let json = serde_json::to_value(&relocated).expect("JSON");
        assert_eq!(
            json["sourceRoot"]["recordedPath"],
            manifest.source_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            fs::read(relocated.layout().manifest_path()).expect("unchanged manifest"),
            saved
        );
    }

    #[test]
    fn missing_current_source_directory_returns_not_found() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("manifest");

        let error = inspect_workspace_manifest(&directory.path().join("missing"), &layout)
            .expect_err("source directory is unavailable");
        assert_eq!(error.code(), EngineError::NOT_FOUND);
    }

    #[test]
    fn historical_source_locations_do_not_change_the_discovered_root() {
        let directory = tempdir().expect("workspace");
        let other = directory.path().join("another-project");
        fs::create_dir(&other).expect("other source root");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("manifest");
        for source in [
            other.to_string_lossy().into_owned(),
            directory
                .path()
                .join("missing")
                .to_string_lossy()
                .into_owned(),
            r"C:\source".to_owned(),
        ] {
            let mut json = serde_json::to_value(&manifest).expect("JSON");
            json["sourceRoot"]["recordedPath"] = serde_json::json!(source);
            fs::write(layout.manifest_path(), json.to_string()).expect("saved location");
            let loaded = read_workspace_manifest(directory.path(), &layout)
                .expect("load at discovered location")
                .expect("workspace");
            assert_eq!(loaded.source_root(), manifest.source_root());
            assert_eq!(
                serde_json::to_value(loaded).expect("saved history")["sourceRoot"]["recordedPath"],
                source
            );
            require_compatible_manifest(directory.path(), &layout)
                .expect("historical location does not prevent recovery");
        }
    }

    #[test]
    fn obsolete_relative_source_locations_require_rebuild() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("manifest");
        let mut json = serde_json::to_value(&manifest).expect("JSON");
        json["sourceRoot"]["relative"] = serde_json::json!("..");
        fs::write(layout.manifest_path(), json.to_string()).expect("old development format");

        let error =
            read_workspace_manifest(directory.path(), &layout).expect_err("rebuild required");
        assert!(error.message().contains("relative"));
        assert!(error.message().contains("zg --index --rebuild"));
        assert!(
            inspect_workspace_manifest(directory.path(), &layout)
                .expect("inspect")
                .into_workspace(true)
                .expect("rebuild can replace obsolete metadata")
                .is_none()
        );
    }

    #[test]
    fn duplicate_manifest_and_scan_fields_are_rejected() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        let json = serde_json::to_string(&manifest).expect("JSON");
        for duplicate in [
            json.replacen("\"scan\":", "\"scan\":{},\"scan\":", 1),
            json.replacen(
                "\"include_hidden\":false",
                "\"include_hidden\":true,\"include_hidden\":false",
                1,
            ),
        ] {
            let error = serde_json::from_slice::<ManifestFields>(duplicate.as_bytes())
                .err()
                .expect("duplicate field");
            assert!(error.to_string().contains("duplicate field"));
        }
    }

    #[test]
    fn internal_ignore_paths_move_with_the_source() {
        let directory = tempdir().expect("directory");
        let original = directory.path().join("original");
        fs::create_dir_all(original.join("config")).expect("source");
        fs::write(original.join("config/ignore"), "*.log\n").expect("ignore");
        let mut manifest =
            fixture_manifest(&WorkspaceLayout::new(original.join(".zvec-grep")).expect("layout"));
        let mut update = crate::api::index::options::ScanRulesUpdate {
            ignore_files: Some(vec![original.join("config/ignore")]),
            ..Default::default()
        };
        update
            .normalize_ignore_paths(manifest.source_root())
            .expect("normalize requested paths");
        manifest = crate::domain::Workspace::new(
            &original,
            manifest.layout().clone(),
            update.apply(manifest.scan()).expect("scan"),
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
        assert_eq!(
            json["sourceRoot"]["recordedPath"],
            manifest.source_root().to_string_lossy().as_ref()
        );
        assert_eq!(
            json["scan"]["ignore_files"][0]["path"]["components"],
            serde_json::json!(["config", "ignore"])
        );
        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move");
        let loaded = read_workspace_manifest(
            &moved,
            &WorkspaceLayout::new(moved.join(".zvec-grep")).expect("layout"),
        )
        .expect("read")
        .expect("manifest");
        let ignore_files = loaded.scan().ignore_files().collect::<Vec<_>>();
        assert_eq!(ignore_files, vec![Path::new("config/ignore")]);
        assert!(loaded.source_root().join(ignore_files[0]).is_file());
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
    fn missing_external_ignore_files_remain_saved_and_apply_when_restored() {
        let directory = tempdir().expect("directory");
        let root = directory.path().join("source");
        fs::create_dir(&root).expect("source");
        let external = directory.path().join("external.ignore");
        fs::write(&external, "blocked.rs\n").expect("external ignore");
        let manifest =
            fixture_manifest(&WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout"));
        let scan = crate::api::index::options::ScanRulesUpdate {
            globs: Some(Vec::new()),
            ignore_files: Some(vec![external.clone()]),
            ..Default::default()
        }
        .apply(manifest.scan())
        .expect("scan");
        let manifest = Workspace::new(
            &root,
            manifest.layout().clone(),
            scan,
            manifest.index_version(),
            manifest.storage_generation().map(str::to_owned),
            manifest.created_epoch_ms(),
            manifest.updated_epoch_ms(),
            manifest.index().clone(),
        )
        .expect("workspace");
        write_workspace_manifest(manifest.layout().manifest_path(), &manifest).expect("write");
        fs::remove_file(&external).expect("remove dependency");
        let loaded = read_workspace_manifest(manifest.source_root(), manifest.layout())
            .expect("read metadata")
            .expect("manifest");
        assert_eq!(loaded.scan(), manifest.scan());
        let policy = crate::file_selection::ScanPolicy::new(loaded.source_root(), loaded.scan())
            .expect("missing rules are skipped");
        let source = loaded.source_root().join("blocked.rs");
        assert!(policy.includes_file(&source).expect("no rule applies"));
        write_workspace_manifest(loaded.layout().manifest_path(), &loaded).expect("save metadata");
        let reloaded = read_workspace_manifest(manifest.source_root(), manifest.layout())
            .expect("read saved metadata")
            .expect("manifest");
        assert_eq!(reloaded.scan(), manifest.scan());
        fs::write(&external, "blocked.rs\n").expect("restore ignore file");
        let policy =
            crate::file_selection::ScanPolicy::new(reloaded.source_root(), reloaded.scan())
                .expect("restored rules");
        assert!(
            !policy
                .includes_file(&source)
                .expect("restored rule applies")
        );
    }

    #[test]
    fn relocation_invalidates_environment_settings_without_changing_the_index() {
        let directory = tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let external = tempfile::NamedTempFile::new().expect("external ignore");
        fs::write(external.path(), "external.rs\n").expect("external rules");
        fs::write(directory.path().join("local.ignore"), "*.log\n").expect("local ignore");
        let manifest = fixture_manifest(&layout);
        let scan = crate::api::index::options::ScanRulesUpdate {
            globs: Some(Vec::new()),
            ignore_files: Some(vec!["local.ignore".into(), external.path().to_path_buf()]),
            ..Default::default()
        }
        .apply(manifest.scan())
        .expect("scan");
        let manifest = Workspace::new(
            directory.path(),
            layout.clone(),
            scan,
            manifest.index_version(),
            manifest.storage_generation().map(str::to_owned),
            manifest.created_epoch_ms(),
            manifest.updated_epoch_ms(),
            manifest.index().clone(),
        )
        .expect("workspace");
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("write manifest");
        let mut original = serde_json::to_value(&manifest).expect("JSON");
        let key = crate::domain::model::EncryptedApiKey {
            nonce: vec![1; 24],
            ciphertext: vec![2; 32],
        };
        original["index"]["descriptor"]["embeddingConfigs"]["local/minilm"] =
            serde_json::to_value(ModelConfig {
                api_key: Some(key.clone()),
                endpoint: Some("https://example.test/embeddings".into()),
                device: Some(crate::domain::model::Device::Cpu),
                cache_dir: Some(directory.path().join("models")),
            })
            .expect("saved settings");
        for field in ["recordedPath", "recordedUsername"] {
            let mut changed = original.clone();
            changed["sourceRoot"][field] = serde_json::json!(format!("changed-{field}"));
            fs::write(layout.manifest_path(), changed.to_string()).expect("changed environment");
            let bytes = fs::read(layout.manifest_path()).expect("saved bytes");
            let loaded = read_workspace_manifest(directory.path(), &layout)
                .expect("load relocated metadata")
                .expect("manifest");
            let descriptor = loaded.index().descriptor().expect("enabled index");
            let config = &descriptor.embedding_configs()["local/minilm"];
            assert_eq!(config.device, None);
            assert_eq!(config.cache_dir, None);
            assert_eq!(config.api_key, Some(key.clone()));
            assert_eq!(
                config.endpoint.as_deref(),
                Some("https://example.test/embeddings")
            );
            assert_eq!(
                descriptor.default_embedding_model(),
                manifest
                    .index()
                    .descriptor()
                    .expect("original index")
                    .default_embedding_model()
            );
            assert_eq!(loaded.storage_generation(), manifest.storage_generation());
            assert_eq!(loaded.index_version(), manifest.index_version());
            assert_eq!(
                fs::read(layout.manifest_path()).expect("read-only load"),
                bytes
            );
            assert_eq!(loaded.scan(), manifest.scan());
            let policy =
                crate::file_selection::ScanPolicy::new(manifest.source_root(), loaded.scan())
                    .expect("external rules survive environment changes");
            assert!(
                !policy
                    .includes_file(&loaded.source_root().join("external.rs"))
                    .expect("external rule applies")
            );
            write_workspace_manifest(layout.manifest_path(), &loaded).expect("rewrite metadata");
            let reloaded = read_workspace_manifest(directory.path(), &layout)
                .expect("read again")
                .expect("manifest");
            assert_eq!(reloaded.scan(), manifest.scan());
            assert_eq!(
                serde_json::to_value(&reloaded).expect("JSON")["sourceRoot"],
                changed["sourceRoot"]
            );
        }
    }

    #[test]
    fn model_preferences_round_trip_without_changing_index_compatibility() {
        let directory = tempdir().expect("directory");
        let manifest = fixture_manifest(
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        fs::create_dir(manifest.layout().storage_dir()).expect("storage directory");
        let mut json = serde_json::to_value(&manifest).expect("JSON");
        json["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]["device"] =
            serde_json::json!("cpu");
        json["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]["cacheDir"] =
            serde_json::json!(directory.path().join("models"));
        let restored = decode_manifest(&json, manifest.layout()).expect("saved preferences");
        let mut expected = json.clone();
        if expected["sourceRoot"]["recordedUsername"].is_null() {
            let config = expected["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]
                .as_object_mut()
                .expect("saved configuration");
            config.remove("device");
            config.remove("cacheDir");
        }
        assert_eq!(serde_json::to_value(&restored).expect("JSON"), expected);
        manifest
            .index()
            .descriptor()
            .expect("index")
            .ensure_index_compatible(restored.index().descriptor().expect("index"))
            .expect("runtime preferences do not require rebuilding");
        json["index"]["descriptor"]["embeddingConfigs"]["local/minilm"]["apiKey"] =
            serde_json::json!("plaintext-secret");
        assert!(decode_manifest(&json, manifest.layout()).is_err());
    }

    #[test]
    fn rejects_other_index_versions_before_decoding_the_manifest_schema() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let storage_dir = layout.storage_dir().to_path_buf();
        fs::create_dir_all(&storage_dir).expect("workspace home");
        fs::write(layout.manifest_path(), r#"{"indexVersion":2}"#).expect("invalid manifest");
        assert!(read_workspace_manifest(directory.path(), &layout).is_err());
        let current = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &current).expect("current manifest");
        let bytes = fs::read(layout.manifest_path()).expect("persisted manifest");
        for version in [0, 1, 3, 4, 5, u32::MAX] {
            let mut unsupported = current.clone();
            unsupported = crate::domain::Workspace::new(
                directory.path(),
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
            let error = read_workspace_manifest(directory.path(), &layout)
                .expect_err("unsupported index version");
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
                read_workspace_manifest(directory.path(), &layout)
                    .expect_err("version required")
                    .message()
                    .contains("rebuild")
            );
            assert!(
                inspect_workspace_manifest(directory.path(), &layout)
                    .expect("inspect")
                    .into_workspace(true)
                    .expect("rebuild bypasses old data")
                    .is_none()
            );
        }
        fs::write(layout.manifest_path(), "{broken").expect("corrupt JSON");
        assert!(read_workspace_manifest(directory.path(), &layout).is_err());
        assert!(
            inspect_workspace_manifest(directory.path(), &layout)
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
            &WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout"),
        );
        for field in ["maxBatchSize", "maxInputTokens", "maxImageBytes"] {
            let mut json = serde_json::to_value(&manifest).expect("manifest JSON");
            if field == "maxBatchSize" {
                json["index"]["descriptor"]["defaultEmbeddingModel"]["model"][field] =
                    serde_json::json!(0);
            } else {
                json["index"]["descriptor"]["defaultEmbeddingModel"][field] = serde_json::json!(0);
            }
            assert!(
                decode_manifest(&json, manifest.layout()).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn deleting_a_missing_manifest_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("workspace directory");
        assert!(delete_workspace_manifest(&layout).is_ok());
        let missing = WorkspaceLayout::new(directory.path().join("missing").join(".zvec-grep"))
            .expect("layout");
        assert!(delete_workspace_manifest(&missing).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_operations_ignore_unrelated_storage_paths() {
        let directory = tempdir().expect("workspace");
        let external = tempdir().expect("external data");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
            read_workspace_manifest(directory.path(), &layout).expect("read metadata"),
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let manifest = fixture_manifest(&layout);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("initial manifest");
        let target = directory.path().join("saved-manifest.json");
        let path = layout.manifest_path().to_path_buf();
        fs::rename(&path, &target).expect("move manifest");
        let original = fs::read(&target).expect("saved bytes");
        std::os::unix::fs::symlink(&target, &path).expect("manifest alias");

        assert!(read_workspace_manifest(directory.path(), &layout).is_err());
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
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
            read_workspace_manifest(directory.path(), &layout).expect("updated manifest"),
            Some(manifest)
        );
    }
}
