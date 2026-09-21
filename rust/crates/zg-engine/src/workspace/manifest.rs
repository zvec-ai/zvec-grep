use crate::domain::model::ModelConfig;
#[cfg(test)]
use crate::domain::model::{Device, Metric};
use crate::{
    EngineError,
    domain::{
        ContentKind, IndexDescriptor, IndexState, ScanRules, Workspace, model::EmbeddingModelInfo,
    },
    utils::{atomic_write, sync_directory},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(crate) const WORKSPACE_MANIFEST_FILE: &str = "manifest.json";

/// Disk metadata owns layout/versioning; domain workspace owns its logical state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "ManifestData", into = "ManifestData")]
pub(crate) struct WorkspaceManifest {
    pub workspace: Workspace,
    /// Root recorded on disk before resolving a moved workspace.
    pub recorded_root: PathBuf,
    pub path: PathBuf,
    pub index_version: Option<u32>,
    pub storage_generation: Option<String>,
    pub embedding_runtimes: BTreeMap<String, ModelConfig>,
}

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
    name: String,
    path: PathBuf,
    root: PathBuf,
    #[serde(default)]
    scan: ScanRules,
    index_policy: IndexPolicy,
    #[serde(default)]
    embeddings: Vec<EmbeddingModelInfo>,
    #[serde(default)]
    embedding_routes: BTreeMap<ContentKind, String>,
    index_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage_generation: Option<String>,
    created_time: u64,
    updated_time: u64,
    #[serde(default)]
    embedding_runtimes: BTreeMap<String, ModelConfig>,
}

impl TryFrom<ManifestData> for WorkspaceManifest {
    type Error = String;
    fn try_from(input: ManifestData) -> Result<Self, Self::Error> {
        let index = match input.index_policy {
            IndexPolicy::Disabled => IndexState::Disabled,
            IndexPolicy::Uninitialized => IndexState::Uninitialized,
            IndexPolicy::Enabled => IndexState::Enabled(IndexDescriptor {
                embeddings: input.embeddings,
                routes: input.embedding_routes,
                fts: crate::domain::FTS_CONFIG,
            }),
        };
        let manifest = Self {
            recorded_root: input.root.clone(),
            workspace: Workspace {
                name: input.name,
                root: input.root,
                scan: input.scan,
                index,
                created_epoch_ms: input.created_time,
                updated_epoch_ms: input.updated_time,
            },
            path: input.path,
            index_version: input.index_version,
            storage_generation: input.storage_generation,
            embedding_runtimes: input.embedding_runtimes,
        };
        manifest
            .validate_published()
            .map_err(|error| error.to_string())?;
        Ok(manifest)
    }
}

impl From<WorkspaceManifest> for ManifestData {
    fn from(manifest: WorkspaceManifest) -> Self {
        let workspace = manifest.workspace;
        Self {
            name: workspace.name,
            path: manifest.path,
            root: workspace.root,
            scan: workspace.scan,
            index_policy: match &workspace.index {
                IndexState::Uninitialized => IndexPolicy::Uninitialized,
                IndexState::Disabled => IndexPolicy::Disabled,
                IndexState::Enabled(_) => IndexPolicy::Enabled,
            },
            embeddings: workspace
                .index
                .descriptor()
                .map_or_else(Vec::new, |index| index.embeddings.clone()),
            embedding_routes: workspace
                .index
                .descriptor()
                .map_or_else(BTreeMap::new, |index| index.routes.clone()),
            index_version: manifest.index_version,
            storage_generation: manifest.storage_generation,
            created_time: workspace.created_epoch_ms,
            updated_time: workspace.updated_epoch_ms,
            embedding_runtimes: manifest.embedding_runtimes,
        }
    }
}

impl WorkspaceManifest {
    pub(crate) fn new(
        workspace: Workspace,
        home: PathBuf,
        index_version: Option<u32>,
        embedding_runtimes: BTreeMap<String, ModelConfig>,
    ) -> Result<Self, EngineError> {
        let manifest = Self {
            path: home,
            recorded_root: workspace.root.clone(),
            workspace,
            index_version,
            storage_generation: None,
            embedding_runtimes,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn storage_home(&self) -> PathBuf {
        self.storage_generation.as_ref().map_or_else(
            || self.path.clone(),
            |generation| self.path.join("generations").join(generation),
        )
    }

    pub(crate) fn embedding(&self) -> Option<&EmbeddingModelInfo> {
        self.workspace
            .index
            .descriptor()
            .and_then(|index| index.embeddings.first())
    }

    pub(crate) fn embeddings(&self) -> &[EmbeddingModelInfo] {
        self.workspace
            .index
            .descriptor()
            .map_or(&[], |index| index.embeddings.as_slice())
    }

    pub(crate) fn record_update(&mut self, updated_epoch_ms: u64) {
        self.workspace.updated_epoch_ms = updated_epoch_ms;
    }

    /// Persisted enabled indexes always select a generation; drafts may omit it.
    fn validate_published(&self) -> Result<(), EngineError> {
        self.validate()?;
        if self.workspace.index_enabled() && self.storage_generation.is_none() {
            return Err(invalid_manifest(
                "enabled workspace requires storageGeneration",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        if self.workspace.index_enabled() {
            require_current_index_version(self.index_version)?;
        } else if self.index_version.is_some() || self.storage_generation.is_some() {
            return Err(invalid_manifest(
                "an unbuilt workspace cannot select an index version or storage generation",
            ));
        }
        if let Some(generation) = &self.storage_generation
            && uuid::Uuid::parse_str(generation).is_err()
        {
            return Err(invalid_manifest("storageGeneration must be a UUID"));
        }
        if !self.path.is_absolute() {
            return Err(invalid_manifest("path must be an absolute workspace home"));
        }
        self.workspace
            .validate()
            .map_err(|error| invalid_manifest(error.message()))
    }
}

pub(crate) fn workspace_manifest_path(home: &Path) -> PathBuf {
    home.join(WORKSPACE_MANIFEST_FILE)
}

/// Inspect the format header without asking a current-format decoder to read an old index.
#[derive(Debug)]
pub(crate) enum ManifestState {
    Missing,
    Current(Box<WorkspaceManifest>),
    RebuildRequired {
        actual_version: Option<u32>,
        reason: String,
    },
}

impl ManifestState {
    /// Explicit rebuild uses current configuration only when it is readable.
    /// Incompatible data is never decoded or migrated to recover old settings.
    pub(crate) fn into_manifest(
        self,
        rebuild: bool,
    ) -> Result<Option<WorkspaceManifest>, EngineError> {
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
        "{reason}; rebuild the index with `zg index --rebuild`"
    ))
}

pub(crate) fn inspect_workspace_manifest(home: &Path) -> Result<ManifestState, EngineError> {
    let path = workspace_manifest_path(home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestState::Missing);
        }
        Err(error) => return Err(manifest_io("read", &path, &error)),
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
    let mut manifest: WorkspaceManifest = match serde_json::from_value(value) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Ok(ManifestState::RebuildRequired {
                actual_version: version,
                reason: format!("invalid workspace manifest: {error}"),
            });
        }
    };
    // The directory containing the manifest defines the workspace location.
    // Persisted absolute paths may refer to where the workspace lived before a move.
    manifest.path = std::path::absolute(home)
        .map_err(|error| manifest_io("resolve directory for", home, &error))?;
    manifest.workspace.root = manifest
        .path
        .parent()
        .ok_or_else(|| invalid_manifest("workspace home has no parent directory"))?
        .to_path_buf();
    Ok(ManifestState::Current(Box::new(manifest)))
}

pub(crate) fn read_workspace_manifest(
    home: &Path,
) -> Result<Option<WorkspaceManifest>, EngineError> {
    inspect_workspace_manifest(home)?.into_manifest(false)
}

pub(crate) fn write_workspace_manifest(
    home: &Path,
    manifest: &WorkspaceManifest,
) -> Result<(), EngineError> {
    manifest.validate_published()?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(home)
        && !(error.kind() == std::io::ErrorKind::AlreadyExists && home.is_dir())
    {
        return Err(manifest_io("create directory for", home, &error));
    }
    let path = workspace_manifest_path(home);
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        EngineError::internal(format!("failed to encode workspace manifest: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_write(&path, &bytes)?;
    if home.file_name().is_some() {
        sync_directory(home.parent().unwrap_or(home))?;
    }
    Ok(())
}

pub(crate) fn delete_workspace_manifest(home: &Path) -> Result<(), EngineError> {
    let path = workspace_manifest_path(home);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(manifest_io("delete", &path, &error)),
    }
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
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "fixture".to_owned(),
                root: home.parent().expect("workspace root").to_path_buf(),
                scan: crate::domain::ScanRules {
                    globs: vec!["*.rs".into()],
                    ..crate::domain::ScanRules::default()
                },
                index: IndexState::Enabled(IndexDescriptor::single(EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".into(),
                        name: "minilm".into(),
                        endpoint: Some("https://models.example.test/embeddings".into()),
                    },
                    dimension: 384,
                    metric: Metric::Cosine,
                    max_batch_size: 32,
                    max_input_tokens: Some(8192),
                    max_image_bytes: Some(1_048_576),
                })),
                created_epoch_ms: 10,
                updated_epoch_ms: 20,
            },
            home.to_path_buf(),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            BTreeMap::from([(
                "local/minilm".into(),
                ModelConfig {
                    device: Some(Device::Cpu),
                    cache_dir: Some(home.join("models")),
                    ..ModelConfig::default()
                },
            )]),
        )
        .expect("fixture manifest");
        manifest.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        manifest
    }

    #[test]
    fn enabled_manifests_require_a_generation_at_persistence_boundaries() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let mut draft = fixture_manifest(&home);
        draft.storage_generation = None;
        draft
            .validate()
            .expect("draft may await generation allocation");
        let error = write_workspace_manifest(&home, &draft)
            .expect_err("draft must not be published without a generation");
        assert!(error.message().contains("storageGeneration"));
        assert!(!workspace_manifest_path(&home).exists());
        let value = serde_json::to_value(&draft).expect("draft JSON");
        assert!(value.get("storageGeneration").is_none());
        for null_generation in [false, true] {
            let mut missing_generation = value.clone();
            if null_generation {
                missing_generation["storageGeneration"] = serde_json::Value::Null;
            }
            let error = serde_json::from_value::<WorkspaceManifest>(missing_generation.clone())
                .expect_err("enabled manifest requires a generation");
            assert!(error.to_string().contains("storageGeneration"));
        }
        fs::create_dir(&home).expect("workspace home");
        let build = crate::workspace::build::prepare_build(draft).expect("allocate generation");
        assert!(build.target.storage_generation.is_some());
        assert!(build.target.storage_home().is_dir());
        write_workspace_manifest(&home, &build.target).expect("publish allocated generation");
        assert_eq!(
            read_workspace_manifest(&home).expect("read manifest"),
            Some(build.target)
        );
    }

    #[test]
    fn manifest_requires_current_fields_without_migration_aliases() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let value = serde_json::to_value(&manifest).expect("manifest");
        for (field, extra) in [
            ("id", serde_json::json!(uuid::Uuid::new_v4())),
            (
                "rootPaths",
                serde_json::json!([{"absolutePath": directory.path(), "recursive": true}]),
            ),
            ("embedding", value["embeddings"][0].clone()),
            (
                "embeddingRuntime",
                value["embeddingRuntimes"]["local/minilm"].clone(),
            ),
        ] {
            let mut unsupported = value.clone();
            unsupported[field] = extra;
            let error = serde_json::from_value::<WorkspaceManifest>(unsupported)
                .expect_err("unsupported manifest field");
            assert!(error.to_string().contains(field));
        }
        let mut missing_root = value;
        missing_root
            .as_object_mut()
            .expect("manifest object")
            .remove("root");
        let error = serde_json::from_value::<WorkspaceManifest>(missing_root)
            .expect_err("root is required");
        assert!(error.to_string().contains("root"));
    }

    #[test]
    fn manifest_rejects_multiple_models_and_nontext_routes() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let value = serde_json::to_value(&manifest).expect("manifest");
        let mut multiple = value.clone();
        let mut second = multiple["embeddings"][0].clone();
        second["model"]["name"] = serde_json::json!("second-model");
        multiple["embeddings"]
            .as_array_mut()
            .expect("embeddings")
            .push(second);
        assert!(serde_json::from_value::<WorkspaceManifest>(multiple).is_err());
        for kind in ["image", "table"] {
            let mut nontext = value.clone();
            nontext["embeddingRoutes"][kind] = serde_json::json!("local/minilm");
            assert!(serde_json::from_value::<WorkspaceManifest>(nontext).is_err());
        }
    }

    #[test]
    fn enabled_manifest_requires_a_descriptor_and_does_not_persist_runtime_status() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let mut json = serde_json::to_value(&manifest).expect("serialize");
        json["embeddings"] = serde_json::json!([]);
        assert!(serde_json::from_value::<WorkspaceManifest>(json).is_err());
        for index in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            manifest.workspace.index.clone(),
        ] {
            let mut manifest = manifest.clone();
            manifest.workspace.index = index;
            if !manifest.workspace.index_enabled() {
                manifest.index_version = None;
                manifest.storage_generation = None;
            }
            let json = serde_json::to_value(&manifest).expect("serialize");
            assert!(json.get("status").is_none());
            assert!(json.get("indexStatus").is_none());
            assert_eq!(
                serde_json::from_value::<WorkspaceManifest>(json).expect("deserialize"),
                manifest
            );
        }
    }

    #[test]
    fn preserves_the_resolved_workspace_home() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join("custom-index");
        let mut manifest = fixture_manifest(&home);

        assert_eq!(manifest.path, home);
        assert_eq!(manifest.workspace.root, directory.path());
        let generation = uuid::Uuid::new_v4().to_string();
        manifest.storage_generation = Some(generation.clone());
        assert_eq!(
            manifest.storage_home(),
            home.join("generations").join(generation)
        );

        write_workspace_manifest(&home, &manifest).expect("persist at resolved home");
        assert_eq!(
            read_workspace_manifest(&home).expect("read resolved home"),
            Some(manifest)
        );
        assert!(!directory.path().join(".zvec-grep").exists());
    }

    #[test]
    fn writes_and_reads_a_single_workspace_root() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let manifest = fixture_manifest(&home);

        write_workspace_manifest(&home, &manifest).expect("write manifest");
        let text = fs::read_to_string(workspace_manifest_path(&home)).expect("manifest text");
        let json: serde_json::Value = serde_json::from_str(&text).expect("manifest json");

        assert!(json.get("manifestVersion").is_none());
        assert_eq!(
            json["indexVersion"],
            crate::workspace::CURRENT_INDEX_VERSION
        );
        assert!(json.get("id").is_none());
        assert!(json.get("rootPaths").is_none());
        assert_eq!(json["root"], directory.path().to_string_lossy().as_ref());
        assert_eq!(json["scan"]["globs"][0]["pattern"], "*.rs");
        assert_eq!(json["embeddingRuntimes"]["local/minilm"]["device"], "cpu");
        assert!(json.get("generation").is_none());
        assert_eq!(
            json["embeddingRuntimes"]["local/minilm"]["cacheDir"],
            home.join("models").to_string_lossy().as_ref()
        );
        assert_eq!(
            read_workspace_manifest(&home).expect("read manifest"),
            Some(manifest)
        );
        let mut without_cache = json;
        without_cache["embeddingRuntimes"]["local/minilm"]
            .as_object_mut()
            .expect("runtime object")
            .remove("cacheDir");
        let without_cache: WorkspaceManifest =
            serde_json::from_value(without_cache).expect("optional runtime cache");
        assert_eq!(
            without_cache.embedding_runtimes["local/minilm"].cache_dir,
            None
        );
    }

    #[test]
    fn removed_selection_fields_are_rejected_instead_of_widening_scan_scope() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let value = serde_json::to_value(&manifest).expect("manifest JSON");
        let mut split_filter = value.clone();
        split_filter["filter"] = serde_json::json!({"globs": ["*.rs"]});
        assert!(serde_json::from_value::<WorkspaceManifest>(split_filter).is_err());
        let mut query_constraint = value;
        query_constraint["scan"]["formats"] = serde_json::json!(["rust"]);
        assert!(serde_json::from_value::<WorkspaceManifest>(query_constraint).is_err());
    }

    #[test]
    fn reading_a_moved_workspace_rebases_only_its_location() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        fs::create_dir(&original).expect("original workspace");
        let mut manifest = fixture_manifest(&original.join(".zvec-grep"));
        write_workspace_manifest(&manifest.path, &manifest).expect("original manifest");
        fs::rename(&original, &moved).expect("move workspace");
        let relocated = read_workspace_manifest(&moved.join(".zvec-grep"))
            .expect("relocated manifest read")
            .expect("manifest");
        manifest.workspace.root = moved.clone();
        manifest.path = moved.join(".zvec-grep");
        assert_eq!(relocated, manifest);
    }

    #[test]
    fn string_names_are_validated_on_manifest_read_and_write() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        write_workspace_manifest(&home, &manifest).expect("valid manifest");
        let original = fs::read(workspace_manifest_path(&home)).expect("original manifest");
        for name in ["", " ", " project", ".", "..", "a/b", "a\\b", "a\nb"] {
            manifest.workspace.name = name.to_owned();
            assert!(write_workspace_manifest(&home, &manifest).is_err());
            assert_eq!(
                fs::read(workspace_manifest_path(&home)).expect("preserved"),
                original
            );
            // Raw JSON can bypass in-memory validation; reads must still reject it.
            let json = serde_json::to_value(&manifest).expect("raw manifest");
            assert!(serde_json::from_value::<WorkspaceManifest>(json).is_err());
        }
    }

    #[test]
    fn rejects_other_index_versions_before_decoding_the_manifest_schema() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(workspace_manifest_path(&home), r#"{"indexVersion":2}"#)
            .expect("invalid manifest");
        assert!(read_workspace_manifest(&home).is_err());
        let current = fixture_manifest(&home);
        write_workspace_manifest(&home, &current).expect("current manifest");
        let bytes = fs::read(workspace_manifest_path(&home)).expect("persisted manifest");
        for version in [0, 1, 3, 4, 5, u32::MAX] {
            let mut unsupported = current.clone();
            unsupported.index_version = Some(version);
            assert!(write_workspace_manifest(&home, &unsupported).is_err());
            assert_eq!(
                fs::read(workspace_manifest_path(&home)).expect("preserved manifest"),
                bytes
            );
            fs::write(
                workspace_manifest_path(&home),
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
            let error = read_workspace_manifest(&home).expect_err("unsupported index version");
            assert!(
                error
                    .message()
                    .contains(&format!("unsupported index version {version}"))
            );
            assert!(error.message().contains("expected 2"));
            assert!(error.message().contains("zg index --rebuild"));
            fs::write(workspace_manifest_path(&home), &bytes).expect("restore current manifest");
        }
    }

    #[test]
    fn enabled_indexes_cannot_omit_the_version_and_rebuild_can_replace_invalid_metadata() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        write_workspace_manifest(&home, &manifest).expect("current manifest");
        manifest.index_version = None;
        assert!(write_workspace_manifest(&home, &manifest).is_err());
        let value = serde_json::to_value(&manifest).expect("manifest JSON");
        for missing in [false, true] {
            let mut invalid = value.clone();
            if missing {
                invalid
                    .as_object_mut()
                    .expect("object")
                    .remove("indexVersion");
            }
            fs::write(workspace_manifest_path(&home), invalid.to_string()).expect("raw manifest");
            assert!(
                read_workspace_manifest(&home)
                    .expect_err("version required")
                    .message()
                    .contains("rebuild")
            );
            assert!(
                inspect_workspace_manifest(&home)
                    .expect("inspect")
                    .into_manifest(true)
                    .expect("rebuild bypasses old data")
                    .is_none()
            );
        }
        fs::write(workspace_manifest_path(&home), "{broken").expect("corrupt JSON");
        assert!(read_workspace_manifest(&home).is_err());
        assert!(
            inspect_workspace_manifest(&home)
                .expect("inspect")
                .into_manifest(true)
                .expect("rebuild bypasses corrupt data")
                .is_none()
        );
    }

    #[test]
    fn rejects_invalid_embedding_limits_read_from_manifest() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        for field in ["maxBatchSize", "maxInputTokens", "maxImageBytes"] {
            let mut json = serde_json::to_value(&manifest).expect("manifest JSON");
            json["embeddings"][0][field] = serde_json::json!(0);
            assert!(
                serde_json::from_value::<WorkspaceManifest>(json).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn deleting_a_missing_manifest_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        assert!(delete_workspace_manifest(directory.path()).is_ok());
        assert!(delete_workspace_manifest(&directory.path().join("missing")).is_ok());
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

    #[cfg(unix)]
    #[test]
    fn replacing_manifest_preserves_file_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        write_workspace_manifest(&home, &manifest).expect("initial manifest");
        let path = workspace_manifest_path(&home);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o750))
            .expect("custom workspace permissions");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("custom manifest permissions");

        manifest.record_update(30);
        write_workspace_manifest(&home, &manifest).expect("replace manifest");

        assert_eq!(
            fs::metadata(&home)
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
            read_workspace_manifest(&home).expect("updated manifest"),
            Some(manifest)
        );
    }
}
