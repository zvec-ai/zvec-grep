//! Prepare, publish, recover, and remove workspace index storage.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::manifest::write_workspace_manifest;
use crate::{
    EngineError,
    domain::{Workspace, WorkspaceLayout},
    storage::IndexStore,
    utils::{
        atomic_write, directory_exists, ensure_directory, reject_symlink, sha256_hex,
        sync_directory,
    },
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildRecord {
    index_version: u32,
    generation: String,
    previous_manifest: Option<String>,
    had_index: bool,
    publishing: bool,
}

impl BuildRecord {
    fn validate(&self) -> Result<(), EngineError> {
        if self.index_version != super::CURRENT_INDEX_VERSION {
            return Err(EngineError::storage_failure(format!(
                "unsupported workspace build index version {}; expected {}",
                self.index_version,
                super::CURRENT_INDEX_VERSION,
            )));
        }
        if Uuid::parse_str(&self.generation).is_err() {
            return Err(EngineError::storage_failure(
                "invalid workspace build ownership record",
            ));
        }
        Ok(())
    }
}

/// Call while holding the workspace read lock; a pending swap must recover before reads.
pub(crate) fn needs_recovery(layout: &WorkspaceLayout) -> Result<bool, EngineError> {
    let Some(record) = read_record(layout)? else {
        return directory_exists(layout.backup_index_dir());
    };
    Ok(record.publishing && !is_committed(layout, &record)?)
}

pub(crate) fn recover_build(layout: &WorkspaceLayout) -> Result<(), EngineError> {
    recover_build_with(layout, false, &IndexStore::delete)
}

pub(crate) fn recover_build_for_rebuild(layout: &WorkspaceLayout) -> Result<(), EngineError> {
    recover_build_with(layout, true, &IndexStore::delete)
}

fn recover_build_with(
    layout: &WorkspaceLayout,
    discard_invalid: bool,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let record = match read_record(layout) {
        Ok(Some(record)) => record,
        Ok(None) => {
            if directory_exists(layout.backup_index_dir())? {
                return Err(EngineError::storage_failure(
                    "workspace backup has no build record; cannot safely recover",
                ));
            }
            return cleanup_stage(layout, delete_storage);
        }
        // Without a backup there is no interrupted directory swap to roll back.
        Err(_) if discard_invalid && !directory_exists(layout.backup_index_dir())? => {
            return cleanup_stage(layout, delete_storage);
        }
        Err(error) => return Err(error),
    };
    if is_committed(layout, &record)? {
        delete_storage(layout.backup_index_dir())?;
    } else if record.publishing {
        let current = read_optional(layout.manifest_path())?;
        if current.as_deref().map(sha256_hex) != record.previous_manifest {
            return Err(EngineError::storage_failure(
                "workspace manifest changed during index publication; cannot safely recover",
            ));
        }
        let previous = layout.backup_index_dir();
        if directory_exists(previous)? {
            delete_storage(layout.index_dir())?;
            move_index(previous, layout.index_dir())?;
        } else if !record.had_index {
            delete_storage(layout.index_dir())?;
        }
    }
    cleanup_stage(layout, delete_storage)
}

/// Start a new attempt only after recovering the previous one.
pub(crate) fn prepare_build(target: &Workspace) -> Result<(), EngineError> {
    let generation = target.storage_generation().ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "workspace {}: preparing an index build requires an enabled index",
            target.source_root().display(),
        ))
    })?;
    ensure_directory(target.layout().storage_dir())?;
    if target.layout().has_build_record()?
        || directory_exists(target.layout().backup_index_dir())?
        || directory_exists(target.layout().staged_index_dir())?
    {
        return Err(EngineError::resource_busy(
            "recover the unfinished workspace build before starting another",
        ));
    }
    let record = BuildRecord {
        index_version: super::CURRENT_INDEX_VERSION,
        generation: generation.to_owned(),
        previous_manifest: read_optional(target.layout().manifest_path())?
            .as_deref()
            .map(sha256_hex),
        had_index: directory_exists(target.layout().index_dir())?,
        publishing: false,
    };
    ensure_directory(target.layout().temp_dir())?;
    write_record(target.layout(), &record)?;
    sync_directory(target.layout().storage_dir())?;
    if let Err(error) = ensure_directory(target.layout().staged_index_dir())
        .and_then(|()| sync_directory(target.layout().temp_dir()))
    {
        let _ = discard_build(target);
        return Err(error);
    }
    Ok(())
}

/// A stale completion must never discard a newer attempt or a published index.
pub(crate) fn discard_build(build: &Workspace) -> Result<(), EngineError> {
    let Some(record) = read_record(build.layout())? else {
        return Ok(());
    };
    if Some(record.generation.as_str()) != build.storage_generation()
        || is_committed(build.layout(), &record)?
    {
        return Ok(());
    }
    recover_build(build.layout())
}

pub(crate) fn publish_build(build: Workspace, updated_time: u64) -> Result<(), EngineError> {
    publish_build_with(build, updated_time, &IndexStore::delete)
}

fn publish_build_with(
    mut build: Workspace,
    updated_time: u64,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    build.record_update(updated_time);
    let layout = build.layout();
    let result = begin_publication(&build).and_then(|()| {
        if directory_exists(build.layout().index_dir())? {
            move_index(build.layout().index_dir(), layout.backup_index_dir())?;
        }
        move_index(
            build.layout().staged_index_dir(),
            build.layout().index_dir(),
        )?;
        // Only this atomic file replacement commits the new generation.
        let bytes = fs::read(layout.staged_manifest_path()).map_err(build_io)?;
        atomic_write(layout.manifest_path(), &bytes)
    });
    if let Err(error) = result {
        let _ = discard_build(&build);
        return Err(error);
    }
    // Keep the record if cleanup fails, so the next writer can finish it.
    let _ = recover_build_with(layout, false, delete_storage);
    Ok(())
}

fn begin_publication(build: &Workspace) -> Result<(), EngineError> {
    let layout = build.layout();
    let mut record = read_record(layout)?
        .ok_or_else(|| EngineError::storage_failure("workspace build record is missing"))?;
    if Some(record.generation.as_str()) != build.storage_generation() || record.publishing {
        return Err(EngineError::storage_failure(
            "workspace build no longer owns the pending publication",
        ));
    }
    if read_optional(layout.manifest_path())?
        .as_deref()
        .map(sha256_hex)
        != record.previous_manifest
    {
        return Err(EngineError::storage_failure(
            "workspace manifest changed while building the index",
        ));
    }
    write_workspace_manifest(layout.staged_manifest_path(), build)?;
    record.publishing = true;
    write_record(layout, &record)
}

/// Both parents must be durable before the manifest may commit a directory move.
fn move_index(from: &Path, to: &Path) -> Result<(), EngineError> {
    if !directory_exists(from)? {
        return Err(EngineError::storage_failure(format!(
            "index directory is missing: {}",
            from.display(),
        )));
    }
    reject_symlink(to)?;
    fs::rename(from, to).map_err(build_io)?;
    sync_directory(from.parent().expect("index parent"))?;
    sync_directory(to.parent().expect("index parent"))
}

pub(crate) fn drop_index_storage(layout: &WorkspaceLayout) -> Result<(), EngineError> {
    IndexStore::delete(layout.index_dir())?;
    reject_symlink(layout.temp_dir())?;
    IndexStore::delete(layout.backup_index_dir())?;
    cleanup_stage(layout, &IndexStore::delete)
}

fn cleanup_stage(
    layout: &WorkspaceLayout,
    delete_storage: &impl Fn(&Path) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    reject_symlink(layout.temp_dir())?;
    delete_storage(layout.staged_index_dir())?;
    for path in [layout.staged_manifest_path(), layout.build_record_path()] {
        reject_symlink(path)?;
        match fs::remove_file(path) {
            Ok(()) => sync_directory(layout.temp_dir())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(build_io(error)),
        }
    }
    match fs::remove_dir(layout.temp_dir()) {
        Ok(()) => sync_directory(layout.storage_dir()),
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

fn read_record(layout: &WorkspaceLayout) -> Result<Option<BuildRecord>, EngineError> {
    reject_symlink(layout.temp_dir())?;
    read_optional(layout.build_record_path())?
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

fn is_committed(layout: &WorkspaceLayout, build: &BuildRecord) -> Result<bool, EngineError> {
    let bytes = read_optional(layout.manifest_path())?;
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

fn write_record(layout: &WorkspaceLayout, record: &BuildRecord) -> Result<(), EngineError> {
    record.validate()?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| EngineError::internal(format!("encode workspace build: {error}")))?;
    atomic_write(layout.build_record_path(), &bytes)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    reject_symlink(path)?;
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
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
        model::{EmbeddingMetric, EmbeddingModelInfo},
    };
    use crate::workspace::manifest::read_workspace_manifest;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn build_operations_reject_a_symlinked_temporary_directory() {
        let directory = tempdir().expect("workspace");
        let external = tempdir().expect("external directory");
        let target = manifest(directory.path());
        fs::create_dir(target.layout().storage_dir()).expect("workspace home");
        std::os::unix::fs::symlink(external.path(), target.layout().temp_dir())
            .expect("temporary directory alias");
        let marker = external.path().join("manifest.json");
        fs::write(&marker, b"external data").expect("external marker");

        assert!(needs_recovery(target.layout()).is_err());
        assert!(prepare_build(&target).is_err());
        assert!(recover_build(target.layout()).is_err());
        assert!(drop_index_storage(target.layout()).is_err());
        assert_eq!(
            fs::read(marker).expect("preserved marker"),
            b"external data"
        );
        assert!(!external.path().join("new").exists());
        assert!(!external.path().join("build.json").exists());
    }

    fn fail_cleanup(_path: &Path) -> Result<(), EngineError> {
        Err(EngineError::storage_failure("injected cleanup failure"))
    }

    fn manifest(root: &Path) -> Workspace {
        let root = fs::canonicalize(root).expect("canonical fixture root");
        Workspace::new(
            &root,
            WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout"),
            crate::domain::ScanRules::default(),
            Some(crate::workspace::CURRENT_INDEX_VERSION),
            Some(Uuid::new_v4().to_string()),
            1,
            2,
            IndexState::Enabled(Box::new(
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
                    std::collections::BTreeMap::from([(
                        "local/example".into(),
                        crate::domain::model::ModelConfig::default(),
                    )]),
                    crate::domain::FTS_CONFIG,
                )
                .expect("index descriptor"),
            )),
        )
        .expect("manifest")
    }

    fn write_active(manifest: &Workspace) {
        fs::create_dir_all(manifest.layout().index_dir()).expect("active storage");
        fs::write(manifest.layout().index_dir().join("old"), "old data").expect("old data");
        write_workspace_manifest(manifest.layout().manifest_path(), manifest)
            .expect("active manifest");
    }

    fn build_target(active: &Workspace) -> Workspace {
        Workspace::new(
            active.source_root(),
            active.layout().clone(),
            active.scan().clone(),
            active.index_version(),
            Some(Uuid::new_v4().to_string()),
            active.created_epoch_ms(),
            active.updated_epoch_ms(),
            active.index().clone(),
        )
        .expect("build target")
    }

    fn stage(active: &Workspace) -> Workspace {
        let build = build_target(active);
        prepare_build(&build).expect("stage");
        fs::write(build.layout().staged_index_dir().join("new"), "new data").expect("new data");
        build
    }

    #[test]
    fn interrupted_builds_are_discarded_before_a_fresh_attempt() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        recover_build_with(active.layout(), false, &fail_cleanup).expect_err("cleanup fails");
        assert!(active.layout().has_build_record().expect("build record"));
        assert!(build.layout().staged_index_dir().join("new").exists());
        recover_build(active.layout()).expect("retry cleanup");
        recover_build(active.layout()).expect("idempotent");
        assert!(!active.layout().temp_dir().exists());
        assert!(active.layout().index_dir().join("old").exists());
        let fresh = build_target(&active);
        prepare_build(&fresh).expect("fresh attempt");
        assert_ne!(fresh.storage_generation(), build.storage_generation());
        assert!(!fresh.layout().staged_index_dir().join("new").exists());
        discard_build(&build).expect("stale completion does not discard fresh attempt");
        assert!(fresh.layout().staged_index_dir().exists());
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
                    move_index(
                        active.layout().index_dir(),
                        active.layout().backup_index_dir(),
                    )
                    .expect("backup");
                }
                if step >= 2 {
                    move_index(
                        build.layout().staged_index_dir(),
                        active.layout().index_dir(),
                    )
                    .expect("install new index");
                }
                assert!(needs_recovery(active.layout()).expect("publication pending"));
                recover_build(active.layout()).expect("rollback interrupted publication");
                recover_build(active.layout()).expect("rollback is idempotent");
                assert!(!active.layout().temp_dir().exists());
                assert_eq!(active.layout().index_dir().join("old").exists(), had_index);
                assert!(!active.layout().index_dir().join("new").exists());
                assert_eq!(
                    read_workspace_manifest(active.source_root(), active.layout())
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
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        move_index(
            build.layout().staged_index_dir(),
            active.layout().index_dir(),
        )
        .expect("install");
        let _reader = LockWait::new(None, None)
            .expect("wait")
            .acquire(
                active.source_root(),
                active.layout(),
                LockMode::Shared,
                "test.read",
            )
            .await
            .expect("recovered read");
        assert!(active.layout().index_dir().join("old").exists());
        assert!(!active.layout().index_dir().join("new").exists());
        assert!(!active.layout().has_build_record().expect("build record"));
    }

    #[test]
    fn recovery_can_itself_be_interrupted_after_restoring_the_old_directory() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        move_index(
            build.layout().staged_index_dir(),
            active.layout().index_dir(),
        )
        .expect("install");
        IndexStore::delete(active.layout().index_dir()).expect("remove uncommitted index");
        move_index(
            active.layout().backup_index_dir(),
            active.layout().index_dir(),
        )
        .expect("restore");
        recover_build(active.layout()).expect("resume interrupted rollback");
        assert!(active.layout().index_dir().join("old").exists());
        assert!(!active.layout().temp_dir().exists());
    }

    #[test]
    fn publication_cleanup_failure_preserves_new_index_and_retries_after_metadata_update() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        publish_build_with(build.clone(), 3, &fail_cleanup)
            .expect("committed despite cleanup failure");
        let mut committed = read_workspace_manifest(active.source_root(), active.layout())
            .expect("read")
            .expect("manifest");
        assert_eq!(committed.storage_generation(), build.storage_generation());
        assert_eq!(committed.updated_epoch_ms(), 3);
        assert!(committed.layout().index_dir().join("new").exists());
        assert!(!needs_recovery(active.layout()).expect("safe for readers"));
        assert!(active.layout().has_build_record().expect("build record"));
        committed.record_update(4);
        write_workspace_manifest(active.layout().manifest_path(), &committed).expect("rename");
        discard_build(&build).expect("never discard published index");
        recover_build(active.layout()).expect("finish cleanup");
        assert!(!active.layout().temp_dir().exists());
        assert!(committed.layout().index_dir().join("new").exists());
        assert!(!committed.layout().index_dir().join("old").exists());
        assert_eq!(
            read_workspace_manifest(active.source_root(), active.layout()).expect("read"),
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
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        let moved = directory.path().join("moved");
        fs::rename(&original, &moved).expect("move workspace");
        let layout = WorkspaceLayout::new(moved.join(".zvec-grep")).expect("layout");
        recover_build(&layout).expect("recover relocated index");
        assert!(layout.index_dir().join("old").exists());
        assert!(!layout.temp_dir().exists());
    }

    #[test]
    fn automatic_recovery_rejects_unknown_versions_before_moving_index_data() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("pending publication");
        let layout = active.layout();
        move_index(layout.index_dir(), layout.backup_index_dir()).expect("backup");
        move_index(layout.staged_index_dir(), layout.index_dir()).expect("install");
        let original = fs::read(layout.manifest_path()).expect("manifest");
        let mut future: serde_json::Value = serde_json::from_slice(&original).expect("JSON");
        future["indexVersion"] = serde_json::json!(super::super::CURRENT_INDEX_VERSION + 1);
        fs::write(layout.manifest_path(), future.to_string()).expect("future version");
        let error = crate::workspace::lock::try_acquire_workspace_read(
            active.source_root(),
            layout,
            "test.recovery",
        )
        .expect_err("unsupported manifest must not recover");
        assert!(error.message().contains("unsupported index version"));
        assert!(layout.index_dir().join("new").exists());
        assert!(layout.backup_index_dir().join("old").exists());
        fs::write(layout.manifest_path(), original).expect("restore manifest");
        recover_build(layout).expect("recover current format");
        assert!(layout.index_dir().join("old").exists());

        let build = stage(&active);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(layout.build_record_path()).expect("build record"))
                .expect("record JSON");
        record["index_version"] = serde_json::json!(super::super::CURRENT_INDEX_VERSION + 1);
        fs::write(layout.build_record_path(), record.to_string()).expect("future build record");
        assert!(recover_build(layout).is_err());
        assert!(build.layout().staged_index_dir().join("new").exists());
        assert!(layout.index_dir().join("old").exists());
    }

    #[test]
    fn malformed_original_manifest_is_preserved_until_successful_publication() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        fs::write(active.layout().manifest_path(), b"not JSON").expect("damaged manifest");
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        move_index(
            build.layout().staged_index_dir(),
            active.layout().index_dir(),
        )
        .expect("install");
        discard_build(&build).expect("rollback failed replacement");
        assert!(active.layout().index_dir().join("old").exists());
        assert_eq!(
            fs::read(active.layout().manifest_path()).expect("manifest"),
            b"not JSON"
        );
        let fresh = stage(&active);
        publish_build(fresh, 3).expect("replace corrupted index");
        assert!(active.layout().index_dir().join("new").exists());
        assert!(!active.layout().temp_dir().exists());
    }

    #[test]
    fn unexpected_manifest_change_prevents_a_destructive_rollback() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("commit marker");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        move_index(
            build.layout().staged_index_dir(),
            active.layout().index_dir(),
        )
        .expect("install");
        fs::write(active.layout().manifest_path(), b"unexpected modification").expect("modify");
        recover_build(active.layout()).expect_err("cannot determine commit safely");
        assert!(active.layout().index_dir().join("new").exists());
        assert!(active.layout().backup_index_dir().join("old").exists());
        assert!(active.layout().has_build_record().expect("build record"));
    }

    #[test]
    fn drop_removes_current_and_temporary_indexes_but_keeps_unrelated_files() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("marker");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        fs::write(active.layout().authorization_path(), b"keep").expect("unrelated data");
        assert!(directory_exists(active.layout().backup_index_dir()).expect("backup index"));
        drop_index_storage(active.layout()).expect("drop");
        drop_index_storage(active.layout()).expect("idempotent drop");
        assert!(!directory_exists(active.layout().index_dir()).expect("index"));
        assert!(!active.layout().temp_dir().exists());
        assert!(active.layout().authorization_path().exists());
    }

    fn with_api_key(workspace: &Workspace, api_key: &str) -> Workspace {
        let index = workspace.index().descriptor().expect("index");
        let mut configs = index.embedding_configs().clone();
        configs.insert(
            "local/example".into(),
            crate::domain::model::ModelConfig {
                api_key: Some(
                    crate::workspace::credentials::encrypt_api_key(
                        workspace.layout(),
                        "local/example",
                        api_key,
                    )
                    .expect("encrypted key"),
                ),
                endpoint: None,
                ..Default::default()
            },
        );
        let index = crate::domain::IndexDescriptor::new(
            index.default_embedding_model().clone(),
            index.embedding_routes().clone(),
            configs,
            index.fts(),
        )
        .expect("descriptor");
        crate::domain::Workspace::new(
            workspace.source_root(),
            workspace.layout().clone(),
            workspace.scan().clone(),
            workspace.index_version(),
            workspace.storage_generation().map(str::to_owned),
            workspace.created_epoch_ms(),
            workspace.updated_epoch_ms(),
            crate::domain::IndexState::Enabled(Box::new(index)),
        )
        .expect("workspace")
    }

    #[test]
    fn a_failed_publication_preserves_saved_credentials() {
        let directory = tempdir().expect("workspace");
        let mut active = manifest(directory.path());
        write_active(&active);
        active = with_api_key(&active, "original-key");
        write_workspace_manifest(active.layout().manifest_path(), &active)
            .expect("original manifest");
        let key = fs::read(active.layout().credentials_key_path()).expect("workspace key");
        let target = with_api_key(&active, "replacement-key");
        let build = stage(&target);
        begin_publication(&build).expect("publication");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        move_index(
            build.layout().staged_index_dir(),
            active.layout().index_dir(),
        )
        .expect("install");
        discard_build(&build).expect("rollback");
        let restored = read_workspace_manifest(active.source_root(), active.layout())
            .expect("read")
            .expect("manifest");
        assert_eq!(restored, active);
        assert_eq!(
            restored
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs()["local/example"]
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    active.layout(),
                    "local/example"
                ))
                .transpose()
                .expect("decrypt"),
            Some("original-key".into())
        );
        assert_eq!(
            fs::read(active.layout().credentials_key_path()).expect("preserved key"),
            key
        );
        let build = stage(&target);
        publish_build(build, 3).expect("publish replacement");
        let replaced = read_workspace_manifest(active.source_root(), active.layout())
            .expect("read")
            .expect("manifest");
        assert_eq!(
            replaced
                .index()
                .descriptor()
                .expect("indexed workspace")
                .embedding_configs()["local/example"]
                .api_key
                .as_ref()
                .map(|key| crate::workspace::credentials::decrypt_api_key(
                    key,
                    active.layout(),
                    "local/example"
                ))
                .transpose()
                .expect("decrypt"),
            Some("replacement-key".into())
        );
    }

    #[test]
    fn corrupt_build_metadata_never_discards_a_possible_backup() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let build = stage(&active);
        begin_publication(&build).expect("publication");
        move_index(
            active.layout().index_dir(),
            active.layout().backup_index_dir(),
        )
        .expect("backup");
        fs::write(active.layout().build_record_path(), b"invalid").expect("corrupt record");
        assert!(recover_build_for_rebuild(active.layout()).is_err());
        assert!(active.layout().backup_index_dir().join("old").is_file());
        assert!(build.layout().staged_index_dir().join("new").is_file());
    }

    #[test]
    fn orphaned_stage_is_discarded_but_an_unowned_backup_is_preserved() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let orphan = active.layout().staged_index_dir();
        fs::create_dir_all(orphan).expect("orphaned stage");
        fs::write(orphan.join("stale"), "stale data").expect("stale data");
        assert!(prepare_build(&active).is_err());
        recover_build(active.layout()).expect("discard orphaned stage");
        assert!(!orphan.exists());
        assert!(active.layout().index_dir().join("old").is_file());
        fs::create_dir_all(active.layout().backup_index_dir()).expect("unowned backup");
        assert!(recover_build_for_rebuild(active.layout()).is_err());
        assert!(active.layout().backup_index_dir().exists());
    }
}
