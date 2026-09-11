use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::{EngineError, storage::spi::WorkspaceIndexStorageFactory};

use super::manifest::{delete_workspace_manifest, workspace_manifest_path};

pub(crate) const ZVEC_GREP_DIRECTORY: &str = ".zvec-grep";
const WORKSPACE_INDEX_DIRECTORY: &str = "storage";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceIndexLocation {
    pub root: PathBuf,
    pub home: PathBuf,
    pub manifest_path: PathBuf,
    pub index_path: PathBuf,
}

pub(crate) fn resolve_workspace_root(root: Option<&Path>) -> Result<PathBuf, EngineError> {
    let root = match root {
        Some(root) => root.to_path_buf(),
        None => {
            env::current_dir().map_err(|error| workspace_io("resolve current directory", &error))?
        }
    };
    absolute_path(&root)
}

pub(crate) fn workspace_index_location(root: &Path) -> Result<WorkspaceIndexLocation, EngineError> {
    let resolved_root = resolve_workspace_root(Some(root))?;
    let requested_home = resolved_root.join(ZVEC_GREP_DIRECTORY);
    let home = if requested_home.exists() {
        fs::canonicalize(&requested_home)
            .map_err(|error| workspace_io("canonicalize workspace home", &error))?
    } else {
        requested_home
    };
    let canonical_root = home
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(resolved_root);

    Ok(WorkspaceIndexLocation {
        root: canonical_root,
        manifest_path: workspace_manifest_path(&home),
        index_path: home.join(WORKSPACE_INDEX_DIRECTORY),
        home,
    })
}

pub(crate) fn find_nearest_workspace(
    start: &Path,
) -> Result<Option<WorkspaceIndexLocation>, EngineError> {
    find_nearest_workspace_location(start, |location| Ok(location.manifest_path.is_file()))
}

pub(crate) fn reset_workspace_index(
    location: &WorkspaceIndexLocation,
    storage_factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    storage_factory.delete(&location.home)?;
    delete_workspace_manifest(&location.home)
}

fn find_nearest_workspace_location(
    start: &Path,
    predicate: impl Fn(&WorkspaceIndexLocation) -> Result<bool, EngineError>,
) -> Result<Option<WorkspaceIndexLocation>, EngineError> {
    let mut current = resolve_workspace_root(Some(start))?;
    loop {
        let location = workspace_index_location(&current)?;
        if predicate(&location)? {
            return Ok(Some(location));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, EngineError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    env::current_dir()
        .map(|current| current.join(path))
        .map_err(|error| workspace_io("resolve workspace root", &error))
}

#[track_caller]
fn workspace_io(operation: &str, error: &std::io::Error) -> EngineError {
    EngineError::from_io(operation, error)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn creates_workspace_paths() {
        let directory = tempdir().expect("temporary directory");
        let location = workspace_index_location(directory.path()).expect("workspace location");

        assert_eq!(location.root, directory.path());
        assert_eq!(location.home, directory.path().join(".zvec-grep"));
        assert_eq!(location.manifest_path, location.home.join("manifest.json"));
        assert_eq!(location.index_path, location.home.join("storage"));
    }

    #[test]
    fn finds_the_nearest_parent_manifest() {
        let directory = tempdir().expect("temporary directory");
        let nested = directory.path().join("one/two");
        fs::create_dir_all(&nested).expect("nested directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(home.join("manifest.json"), b"{}").expect("manifest marker");

        let nearest = find_nearest_workspace(&nested)
            .expect("nearest workspace lookup")
            .expect("parent workspace");

        assert_eq!(
            nearest.root,
            fs::canonicalize(directory.path()).expect("canonical workspace root")
        );
    }
}
