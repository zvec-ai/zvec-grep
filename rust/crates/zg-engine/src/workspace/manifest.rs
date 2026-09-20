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
pub(crate) const CURRENT_MANIFEST_VERSION: u32 = 5;

/// Disk metadata owns layout/versioning; domain workspace owns its logical state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "ManifestData", into = "ManifestData")]
pub(crate) struct WorkspaceManifest {
    pub manifest_version: u32,
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
    manifest_version: u32,
    // Older manifests carried a workspace UUID; names now provide identity.
    #[serde(default, rename = "id", skip_serializing)]
    _legacy_id: Option<serde::de::IgnoredAny>,
    name: String,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    #[serde(default)]
    scan: ScanRules,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_paths: Option<Vec<LegacyRootPath>>,
    index_policy: IndexPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding: Option<EmbeddingModelInfo>,
    #[serde(default)]
    embeddings: Vec<EmbeddingModelInfo>,
    #[serde(default)]
    embedding_routes: BTreeMap<ContentKind, String>,
    index_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage_generation: Option<String>,
    created_time: u64,
    updated_time: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embedding_runtime: Option<ModelConfig>,
    #[serde(default)]
    embedding_runtimes: BTreeMap<String, ModelConfig>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRootPath {
    absolute_path: PathBuf,
    recursive: bool,
}

impl TryFrom<ManifestData> for WorkspaceManifest {
    type Error = String;
    fn try_from(input: ManifestData) -> Result<Self, Self::Error> {
        let mut scan = input.scan;
        let root = match (input.root, input.root_paths) {
            (Some(root), None) => root,
            (None, Some(mut roots)) => {
                if roots.len() != 1 {
                    return Err("legacy rootPaths must contain exactly one workspace root; multiple-root workspaces are unsupported".into());
                }
                let root = roots.remove(0);
                if input.path.parent() != Some(root.absolute_path.as_path()) {
                    return Err("legacy source root differs from the workspace directory; recreate the index with one workspace root".into());
                }
                if !root.recursive {
                    scan.max_depth = Some(scan.max_depth.unwrap_or(1).min(1));
                }
                root.absolute_path
            }
            (Some(_), Some(_)) => return Err("specify root or legacy rootPaths, not both".into()),
            (None, None) => return Err("workspace root is missing".into()),
        };
        if input.embedding.is_some() && !input.embeddings.is_empty() {
            return Err("specify embeddings or legacy embedding, not both".into());
        }
        let index = if input.embeddings.is_empty() {
            input.embedding.map(IndexDescriptor::single)
        } else {
            Some(IndexDescriptor {
                embeddings: input.embeddings,
                routes: input.embedding_routes,
                fts: crate::domain::FTS_CONFIG,
            })
        };
        let mut embedding_runtimes = input.embedding_runtimes;
        if let Some(runtime) = input.embedding_runtime
            && let Some(index) = &index
        {
            for embedding in &index.embeddings {
                embedding_runtimes
                    .entry(embedding.model.reference())
                    .or_insert_with(|| runtime.clone());
            }
        }
        let index = match input.index_policy {
            IndexPolicy::Disabled => IndexState::Disabled,
            IndexPolicy::Uninitialized => IndexState::Uninitialized,
            IndexPolicy::Enabled => {
                IndexState::Enabled(index.ok_or("enabled workspace requires an index descriptor")?)
            }
        };
        let manifest = Self {
            manifest_version: input.manifest_version,
            recorded_root: root.clone(),
            workspace: Workspace {
                name: input.name,
                root,
                scan,
                index,
                created_epoch_ms: input.created_time,
                updated_epoch_ms: input.updated_time,
            },
            path: input.path,
            index_version: input.index_version,
            storage_generation: input.storage_generation,
            embedding_runtimes,
        };
        manifest.validate().map_err(|error| error.to_string())?;
        Ok(manifest)
    }
}

impl From<WorkspaceManifest> for ManifestData {
    fn from(manifest: WorkspaceManifest) -> Self {
        let workspace = manifest.workspace;
        Self {
            manifest_version: manifest.manifest_version,
            _legacy_id: None,
            name: workspace.name,
            path: manifest.path,
            root: Some(workspace.root),
            scan: workspace.scan,
            root_paths: None,
            index_policy: match &workspace.index {
                IndexState::Uninitialized => IndexPolicy::Uninitialized,
                IndexState::Disabled => IndexPolicy::Disabled,
                IndexState::Enabled(_) => IndexPolicy::Enabled,
            },
            embedding: None,
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
            embedding_runtime: None,
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
            manifest_version: CURRENT_MANIFEST_VERSION,
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
        self.manifest_version = CURRENT_MANIFEST_VERSION;
    }

    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        if !matches!(
            self.manifest_version,
            1 | 2 | 3 | 4 | CURRENT_MANIFEST_VERSION
        ) {
            return Err(invalid_manifest(format!(
                "unsupported manifestVersion {}",
                self.manifest_version
            )));
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

pub(crate) fn read_workspace_manifest(
    home: &Path,
) -> Result<Option<WorkspaceManifest>, EngineError> {
    let path = workspace_manifest_path(home);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(manifest_io("read", &path, &error)),
    };
    let mut manifest: WorkspaceManifest = serde_json::from_str(&text)
        .map_err(|error| invalid_manifest(format!("path={} cause={error}", path.display())))?;
    manifest.validate()?;
    // The directory containing the manifest defines the workspace location.
    // Persisted absolute paths may refer to where the workspace lived before a move.
    manifest.path = std::path::absolute(home)
        .map_err(|error| manifest_io("resolve directory for", home, &error))?;
    manifest.workspace.root = manifest
        .path
        .parent()
        .ok_or_else(|| invalid_manifest("workspace home has no parent directory"))?
        .to_path_buf();
    Ok(Some(manifest))
}

pub(crate) fn write_workspace_manifest(
    home: &Path,
    manifest: &WorkspaceManifest,
) -> Result<(), EngineError> {
    manifest.validate()?;
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
        WorkspaceManifest::new(
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
            Some(1),
            BTreeMap::from([(
                "local/minilm".into(),
                ModelConfig {
                    device: Some(Device::Cpu),
                    cache_dir: Some(home.join("models")),
                    ..ModelConfig::default()
                },
            )]),
        )
        .expect("fixture manifest")
    }

    #[test]
    fn legacy_single_model_metadata_normalizes_into_explicit_routes() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let mut value = serde_json::to_value(&manifest).expect("manifest");
        value["manifestVersion"] = serde_json::json!(4);
        value["embedding"] = value["embeddings"][0].clone();
        value["embeddingRuntime"] = value["embeddingRuntimes"]["local/minilm"].clone();
        for key in ["embeddings", "embeddingRoutes", "embeddingRuntimes"] {
            value.as_object_mut().expect("object").remove(key);
        }
        let restored: WorkspaceManifest = serde_json::from_value(value).expect("legacy manifest");
        assert_eq!(restored.workspace.index, manifest.workspace.index);
        assert_eq!(restored.embedding_runtimes, manifest.embedding_runtimes);
    }

    #[test]
    fn enabled_manifest_requires_a_descriptor_and_does_not_persist_runtime_status() {
        let directory = tempdir().expect("workspace");
        let mut manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let mut json = serde_json::to_value(&manifest).expect("serialize");
        json["embeddings"] = serde_json::json!([]);
        assert!(serde_json::from_value::<WorkspaceManifest>(json).is_err());
        for index in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            manifest.workspace.index.clone(),
        ] {
            manifest.workspace.index = index;
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
        assert_eq!(manifest.storage_home(), home);
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

        assert_eq!(json["manifestVersion"], CURRENT_MANIFEST_VERSION);
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
    fn legacy_single_root_keeps_metadata_available_for_rebuild() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        manifest.manifest_version = 1;
        let mut json = serde_json::to_value(&manifest).expect("manifest json");
        let root = json
            .as_object_mut()
            .expect("manifest object")
            .remove("root")
            .expect("root");
        let discovery = serde_json::json!({"absolutePath":root,"recursive":true});
        json["rootPaths"] = serde_json::json!([discovery.clone()]);
        json["id"] = serde_json::json!(uuid::Uuid::new_v4());
        fs::create_dir(&home).expect("workspace home");
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&json).expect("legacy json"),
        )
        .expect("legacy manifest");
        let legacy = read_workspace_manifest(&home)
            .expect("legacy read")
            .expect("manifest");
        assert_eq!(legacy, manifest);
        assert!(
            serde_json::to_value(&legacy)
                .expect("current JSON")
                .get("id")
                .is_none()
        );

        for version in 1..=CURRENT_MANIFEST_VERSION {
            let mut supported = json.clone();
            supported["manifestVersion"] = version.into();
            assert!(serde_json::from_value::<WorkspaceManifest>(supported).is_ok());
        }

        let mut subtree = json.clone();
        subtree["rootPaths"][0]["absolutePath"] = serde_json::json!(directory.path().join("src"));
        let error = serde_json::from_value::<WorkspaceManifest>(subtree)
            .expect_err("legacy source scope cannot be widened to the workspace");
        assert!(error.to_string().contains("legacy source root differs"));

        let mut shallow = json.clone();
        shallow["rootPaths"][0]["recursive"] = false.into();
        let shallow: WorkspaceManifest =
            serde_json::from_value(shallow).expect("nonrecursive legacy workspace");
        assert_eq!(shallow.workspace.scan.max_depth, Some(1));

        json["rootPaths"] = serde_json::json!([discovery.clone(), discovery]);
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&json).expect("legacy json"),
        )
        .expect("multiple legacy roots");
        let error = read_workspace_manifest(&home).expect_err("multiple roots are unsupported");
        assert!(error.message().contains("exactly one workspace root"));
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
    fn rejects_invalid_or_unsupported_manifests() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(workspace_manifest_path(&home), r#"{"manifestVersion":2}"#)
            .expect("invalid manifest");
        assert!(read_workspace_manifest(&home).is_err());
        let mut unsupported = fixture_manifest(&home);
        unsupported.manifest_version = CURRENT_MANIFEST_VERSION + 1;
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&unsupported).expect("unsupported manifest json"),
        )
        .expect("unsupported manifest");
        let error = read_workspace_manifest(&home).expect_err("unsupported manifest version");
        assert!(error.message().contains("unsupported manifestVersion"));
    }

    #[test]
    fn rejects_invalid_embedding_limits_read_from_manifest() {
        let directory = tempdir().expect("workspace");
        let manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        for field in ["maxBatchSize", "maxInputTokens", "maxImageBytes"] {
            let mut json = serde_json::to_value(&manifest).expect("manifest JSON");
            json["embedding"][field] = serde_json::json!(0);
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
