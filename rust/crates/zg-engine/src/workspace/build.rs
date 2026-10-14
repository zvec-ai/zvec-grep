//! Build a private index, then publish it under the workspace write lock.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::manifest::{WORKSPACE_MANIFEST_FILE, WorkspaceManifest, write_workspace_manifest};
use crate::{
    EngineError,
    storage::IndexStore,
    utils::{atomic_write, sha256_hex, sync_directory},
};

const BUILD_FILE: &str = "temp/build.json";
const NEW_INDEX: &str = "temp/new";
const OLD_INDEX: &str = "temp/old";

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceBuild {
    pub target: WorkspaceManifest,
}

impl WorkspaceBuild {
    pub(crate) fn index_path(&self) -> PathBuf {
        self.target.path.join(NEW_INDEX)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildRecord {
    generation: String,
    name: String,
    previous_manifest: Option<String>,
    had_index: bool,
    publishing: bool,
}

impl BuildRecord {
    fn validate(&self) -> Result<(), EngineError> {
        if Uuid::parse_str(&self.generation).is_err() {
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

pub(crate) fn has_index_storage(home: &Path) -> Result<bool, EngineError> {
    for path in ["index", NEW_INDEX, OLD_INDEX] {
        match fs::metadata(home.join(path)) {
            Ok(metadata) if metadata.is_dir() => return Ok(true),
            Ok(_) => {
                return Err(EngineError::storage_failure(
                    "workspace index path is not a directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(build_io(error)),
        }
    }
    Ok(false)
}

/// Call while holding the home read lock; a pending swap must recover before reads.
pub(crate) fn needs_recovery(home: &Path) -> Result<bool, EngineError> {
    let Some(record) = read_record(home)? else {
        return Ok(false);
    };
    Ok(record.publishing && !is_committed(home, &record)?)
}

pub(crate) fn recover_build(home: &Path) -> Result<(), EngineError> {
    recover_build_with(home, false, &IndexStore::delete)
}

pub(crate) fn recover_build_for_rebuild(home: &Path) -> Result<(), EngineError> {
    recover_build_with(home, true, &IndexStore::delete)
}

fn recover_build_with(
    home: &Path,
    discard_invalid: bool,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    super::layout::validate_storage_paths(home)?;
    let record = match read_record(home) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(()),
        // Without a backup there is no interrupted directory swap to roll back.
        Err(_) if discard_invalid && !home.join(OLD_INDEX).try_exists().map_err(build_io)? => {
            return cleanup_stage(home, delete_storage);
        }
        Err(error) => return Err(error),
    };
    if is_committed(home, &record)? {
        delete_storage(&home.join(OLD_INDEX))?;
        cleanup_node_storage(home)?;
    } else if record.publishing {
        let current = read_optional(&home.join(WORKSPACE_MANIFEST_FILE))?;
        if current.as_deref().map(sha256_hex) != record.previous_manifest {
            return Err(EngineError::storage_failure(
                "workspace manifest changed during index publication; cannot safely recover",
            ));
        }
        let previous = home.join(OLD_INDEX);
        if previous.try_exists().map_err(build_io)? {
            delete_storage(&home.join("index"))?;
            move_index(&previous, &home.join("index"))?;
        } else if !record.had_index {
            delete_storage(&home.join("index"))?;
        }
    }
    cleanup_stage(home, delete_storage)
}

/// Start a new attempt only after recovering the previous one.
pub(crate) fn prepare_build(mut target: WorkspaceManifest) -> Result<WorkspaceBuild, EngineError> {
    super::layout::ensure_workspace_home(&super::layout::workspace_index_location(
        &target.workspace.root,
        Some(&target.path),
    )?)?;
    if has_build(&target.path) || target.path.join(OLD_INDEX).try_exists().map_err(build_io)? {
        return Err(EngineError::resource_busy(
            "recover the unfinished workspace build before starting another",
        ));
    }
    target.storage_generation = Some(Uuid::new_v4().to_string());
    target.validate()?;
    let record = BuildRecord {
        generation: target.storage_generation.clone().expect("new generation"),
        name: target.workspace.name.clone(),
        previous_manifest: read_optional(&target.path.join(WORKSPACE_MANIFEST_FILE))?
            .as_deref()
            .map(sha256_hex),
        had_index: target.index_path().try_exists().map_err(build_io)?,
        publishing: false,
    };
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(target.path.join("temp")).map_err(build_io)?;
    write_record(&target.path, &record)?;
    sync_directory(&target.path)?;
    let build = WorkspaceBuild { target };
    if let Err(error) = builder
        .create(build.index_path())
        .map_err(build_io)
        .and_then(|()| sync_directory(&build.target.path.join("temp")))
    {
        let _ = discard_build(&build);
        return Err(error);
    }
    Ok(build)
}

/// A stale completion must never discard a newer attempt or a published index.
pub(crate) fn discard_build(build: &WorkspaceBuild) -> Result<(), EngineError> {
    let Some(record) = read_record(&build.target.path)? else {
        return Ok(());
    };
    if Some(&record.generation) != build.target.storage_generation.as_ref()
        || is_committed(&build.target.path, &record)?
    {
        return Ok(());
    }
    recover_build(&build.target.path)
}

pub(crate) fn publish_build(build: WorkspaceBuild, updated_time: u64) -> Result<(), EngineError> {
    publish_build_with(build, updated_time, &IndexStore::delete)
}

fn publish_build_with(
    mut build: WorkspaceBuild,
    updated_time: u64,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    build.target.record_update(updated_time);
    let home = &build.target.path;
    let result = begin_publication(&build).and_then(|()| {
        if build.target.index_path().try_exists().map_err(build_io)? {
            move_index(&build.target.index_path(), &home.join(OLD_INDEX))?;
        }
        move_index(&build.index_path(), &build.target.index_path())?;
        // Only this atomic file replacement commits the new generation.
        let bytes = fs::read(home.join("temp/manifest.json")).map_err(build_io)?;
        atomic_write(&home.join(WORKSPACE_MANIFEST_FILE), &bytes)
    });
    if let Err(error) = result {
        let _ = discard_build(&build);
        return Err(error);
    }
    // Keep the record if cleanup fails, so the next writer can finish it.
    let _ = recover_build_with(home, false, delete_storage);
    Ok(())
}

fn begin_publication(build: &WorkspaceBuild) -> Result<(), EngineError> {
    let home = &build.target.path;
    let mut record = read_record(home)?
        .ok_or_else(|| EngineError::storage_failure("workspace build record is missing"))?;
    if Some(&record.generation) != build.target.storage_generation.as_ref() || record.publishing {
        return Err(EngineError::storage_failure(
            "workspace build no longer owns the pending publication",
        ));
    }
    if read_optional(&home.join(WORKSPACE_MANIFEST_FILE))?
        .as_deref()
        .map(sha256_hex)
        != record.previous_manifest
    {
        return Err(EngineError::storage_failure(
            "workspace manifest changed while building the index",
        ));
    }
    write_workspace_manifest(&home.join("temp"), &build.target)?;
    record.publishing = true;
    write_record(home, &record)
}

/// Both parents must be durable before the manifest may commit a directory move.
fn move_index(from: &Path, to: &Path) -> Result<(), EngineError> {
    fs::rename(from, to).map_err(build_io)?;
    sync_directory(from.parent().expect("index parent"))?;
    sync_directory(to.parent().expect("index parent"))
}

pub(crate) fn drop_build_storage(home: &Path) -> Result<(), EngineError> {
    super::layout::validate_storage_paths(home)?;
    IndexStore::delete(&home.join("index"))?;
    IndexStore::delete(&home.join(OLD_INDEX))?;
    cleanup_stage(home, &IndexStore::delete)?;
    cleanup_node_storage(home)
}

fn cleanup_stage(
    home: &Path,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    delete_storage(&home.join(NEW_INDEX))?;
    for name in ["temp/manifest.json", BUILD_FILE] {
        match fs::remove_file(home.join(name)) {
            Ok(()) => sync_directory(&home.join("temp"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(build_io(error)),
        }
    }
    match fs::remove_dir(home.join("temp")) {
        Ok(()) => sync_directory(home),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(build_io(error)),
    }
}

pub(crate) fn read_build_registration(home: &Path) -> Result<Option<String>, EngineError> {
    Ok(read_record(home)?.map(|build| build.name))
}

fn read_record(home: &Path) -> Result<Option<BuildRecord>, EngineError> {
    read_optional(&home.join(BUILD_FILE))?
        .as_deref()
        .map(decode_build)
        .transpose()
}

fn decode_build(bytes: &[u8]) -> Result<BuildRecord, EngineError> {
    let build: BuildRecord = serde_json::from_slice(bytes).map_err(|error| {
        EngineError::storage_failure(format!("invalid workspace build record: {error}"))
    })?;
    build.validate()?;
    Ok(build)
}

fn is_committed(home: &Path, build: &BuildRecord) -> Result<bool, EngineError> {
    let bytes = read_optional(&home.join(WORKSPACE_MANIFEST_FILE))?;
    let generation = bytes
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|value| {
            value
                .get("storageGeneration")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    Ok(generation.as_deref() == Some(&build.generation))
}

fn write_record(home: &Path, record: &BuildRecord) -> Result<(), EngineError> {
    record.validate()?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| EngineError::internal(format!("encode workspace build: {error}")))?;
    atomic_write(&home.join(BUILD_FILE), &bytes)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(build_io(error)),
    }
}

fn cleanup_node_storage(home: &Path) -> Result<(), EngineError> {
    for name in ["files.zvec", "index.zvec"] {
        let path = home.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                }
                .map_err(build_io)?;
                sync_directory(home)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(build_io(error)),
        }
    }
    Ok(())
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
        model::{EmbeddingMetric, EmbeddingModelInfo},
    };
    use crate::workspace::manifest::read_workspace_manifest;

    use super::*;

    fn fail_cleanup(_path: &Path) -> Result<(), EngineError> {
        Err(EngineError::storage_failure("injected cleanup failure"))
    }

    fn manifest(root: &Path) -> WorkspaceManifest {
        let root = fs::canonicalize(root).expect("canonical fixture root");
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "workspace".to_owned(),
                root: root.clone(),
                scan: crate::domain::ScanRules::default(),
                index: IndexState::Enabled(Box::new(
                    IndexDescriptor::new(
                        EmbeddingModelInfo::new(
                            crate::domain::model::ModelInfo::new(
                                "local",
                                "example",
                                [
                                    crate::domain::ContentKind::Text,
                                    crate::domain::ContentKind::Code,
                                ],
                                32,
                            )
                            .expect("fixture model identity"),
                            8,
                            EmbeddingMetric::Cosine,
                            None,
                            None,
                        )
                        .expect("embedding model"),
                        std::collections::BTreeMap::new(),
                        crate::domain::FTS_CONFIG,
                    )
                    .expect("index descriptor"),
                )),
                created_epoch_ms: 1,
                updated_epoch_ms: 2,
            },
            root.join(".zvec-grep"),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            std::collections::BTreeMap::from([(
                "local/example".into(),
                crate::workspace::credentials::StoredModelConfig::default(),
            )]),
        )
        .expect("manifest");
        manifest.storage_generation = Some(Uuid::new_v4().to_string());
        manifest
    }

    fn write_active(manifest: &WorkspaceManifest) {
        fs::create_dir_all(manifest.index_path()).expect("active storage");
        fs::write(manifest.index_path().join("old"), "old data").expect("old data");
        fs::write(manifest.index_path().join("schema.json"), br#"{"tables":[{"kind":"text","embedding":{"model":{"provider":"local","name":"example"}}}]}"#).expect("owned storage metadata");
        write_workspace_manifest(&manifest.path, manifest).expect("active manifest");
    }

    fn stage(active: &WorkspaceManifest) -> WorkspaceBuild {
        let build = prepare_build(active.clone()).expect("stage");
        fs::write(build.index_path().join("new"), "new data").expect("new data");
        build
    }

    #[test]
    fn interrupted_builds_are_discarded_before_a_fresh_attempt() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        recover_build_with(&active.path, false, &fail_cleanup).expect_err("cleanup fails");
        assert!(has_build(&active.path));
        assert!(build.index_path().join("new").exists());
        recover_build(&active.path).expect("retry cleanup");
        recover_build(&active.path).expect("idempotent");
        assert!(!active.path.join("temp").exists());
        assert!(active.index_path().join("old").exists());
        let fresh = prepare_build(active.clone()).expect("fresh attempt");
        assert_ne!(
            fresh.target.storage_generation,
            build.target.storage_generation
        );
        assert!(!fresh.index_path().join("new").exists());
        discard_build(&build).expect("stale completion does not discard fresh attempt");
        assert!(fresh.index_path().exists());
        discard_build(&fresh).expect("discard fresh attempt");
    }

    #[test]
    fn every_interrupted_publication_step_restores_the_previous_index() {
        for had_index in [false, true] {
            for step in 0..=2 {
                let directory = tempdir().expect("workspace");
                let active = manifest(directory.path());
                if had_index {
                    write_active(&active);
                }
                let build = stage(&active);
                begin_publication(&build).expect("prepare commit");
                if step >= 1 && had_index {
                    move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
                }
                if step >= 2 {
                    move_index(&build.index_path(), &active.index_path())
                        .expect("install new index");
                }
                assert!(needs_recovery(&active.path).expect("publication pending"));
                recover_build(&active.path).expect("rollback interrupted publication");
                recover_build(&active.path).expect("rollback is idempotent");
                assert!(!active.path.join("temp").exists());
                assert_eq!(active.index_path().join("old").exists(), had_index);
                assert!(!active.index_path().join("new").exists());
                assert_eq!(
                    read_workspace_manifest(
                        &crate::workspace::layout::workspace_index_location(
                            &active.workspace.root,
                            Some(&active.path)
                        )
                        .expect("location")
                    )
                    .expect("manifest"),
                    had_index.then_some(active)
                );
            }
        }
    }

    #[tokio::test]
    async fn the_first_read_recovers_an_interrupted_swap_under_the_write_lock() {
        use crate::workspace::lock::{LockMode, LockWait};
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        move_index(&build.index_path(), &active.index_path()).expect("install");
        let _reader = LockWait::new(None, None)
            .expect("wait")
            .acquire(&active.path, LockMode::Read, "test.read")
            .await
            .expect("recovered read");
        assert!(active.index_path().join("old").exists());
        assert!(!active.index_path().join("new").exists());
        assert!(!has_build(&active.path));
    }

    #[test]
    fn recovery_can_itself_be_interrupted_after_restoring_the_old_directory() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        move_index(&build.index_path(), &active.index_path()).expect("install");
        IndexStore::delete(&active.index_path()).expect("remove uncommitted index");
        move_index(&active.path.join(OLD_INDEX), &active.index_path()).expect("restore");
        recover_build(&active.path).expect("resume interrupted rollback");
        assert!(active.index_path().join("old").exists());
        assert!(!active.path.join("temp").exists());
    }

    #[test]
    fn publication_cleanup_failure_preserves_new_index_and_retries_after_rename() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        publish_build_with(build.clone(), 3, &fail_cleanup)
            .expect("committed despite cleanup failure");
        let mut committed = read_workspace_manifest(
            &crate::workspace::layout::workspace_index_location(
                &active.workspace.root,
                Some(&active.path),
            )
            .expect("location"),
        )
        .expect("read")
        .expect("manifest");
        assert_eq!(
            committed.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(committed.workspace.updated_epoch_ms, 3);
        assert!(committed.index_path().join("new").exists());
        assert!(!needs_recovery(&active.path).expect("safe for readers"));
        assert!(has_build(&active.path));
        committed.workspace.name = "renamed".into();
        write_workspace_manifest(&active.path, &committed).expect("rename");
        discard_build(&build).expect("never discard published index");
        recover_build(&active.path).expect("finish cleanup");
        assert!(!active.path.join("temp").exists());
        assert!(committed.index_path().join("new").exists());
        assert!(!committed.index_path().join("old").exists());
        assert_eq!(
            read_workspace_manifest(
                &crate::workspace::layout::workspace_index_location(
                    &active.workspace.root,
                    Some(&active.path)
                )
                .expect("location")
            )
            .expect("read"),
            Some(committed)
        );
    }

    #[test]
    fn moving_workspace_keeps_recovery_relative_to_its_new_home() {
        let directory = tempdir().expect("workspace");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("root");
        let active = manifest(&original);
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move workspace");
        let home = moved.join(".zvec-grep");
        assert_eq!(
            read_build_registration(&home).expect("registration"),
            Some(active.workspace.name)
        );
        recover_build(&home).expect("recover relocated index");
        assert!(home.join("index/old").exists());
        assert!(!home.join("temp").exists());
    }

    #[test]
    fn malformed_original_manifest_is_preserved_until_successful_publication() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        fs::write(active.path.join(WORKSPACE_MANIFEST_FILE), b"not JSON")
            .expect("damaged manifest");
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        move_index(&build.index_path(), &active.index_path()).expect("install");
        discard_build(&build).expect("rollback failed replacement");
        assert!(active.index_path().join("old").exists());
        assert_eq!(
            fs::read(active.path.join(WORKSPACE_MANIFEST_FILE)).expect("manifest"),
            b"not JSON"
        );
        let fresh = stage(&active);
        publish_build(fresh, 3).expect("replace corrupted index");
        assert!(active.index_path().join("new").exists());
        assert!(!active.path.join("temp").exists());
    }

    #[test]
    fn unexpected_manifest_change_prevents_a_destructive_rollback() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        move_index(&build.index_path(), &active.index_path()).expect("install");
        fs::write(
            active.path.join(WORKSPACE_MANIFEST_FILE),
            b"unexpected modification",
        )
        .expect("modify");
        recover_build(&active.path).expect_err("cannot determine commit safely");
        assert!(active.index_path().join("new").exists());
        assert!(active.path.join(OLD_INDEX).join("old").exists());
        assert!(has_build(&active.path));
    }

    #[test]
    fn drop_removes_current_and_temporary_indexes_but_keeps_unrelated_files() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("marker");
        move_index(&active.index_path(), &active.path.join(OLD_INDEX)).expect("backup");
        fs::write(active.path.join("authorization.json"), b"keep").expect("unrelated data");
        assert!(has_index_storage(&active.path).expect("storage"));
        drop_build_storage(&active.path).expect("drop");
        drop_build_storage(&active.path).expect("idempotent drop");
        assert!(!has_index_storage(&active.path).expect("empty"));
        assert!(!active.path.join("temp").exists());
        assert!(active.path.join("authorization.json").exists());
    }
}
