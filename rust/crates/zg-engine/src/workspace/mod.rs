//! Workspace discovery, persisted state, and process coordination.

use std::path::{Path, PathBuf};

use crate::{EngineError, domain::WorkspaceLayout};

pub(crate) mod credentials;
pub(crate) mod index;
pub(crate) mod lock;
pub(crate) mod manifest;

/// Compatibility version of the workspace index contents.
pub(crate) const CURRENT_INDEX_VERSION: u32 = 2;

/// Finds the nearest workspace at or above the given directory.
pub(crate) fn find_nearest_workspace(
    directory: &Path,
) -> Result<Option<(PathBuf, WorkspaceLayout)>, EngineError> {
    let mut root = directory.to_path_buf();
    loop {
        let layout = WorkspaceLayout::new(root.join(".zvec-grep"))?;
        if layout.has_manifest()? || layout.has_build_record()? {
            return Ok(Some((root, layout)));
        }
        if layout.has_index_data()? {
            let Some(_lock) =
                lock::try_acquire_workspace_read(&root, &layout, "workspace.discover")?
            else {
                return Ok(Some((root, layout)));
            };
            if layout.has_manifest()? || layout.has_build_record()? {
                return Ok(Some((root, layout)));
            }
            if layout.has_index_data()? {
                return Err(EngineError::storage_failure(format!(
                    "workspace '{}' contains index data without a manifest or build record; rebuild the index with `zg --index --rebuild`",
                    root.display(),
                )));
            }
        }
        if !root.pop() {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::find_nearest_workspace;
    use crate::domain::WorkspaceLayout;
    use crate::utils::resolve_directory;

    #[test]
    fn layout_construction_checks_storage_without_creating_it() {
        let directory = tempdir().expect("workspace");
        let root = directory.path().to_path_buf();
        let layout =
            WorkspaceLayout::new(root.join(".zvec-grep")).expect("missing storage is allowed");
        assert!(!layout.storage_dir().exists());

        fs::write(layout.storage_dir(), b"not a directory").expect("invalid storage entry");
        assert!(WorkspaceLayout::new(root.join(".zvec-grep")).is_err());
        fs::remove_file(layout.storage_dir()).expect("remove invalid storage entry");
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        assert_eq!(
            WorkspaceLayout::new(root.join(".zvec-grep")).expect("existing storage"),
            layout
        );
        assert!(
            fs::read_dir(layout.storage_dir())
                .expect("empty storage")
                .next()
                .is_none()
        );
    }

    #[test]
    fn layout_queries_allow_missing_entries_and_find_staged_data() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        assert!(!layout.has_manifest().expect("missing manifest"));
        assert!(!layout.has_build_record().expect("missing build record"));
        assert!(!layout.has_index_data().expect("missing index data"));
        assert!(!layout.storage_dir().exists());

        fs::create_dir_all(layout.locks_dir()).expect("locks directory");
        fs::write(layout.workspace_lock_path(), b"").expect("lock file");
        fs::write(layout.authorization_path(), b"{}").expect("authorization file");
        fs::write(layout.credentials_key_path(), b"key").expect("credentials key");
        assert!(!layout.has_index_data().expect("no index data"));

        fs::create_dir(layout.temp_dir()).expect("temporary directory");
        fs::write(layout.staged_manifest_path(), b"{}").expect("staged manifest");
        assert!(layout.has_index_data().expect("staged data"));
        assert!(!layout.has_manifest().expect("no published manifest"));
        assert!(!layout.has_build_record().expect("no build record"));
        fs::write(layout.index_dir(), b"not a directory").expect("invalid index entry");
        assert!(layout.has_index_data().is_err());
    }

    #[test]
    fn malformed_workspace_marker_does_not_fall_back_to_a_parent() {
        let directory = tempdir().expect("workspace");
        let parent = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        fs::create_dir(parent.storage_dir()).expect("parent storage");
        fs::write(parent.manifest_path(), b"{}").expect("parent manifest");
        let child = WorkspaceLayout::new(directory.path().join("child").join(".zvec-grep"))
            .expect("child layout");
        fs::create_dir_all(child.manifest_path()).expect("wrong manifest type");

        assert!(find_nearest_workspace(&directory.path().join("child")).is_err());
    }

    #[test]
    fn finds_parent_workspaces_from_manifest_or_pending_build() {
        for marker in ["manifest.json", "temp/build.json"] {
            let directory = tempdir().expect("workspace");
            let source = resolve_directory(Some(directory.path())).expect("source directory");
            let nested = source.join("src/nested");
            fs::create_dir_all(&nested).expect("nested directory");
            let marker_path = source.join(".zvec-grep").join(marker);
            fs::create_dir_all(marker_path.parent().expect("marker parent"))
                .expect("workspace data directory");
            fs::write(&marker_path, b"{}").expect("workspace marker");

            let (found_root, _) = find_nearest_workspace(&nested)
                .expect("workspace lookup")
                .expect("parent workspace");
            assert_eq!(found_root, source, "{marker}");
        }
    }

    #[test]
    fn nearest_workspace_or_orphaned_index_never_falls_back_to_its_parent() {
        let directory = tempdir().expect("workspace");
        let root = resolve_directory(Some(directory.path())).expect("source root");
        let parent = WorkspaceLayout::new(root.join(".zvec-grep")).expect("parent");
        fs::create_dir(parent.storage_dir()).expect("storage");
        fs::write(parent.manifest_path(), b"{}").expect("parent marker");
        let nested = root.join("child/src");
        fs::create_dir_all(&nested).expect("nested source");
        let child = WorkspaceLayout::new(root.join("child").join(".zvec-grep")).expect("child");
        fs::create_dir(child.storage_dir()).expect("child storage");
        fs::write(child.authorization_path(), b"{}").expect("authorization only");
        assert_eq!(
            find_nearest_workspace(&nested).expect("parent discovery"),
            Some((root.clone(), parent))
        );
        fs::write(child.manifest_path(), b"broken metadata").expect("child marker");
        assert_eq!(
            find_nearest_workspace(&nested).expect("nearest boundary"),
            Some((root.join("child"), child.clone()))
        );
        fs::remove_file(child.manifest_path()).expect("remove child marker");
        fs::create_dir(child.index_dir()).expect("orphaned index");
        assert!(find_nearest_workspace(&nested).is_err());
    }

    #[tokio::test]
    async fn discovery_defers_orphan_checks_until_the_writer_releases_storage() {
        use super::lock::{LockMode, LockWait};

        let directory = tempdir().expect("workspace");
        let root = resolve_directory(Some(directory.path())).expect("source root");
        let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
        let writer = LockWait::new(None, None)
            .expect("wait")
            .acquire(&root, &layout, LockMode::Exclusive, "index")
            .await
            .expect("writer");
        fs::create_dir_all(layout.staged_index_dir()).expect("index data");
        assert_eq!(
            find_nearest_workspace(&root).expect("writer may still publish a marker"),
            Some((root.clone(), layout))
        );
        drop(writer);
        let error = find_nearest_workspace(&root).expect_err("confirmed orphaned index");
        assert!(
            error
                .message()
                .contains("without a manifest or build record")
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_aliases_resolve_and_data_paths_are_checked_separately() {
        use std::os::unix::fs::symlink;
        let directory = tempdir().expect("directory");
        let source = directory.path().join("source");
        fs::create_dir(&source).expect("source");
        let alias = directory.path().join("alias");
        symlink(&source, &alias).expect("source alias");
        assert_eq!(
            WorkspaceLayout::new(
                resolve_directory(Some(&source))
                    .expect("source")
                    .join(".zvec-grep")
            )
            .expect("source layout"),
            WorkspaceLayout::new(
                resolve_directory(Some(&alias))
                    .expect("alias")
                    .join(".zvec-grep")
            )
            .expect("alias layout")
        );
        let layout = WorkspaceLayout::new(source.join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir();
        let external = directory.path().join("external");
        fs::create_dir(&external).expect("external");
        symlink(&external, home).expect("home alias");
        assert!(resolve_directory(Some(&source)).is_ok());
        assert!(WorkspaceLayout::new(source.join(".zvec-grep")).is_err());
        fs::remove_file(home).expect("remove alias");
        fs::create_dir_all(home.join("locks")).expect("locks");
        for name in ["index", "temp", "manifest.json"] {
            symlink(&external, home.join(name)).expect("data alias");
            assert!(resolve_directory(Some(&source)).is_ok(), "{name}");
            assert!(layout.has_index_data().is_err(), "{name}");
            fs::remove_file(home.join(name)).expect("remove data alias");
        }
    }

    #[cfg(unix)]
    #[test]
    fn layout_queries_check_parents_and_ignore_unrelated_entries() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        let home = layout.storage_dir();
        let external = tempdir().expect("external directory");
        fs::create_dir(home).expect("workspace home");
        symlink(external.path(), home.join("temp")).expect("temporary directory alias");

        assert!(layout.has_build_record().is_err());
        assert!(!layout.has_manifest().expect("unrelated manifest"));
        fs::write(home.join("manifest.json"), b"{}").expect("manifest marker");
        let root = resolve_directory(Some(directory.path())).expect("workspace root");
        assert!(
            find_nearest_workspace(&root)
                .expect("discover workspace")
                .is_some()
        );
        assert!(!external.path().join("build.json").exists());
    }
}
