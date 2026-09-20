//! Fresh workspace builds, published by a single atomic manifest update.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    EngineError,
    storage::spi::WorkspaceIndexStorageFactory,
    utils::{atomic_write, sync_directory},
};

use super::manifest::{WorkspaceManifest, read_workspace_manifest, write_workspace_manifest};

const BUILD_FILE: &str = "build.json";
const BUILD_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceBuild {
    version: u32,
    pub target: WorkspaceManifest,
    previous: Option<PreviousStorage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum PreviousStorage {
    Generation { id: String },
}

impl WorkspaceBuild {
    fn validate(&self) -> Result<(), EngineError> {
        self.target.validate()?;
        if self.version != BUILD_VERSION || self.target.storage_generation.is_none() {
            return Err(EngineError::storage_failure(
                "unsupported workspace build record",
            ));
        }
        if let Some(PreviousStorage::Generation { id }) = &self.previous
            && (Uuid::parse_str(id).is_err() || self.target.storage_generation.as_ref() == Some(id))
        {
            return Err(EngineError::storage_failure(
                "invalid previous build generation",
            ));
        }
        Ok(())
    }
}

pub(crate) fn has_build(home: &Path) -> bool {
    home.join(BUILD_FILE).is_file()
}

/// The manifest may have been lost while generation storage is still present.
/// An empty generations container is left behind by drop and is not an index.
pub(crate) fn has_generation_storage(home: &Path) -> Result<bool, EngineError> {
    let entries = match fs::read_dir(home.join("generations")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(build_io(error)),
    };
    for entry in entries {
        if entry
            .map_err(build_io)?
            .file_type()
            .map_err(build_io)?
            .is_dir()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove abandoned builds or finish post-publication cleanup under the write lock.
/// The active manifest is the only commit marker; unfinished work is never resumed.
pub(crate) fn recover_build(
    home: &Path,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    let Some(build) = read_build(home)? else {
        return Ok(());
    };
    let active = read_workspace_manifest(home)?;
    if active
        .as_ref()
        .is_some_and(|manifest| manifest.storage_generation == build.target.storage_generation)
    {
        cleanup_previous(home, &build, factory)?;
    } else {
        remove_generation(home, &build.target, factory)?;
    }
    remove_build(home)
}

/// Start with empty storage. The caller must clean up any previous build first.
pub(crate) fn prepare_build(
    mut target: WorkspaceManifest,
    active: Option<&WorkspaceManifest>,
) -> Result<WorkspaceBuild, EngineError> {
    target.storage_generation = Some(Uuid::new_v4().to_string());
    let previous = active
        .and_then(|manifest| manifest.storage_generation.as_ref())
        .map(|id| PreviousStorage::Generation { id: id.clone() });
    let build = WorkspaceBuild {
        version: BUILD_VERSION,
        target,
        previous,
    };
    // Record ownership before creating storage so a crash cannot orphan a stage.
    write_build(&build)?;
    ensure_stage(&build)?;
    Ok(build)
}

/// The caller has completed and closed stage storage before publishing.
pub(crate) fn publish_build(
    mut build: WorkspaceBuild,
    updated_time: u64,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    build.target.record_update(updated_time);
    write_workspace_manifest(&build.target.path, &build.target)?;
    // Publication succeeded. Cleanup failure must not turn it into a failed build;
    // keep the ownership record so the next writer can retry cleanup.
    let _ = recover_build(&build.target.path, factory);
    Ok(())
}

/// Drop also removes interrupted and superseded generations, under the home lock.
pub(crate) fn drop_build_storage(
    home: &Path,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    let generations = home.join("generations");
    match fs::read_dir(&generations) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(build_io)?;
                if !entry.file_type().map_err(build_io)?.is_dir() {
                    continue;
                }
                factory.delete(&entry.path())?;
                fs::remove_dir_all(entry.path()).map_err(build_io)?;
            }
            sync_directory(&generations)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(build_io(error)),
    }
    remove_build(home)
}

pub(crate) fn read_build(home: &Path) -> Result<Option<WorkspaceBuild>, EngineError> {
    let bytes = match fs::read(home.join(BUILD_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(build_io(error)),
    };
    let mut build: WorkspaceBuild = serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::storage_failure(format!("invalid workspace build record: {error}"))
    })?;
    build.validate()?;
    build.target.path = std::path::absolute(home).map_err(build_io)?;
    build.target.workspace.root = build
        .target
        .path
        .parent()
        .ok_or_else(|| EngineError::storage_failure("workspace home has no parent"))?
        .to_path_buf();
    Ok(Some(build))
}

fn write_build(build: &WorkspaceBuild) -> Result<(), EngineError> {
    build.validate()?;
    let bytes = serde_json::to_vec_pretty(build)
        .map_err(|error| EngineError::internal(format!("encode workspace build: {error}")))?;
    atomic_write(&build.target.path.join(BUILD_FILE), &bytes)
}

fn ensure_stage(build: &WorkspaceBuild) -> Result<(), EngineError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(build.target.storage_home())
        .map_err(build_io)?;
    sync_directory(&build.target.path.join("generations"))?;
    sync_directory(&build.target.path)
}

fn cleanup_previous(
    home: &Path,
    build: &WorkspaceBuild,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    match &build.previous {
        None => Ok(()),
        Some(PreviousStorage::Generation { id }) => {
            let mut previous = build.target.clone();
            previous.storage_generation = Some(id.clone());
            remove_generation(home, &previous, factory)
        }
    }
}

fn remove_generation(
    home: &Path,
    manifest: &WorkspaceManifest,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    let storage = manifest.storage_home();
    factory.delete(&storage)?;
    match fs::remove_dir_all(&storage) {
        Ok(()) => sync_directory(&home.join("generations")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(build_io(error)),
    }
}

fn remove_build(home: &Path) -> Result<(), EngineError> {
    match fs::remove_file(home.join(BUILD_FILE)) {
        Ok(()) => sync_directory(home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(build_io(error)),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a Result::map_err callback"
)]
fn build_io(error: std::io::Error) -> EngineError {
    EngineError::from_io("access workspace build state", &error)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::tempdir;

    use crate::{
        domain::{
            IndexDescriptor, IndexState, Workspace,
            model::{EmbeddingModelInfo, Metric, ModelConfig},
        },
        storage::spi::{StorageResult, WorkspaceIndexStorage, WorkspaceIndexStorageOptions},
    };

    use super::*;

    #[derive(Debug, Default)]
    struct TestFactory {
        fail_delete: AtomicBool,
    }

    impl WorkspaceIndexStorageFactory for TestFactory {
        fn open(
            &self,
            _options: WorkspaceIndexStorageOptions,
        ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
            Err(EngineError::unsupported(
                "build protocol test does not open native storage",
            ))
        }

        fn exists(&self, home: &Path) -> StorageResult<bool> {
            Ok(home.join("storage").is_dir())
        }

        fn delete(&self, home: &Path) -> StorageResult<()> {
            if self.fail_delete.load(Ordering::Relaxed) {
                return Err(EngineError::storage_failure("injected cleanup failure"));
            }
            match fs::remove_dir_all(home.join("storage")) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(build_io(error)),
            }
        }
    }

    fn manifest(root: &Path) -> WorkspaceManifest {
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "workspace".to_owned(),
                root: root.to_path_buf(),
                scan: crate::domain::ScanRules::default(),
                index: IndexState::Enabled(IndexDescriptor::single(EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".into(),
                        name: "example".into(),
                        endpoint: None,
                    },
                    dimension: 8,
                    metric: Metric::Cosine,
                    max_batch_size: 32,
                    max_input_tokens: None,
                    max_image_bytes: None,
                })),
                created_epoch_ms: 1,
                updated_epoch_ms: 2,
            },
            root.join(".zvec-grep"),
            Some(5),
            std::collections::BTreeMap::from([("local/example".into(), ModelConfig::default())]),
        )
        .expect("manifest");
        manifest.storage_generation = Some(Uuid::new_v4().to_string());
        manifest
    }

    fn write_active(manifest: &WorkspaceManifest) {
        fs::create_dir_all(manifest.storage_home().join("storage")).expect("active storage");
        fs::write(manifest.storage_home().join("storage/old"), "old data").expect("old data");
        write_workspace_manifest(&manifest.path, manifest).expect("active manifest");
    }

    #[test]
    fn interrupted_build_is_discarded_even_when_settings_match() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        let checkpoint = build.target.storage_home().join("checkpoint");
        fs::write(&checkpoint, "completed work").expect("checkpoint");
        recover_build(&active.path, &factory).expect("discard interrupted stage");
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        let fresh = prepare_build(active.clone(), Some(&active)).expect("fresh build");
        assert_ne!(
            fresh.target.storage_generation,
            build.target.storage_generation
        );
        assert!(!fresh.target.storage_home().join("checkpoint").exists());
        assert_eq!(
            read_workspace_manifest(&active.path).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage/old").exists());
    }

    #[test]
    fn completed_but_unpublished_storage_is_discarded() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("completed storage");
        recover_build(&active.path, &factory).expect("discard before manifest commit");
        assert!(!build.target.storage_home().exists());
        assert_eq!(
            read_workspace_manifest(&active.path).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage/old").exists());
    }

    #[test]
    fn failed_abandoned_build_cleanup_keeps_record_for_retry() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        factory.fail_delete.store(true, Ordering::Relaxed);
        recover_build(&active.path, &factory).expect_err("cleanup fails");
        assert!(has_build(&active.path));
        assert!(build.target.storage_home().exists());
        assert!(active.storage_home().join("storage/old").exists());
        factory.fail_delete.store(false, Ordering::Relaxed);
        recover_build(&active.path, &factory).expect("retry cleanup");
        assert!(!has_build(&active.path));
        assert!(!build.target.storage_home().exists());
        recover_build(&active.path, &factory).expect("idempotent cleanup");
    }

    #[test]
    fn publication_cleanup_failure_keeps_new_active_and_recovers_after_rename() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("completed new storage");
        factory.fail_delete.store(true, Ordering::Relaxed);
        publish_build(build.clone(), 3, &factory)
            .expect("cleanup failure does not undo successful publication");
        let mut committed = read_workspace_manifest(&active.path)
            .expect("manifest")
            .expect("active");
        assert_eq!(
            committed.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(committed.workspace.updated_epoch_ms, 3);
        assert_eq!(committed.workspace.name, active.workspace.name);
        assert_eq!(
            committed.workspace.created_epoch_ms,
            active.workspace.created_epoch_ms
        );
        assert!(active.storage_home().exists());
        assert!(has_build(&active.path));
        committed.workspace.name = "renamed".to_owned();
        write_workspace_manifest(&active.path, &committed).expect("rename after publication");
        factory.fail_delete.store(false, Ordering::Relaxed);
        recover_build(&active.path, &factory).expect("complete cleanup");
        assert!(!active.storage_home().exists());
        assert!(committed.storage_home().join("storage").is_dir());
        assert!(!has_build(&active.path));
        assert_eq!(
            read_workspace_manifest(&active.path).expect("active manifest"),
            Some(committed)
        );
    }

    #[test]
    fn completed_rebuild_switches_manifest_before_removing_previous_generation() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("new storage");
        publish_build(build, 3, &factory).expect("publish");
        let committed = read_workspace_manifest(&active.path)
            .expect("manifest")
            .expect("active");
        assert_eq!(committed.index_version, Some(5));
        assert_eq!(committed.workspace.updated_epoch_ms, 3);
        assert!(committed.storage_home().join("storage").is_dir());
        assert!(!active.storage_home().join("storage").exists());
        assert!(!has_build(&active.path));
    }

    #[test]
    fn moving_workspace_rebases_abandoned_storage_before_cleanup() {
        let directory = tempdir().expect("workspace");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("root");
        let active = manifest(&original);
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        let moved = directory.path().join("moved");
        fs::rename(original, &moved).expect("move");
        let home = moved.join(".zvec-grep");
        let pending = read_build(&home).expect("read").expect("abandoned build");
        assert_eq!(
            pending.target.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(pending.target.workspace.root, moved);
        assert!(pending.target.storage_home().is_dir());
        assert_eq!(pending.target.workspace.name, active.workspace.name);
        recover_build(&home, &factory).expect("discard moved stage");
        assert!(!pending.target.storage_home().exists());
        assert!(
            home.join("generations")
                .join(
                    active
                        .storage_generation
                        .as_ref()
                        .expect("active generation")
                )
                .join("storage/old")
                .exists()
        );
    }

    #[test]
    fn drop_removes_both_active_and_interrupted_generations() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active)).expect("stage");
        assert!(has_generation_storage(&active.path).expect("generations present"));
        drop_build_storage(&active.path, &factory).expect("drop generations");
        assert!(!active.storage_home().exists());
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        assert!(!has_generation_storage(&active.path).expect("empty generations container"));
        drop_build_storage(&active.path, &factory).expect("idempotent drop");
    }

    #[test]
    fn generation_storage_detection_keeps_io_errors() {
        let directory = tempdir().expect("workspace home");
        assert!(!has_generation_storage(directory.path()).expect("missing generations"));
        fs::write(directory.path().join("generations"), b"not a directory")
            .expect("invalid layout");
        assert!(has_generation_storage(directory.path()).is_err());
    }
}
