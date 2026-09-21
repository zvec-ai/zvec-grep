//! Fresh workspace builds, published by a single atomic manifest update.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    EngineError,
    storage::IndexStore,
    utils::{atomic_write, sync_directory},
};

use super::manifest::{WORKSPACE_MANIFEST_FILE, WorkspaceManifest, write_workspace_manifest};

const BUILD_FILE: &str = "build.json";

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceBuild {
    pub target: WorkspaceManifest,
    previous_manifest: Option<Vec<u8>>,
}

/// Recovery needs ownership and registration only, never an index schema.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildRecord {
    generation: String,
    name: String,
    root: PathBuf,
}

impl BuildRecord {
    fn validate(&self) -> Result<(), EngineError> {
        if Uuid::parse_str(&self.generation).is_err() || !self.root.is_absolute() {
            return Err(EngineError::storage_failure(
                "invalid workspace build ownership record",
            ));
        }
        crate::domain::Workspace::validate_name(&self.name)
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
pub(crate) fn recover_build(home: &Path) -> Result<(), EngineError> {
    recover_build_with(home, false, &IndexStore::delete)
}

/// Explicit rebuild can replace unreadable ownership metadata, but must retain
/// all existing data until its new manifest has been successfully published.
pub(crate) fn recover_build_for_rebuild(home: &Path) -> Result<(), EngineError> {
    recover_build_with(home, true, &IndexStore::delete)
}

fn recover_build_with(
    home: &Path,
    discard_invalid: bool,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let Some(bytes) = read_optional(&home.join(BUILD_FILE))? else {
        return Ok(());
    };
    let build = match decode_build(&bytes) {
        Ok(build) => build,
        Err(_) if discard_invalid => return remove_build(home),
        Err(error) => return Err(error),
    };
    let active_bytes = read_optional(&home.join(WORKSPACE_MANIFEST_FILE))?;
    let active = match active_bytes.as_deref().map(active_generation).transpose() {
        Ok(active) => active.flatten(),
        Err(_) if discard_invalid => return remove_build(home),
        Err(error) => return Err(error),
    };
    if active.as_deref() == Some(&build.generation) {
        cleanup_generations(home, Some(&build.generation), delete_storage)?;
        cleanup_node_storage(home)?;
    } else {
        remove_generation(home, &build.generation, delete_storage)?;
    }
    remove_build(home)
}

/// Start with empty storage. The caller must clean up any previous build first.
pub(crate) fn prepare_build(mut target: WorkspaceManifest) -> Result<WorkspaceBuild, EngineError> {
    let previous_manifest = read_optional(&target.path.join(WORKSPACE_MANIFEST_FILE))?;
    target.storage_generation = Some(Uuid::new_v4().to_string());
    let build = WorkspaceBuild {
        target,
        previous_manifest,
    };
    // Record ownership before creating storage so a crash cannot orphan a stage.
    if let Err(error) = write_build(&build).and_then(|()| ensure_stage(&build)) {
        let _ = discard_build(&build);
        return Err(error);
    }
    Ok(build)
}

/// Discard this attempt after its storage handles close. An unreadable original
/// manifest is safe to retain unchanged; a published target must never be deleted.
pub(crate) fn discard_build(build: &WorkspaceBuild) -> Result<(), EngineError> {
    let home = &build.target.path;
    let generation = build
        .target
        .storage_generation
        .as_deref()
        .ok_or_else(|| EngineError::storage_failure("workspace build requires a generation"))?;
    let active_bytes = read_optional(&home.join(WORKSPACE_MANIFEST_FILE))?;
    match active_bytes.as_deref().map(active_generation).transpose() {
        Ok(Some(Some(active))) if active == generation => return Ok(()),
        Err(error) if active_bytes != build.previous_manifest => return Err(error),
        _ => {}
    }
    remove_generation(home, generation, &IndexStore::delete)?;
    if let Some(bytes) = read_optional(&home.join(BUILD_FILE))?
        && decode_build(&bytes).is_ok_and(|record| record.generation == generation)
    {
        remove_build(home)?;
    }
    Ok(())
}

/// The caller has completed and closed stage storage before publishing.
pub(crate) fn publish_build(build: WorkspaceBuild, updated_time: u64) -> Result<(), EngineError> {
    publish_build_with(build, updated_time, &IndexStore::delete)
}

fn publish_build_with(
    mut build: WorkspaceBuild,
    updated_time: u64,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    build.target.record_update(updated_time);
    write_workspace_manifest(&build.target.path, &build.target)?;
    // Publication succeeded. Cleanup failure must not turn it into a failed build;
    // keep the ownership record so the next writer can retry cleanup.
    let _ = recover_build_with(&build.target.path, false, delete_storage);
    Ok(())
}

/// Drop also removes interrupted and superseded generations, under the home lock.
pub(crate) fn drop_build_storage(home: &Path) -> Result<(), EngineError> {
    cleanup_generations(home, None, &IndexStore::delete)?;
    cleanup_node_storage(home)?;
    remove_build(home)
}

fn cleanup_generations(
    home: &Path,
    keep: Option<&str>,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let generations = home.join("generations");
    match fs::read_dir(&generations) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(build_io)?;
                if !entry.file_type().map_err(build_io)?.is_dir() {
                    continue;
                }
                if keep.is_some_and(|generation| entry.file_name() == generation) {
                    continue;
                }
                delete_storage(&entry.path())?;
                fs::remove_dir_all(entry.path()).map_err(build_io)?;
            }
            sync_directory(&generations)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(build_io(error)),
    }
    Ok(())
}

pub(crate) fn read_build_registration(
    home: &Path,
) -> Result<Option<(String, PathBuf)>, EngineError> {
    read_optional(&home.join(BUILD_FILE))?
        .as_deref()
        .map(decode_build)
        .transpose()
        .map(|build| build.map(|build| (build.name, build.root)))
}

fn decode_build(bytes: &[u8]) -> Result<BuildRecord, EngineError> {
    let build: BuildRecord = serde_json::from_slice(bytes).map_err(|error| {
        EngineError::storage_failure(format!("invalid workspace build record: {error}"))
    })?;
    build.validate()?;
    Ok(build)
}

fn active_generation(bytes: &[u8]) -> Result<Option<String>, EngineError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CommitMarker {
        storage_generation: Option<String>,
    }

    let marker: CommitMarker = serde_json::from_slice(bytes).map_err(|error| {
        EngineError::storage_failure(format!("invalid workspace commit marker: {error}"))
    })?;
    if let Some(generation) = &marker.storage_generation
        && Uuid::parse_str(generation).is_err()
    {
        return Err(EngineError::storage_failure(
            "invalid active workspace generation",
        ));
    }
    Ok(marker.storage_generation)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(build_io(error)),
    }
}

fn write_build(build: &WorkspaceBuild) -> Result<(), EngineError> {
    build.target.validate()?;
    let generation = build
        .target
        .storage_generation
        .clone()
        .ok_or_else(|| EngineError::storage_failure("workspace build requires a generation"))?;
    let record = BuildRecord {
        generation,
        name: build.target.workspace.name.clone(),
        root: build.target.workspace.root.clone(),
    };
    record.validate()?;
    let bytes = serde_json::to_vec_pretty(&record)
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

fn cleanup_node_storage(home: &Path) -> Result<(), EngineError> {
    let mut removed = false;
    for name in ["files.zvec", "index.zvec"] {
        let path = home.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    fs::remove_dir_all(&path).map_err(build_io)?;
                } else {
                    fs::remove_file(&path).map_err(build_io)?;
                }
                removed = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(build_io(error)),
        }
    }
    if removed {
        sync_directory(home)?;
    }
    Ok(())
}

fn remove_generation(
    home: &Path,
    generation: &str,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let storage = home.join("generations").join(generation);
    delete_storage(&storage)?;
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
    use tempfile::tempdir;

    use crate::domain::{
        IndexDescriptor, IndexState, Workspace,
        model::{EmbeddingModelInfo, Metric, ModelConfig},
    };
    use crate::workspace::manifest::read_workspace_manifest;

    use super::*;

    fn fail_cleanup(_path: &Path) -> Result<(), EngineError> {
        Err(EngineError::storage_failure("injected cleanup failure"))
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
            Some(crate::workspace::CURRENT_INDEX_VERSION),
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
        let build = prepare_build(active.clone()).expect("stage");
        let checkpoint = build.target.storage_home().join("checkpoint");
        fs::write(&checkpoint, "completed work").expect("checkpoint");
        recover_build(&active.path).expect("discard interrupted stage");
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        let fresh = prepare_build(active.clone()).expect("fresh build");
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
        let build = prepare_build(active.clone()).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("completed storage");
        recover_build(&active.path).expect("discard before manifest commit");
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
        let build = prepare_build(active.clone()).expect("stage");
        recover_build_with(&active.path, false, &fail_cleanup).expect_err("cleanup fails");
        assert!(has_build(&active.path));
        assert!(build.target.storage_home().exists());
        assert!(active.storage_home().join("storage/old").exists());
        recover_build(&active.path).expect("retry cleanup");
        assert!(!has_build(&active.path));
        assert!(!build.target.storage_home().exists());
        recover_build(&active.path).expect("idempotent cleanup");
    }

    #[test]
    fn publication_cleanup_failure_keeps_new_active_and_recovers_after_rename() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = prepare_build(active.clone()).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("completed new storage");
        publish_build_with(build.clone(), 3, &fail_cleanup)
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
        recover_build(&active.path).expect("complete cleanup");
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
        let build = prepare_build(active.clone()).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("new storage");
        publish_build(build, 3).expect("publish");
        let committed = read_workspace_manifest(&active.path)
            .expect("manifest")
            .expect("active");
        assert_eq!(
            committed.index_version,
            Some(crate::workspace::CURRENT_INDEX_VERSION)
        );
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
        let build = prepare_build(active.clone()).expect("stage");
        let moved = directory.path().join("moved");
        fs::rename(original, &moved).expect("move");
        let home = moved.join(".zvec-grep");
        let pending = decode_build(&fs::read(home.join(BUILD_FILE)).expect("record"))
            .expect("abandoned build");
        assert_eq!(
            Some(&pending.generation),
            build.target.storage_generation.as_ref()
        );
        assert_eq!(
            read_build_registration(&home).expect("registration"),
            Some((active.workspace.name.clone(), active.workspace.root.clone()))
        );
        let pending_home = home.join("generations").join(&pending.generation);
        assert!(pending_home.is_dir());
        recover_build(&home).expect("discard moved stage");
        assert!(!pending_home.exists());
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
        let build = prepare_build(active.clone()).expect("stage");
        assert!(has_generation_storage(&active.path).expect("generations present"));
        drop_build_storage(&active.path).expect("drop generations");
        assert!(!active.storage_home().exists());
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        assert!(!has_generation_storage(&active.path).expect("empty generations container"));
        drop_build_storage(&active.path).expect("idempotent drop");
    }

    #[test]
    fn generation_storage_detection_keeps_io_errors() {
        let directory = tempdir().expect("workspace home");
        assert!(!has_generation_storage(directory.path()).expect("missing generations"));
        fs::write(directory.path().join("generations"), b"not a directory")
            .expect("invalid layout");
        assert!(has_generation_storage(directory.path()).is_err());
    }

    fn write_node_storage(home: &Path) {
        for name in ["files.zvec", "index.zvec"] {
            fs::create_dir_all(home.join(name)).expect("Node storage");
            fs::write(home.join(name).join("old"), "Node data").expect("Node data");
        }
    }

    #[test]
    fn recovery_reads_only_the_manifest_commit_marker() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = prepare_build(active.clone()).expect("stage");
        let legacy = serde_json::to_vec(&serde_json::json!({
            "manifestVersion": 999,
            "indexVersion": 999,
            "storageGeneration": active.storage_generation,
            "unrecognizedPolicy": true
        }))
        .expect("legacy JSON");
        fs::write(active.path.join(WORKSPACE_MANIFEST_FILE), &legacy).expect("legacy manifest");
        recover_build(&active.path).expect("metadata schema does not affect recovery");
        assert!(!build.target.storage_home().exists());
        assert!(active.storage_home().join("storage/old").exists());
        assert_eq!(
            fs::read(active.path.join(WORKSPACE_MANIFEST_FILE)).expect("preserved manifest"),
            legacy
        );
    }

    #[test]
    fn failed_rebuild_preserves_node_storage_until_a_successful_publication() {
        let directory = tempdir().expect("workspace");
        let target = manifest(directory.path());
        write_node_storage(&target.path);
        let legacy = br#"{"manifestVersion":1,"indexVersion":1}"#;
        fs::write(target.path.join(WORKSPACE_MANIFEST_FILE), legacy).expect("Node manifest");
        fs::write(target.path.join("authorization.json"), "preserve").expect("unrelated metadata");
        let build = prepare_build(target.clone()).expect("stage");
        recover_build(&target.path).expect("discard interrupted Node rebuild");
        assert!(!build.target.storage_home().exists());
        for name in ["files.zvec", "index.zvec"] {
            assert!(target.path.join(name).join("old").exists());
        }
        assert_eq!(
            fs::read(target.path.join(WORKSPACE_MANIFEST_FILE)).expect("Node manifest retained"),
            legacy
        );
        let fresh = prepare_build(target.clone()).expect("fresh stage");
        publish_build(fresh, 3).expect("publish Rust index");
        for name in ["files.zvec", "index.zvec"] {
            assert!(!target.path.join(name).exists());
        }
        assert!(target.path.join("authorization.json").exists());
    }

    #[test]
    fn explicit_rebuild_discards_invalid_records_without_deleting_existing_data() {
        for invalid in [br#"{"version":1,"target":{}}"#.as_slice(), b"not JSON"] {
            let directory = tempdir().expect("workspace");
            let active = manifest(directory.path());
            write_active(&active);
            write_node_storage(&active.path);
            fs::write(active.path.join(BUILD_FILE), invalid).expect("invalid record");
            recover_build(&active.path).expect_err("ordinary recovery rejects invalid record");
            recover_build_for_rebuild(&active.path)
                .expect("explicit rebuild clears invalid record");
            assert!(!has_build(&active.path));
            assert!(active.storage_home().join("storage/old").exists());
            assert!(active.path.join("files.zvec/old").exists());
            let build = prepare_build(active.clone()).expect("fresh stage without old schema");
            let new_home = build.target.storage_home();
            publish_build(build, 3).expect("publish replacement");
            assert!(new_home.exists());
            assert!(!active.storage_home().exists());
            assert!(!active.path.join("files.zvec").exists());
        }
    }

    #[test]
    fn malformed_commit_marker_preserves_all_storage_until_rebuild_publishes() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let abandoned = prepare_build(active.clone()).expect("stage");
        fs::write(active.path.join(WORKSPACE_MANIFEST_FILE), b"not JSON")
            .expect("damaged manifest");
        recover_build(&active.path).expect_err("cannot safely identify active generation");
        recover_build_for_rebuild(&active.path).expect("replace metadata without deleting data");
        assert!(active.storage_home().exists());
        assert!(abandoned.target.storage_home().exists());
        assert!(!has_build(&active.path));
        let build = prepare_build(active.clone()).expect("replacement");
        let new_home = build.target.storage_home();
        publish_build(build, 3).expect("publish replacement");
        assert!(new_home.exists());
        assert!(!active.storage_home().exists());
        assert!(!abandoned.target.storage_home().exists());
    }

    #[test]
    fn drop_removes_node_storage_without_a_manifest_or_journal() {
        let directory = tempdir().expect("workspace");
        write_node_storage(directory.path());
        drop_build_storage(directory.path()).expect("drop Node storage");
        assert!(!directory.path().join("files.zvec").exists());
        assert!(!directory.path().join("index.zvec").exists());
    }

    #[test]
    fn discard_failed_attempt_keeps_the_original_malformed_manifest_and_storage() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        write_node_storage(&active.path);
        fs::write(active.path.join(WORKSPACE_MANIFEST_FILE), b"not JSON")
            .expect("original damaged manifest");
        let build = prepare_build(active.clone()).expect("replacement");
        discard_build(&build).expect("discard failed attempt");
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        assert!(active.storage_home().join("storage/old").exists());
        assert!(active.path.join("files.zvec/old").exists());
        assert_eq!(
            fs::read(active.path.join(WORKSPACE_MANIFEST_FILE)).expect("original manifest"),
            b"not JSON"
        );
    }

    #[test]
    fn discard_does_not_delete_published_storage_or_another_attempts_record() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let first = prepare_build(active.clone()).expect("first stage");
        let second = prepare_build(active.clone()).expect("replacement stage");
        discard_build(&first).expect("discard only owned stage");
        assert!(!first.target.storage_home().exists());
        assert!(second.target.storage_home().exists());
        assert!(has_build(&active.path));
        publish_build_with(second.clone(), 3, &fail_cleanup).expect("publish before cleanup");
        discard_build(&second).expect("publication must survive a later error");
        assert!(second.target.storage_home().exists());
        assert!(has_build(&active.path));
        recover_build(&active.path).expect("finish publication cleanup");
        assert!(!has_build(&active.path));
        assert!(second.target.storage_home().exists());
    }
}
