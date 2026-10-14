use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::EngineError;

use super::manifest::{delete_workspace_manifest, workspace_manifest_path};

pub(crate) const ZVEC_GREP_DIRECTORY: &str = ".zvec-grep";
const WORKSPACE_INDEX_DIRECTORY: &str = "index";

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

pub(crate) fn workspace_index_location(
    root: &Path,
    index_dir: Option<&Path>,
) -> Result<WorkspaceIndexLocation, EngineError> {
    let root = canonical_path(&resolve_workspace_root(Some(root))?)?;
    match fs::metadata(&root) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(EngineError::invalid_argument(format!(
                "workspace root must be an existing directory: {}",
                root.display(),
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(workspace_io("inspect workspace root", &error)),
    }
    let home = canonical_path(
        &index_dir.map_or_else(|| root.join(ZVEC_GREP_DIRECTORY), Path::to_path_buf),
    )?;
    if root.starts_with(&home) {
        return Err(EngineError::invalid_argument(
            "index directory must not equal or contain the workspace source directory",
        ));
    }
    relative_source_root(&home, &root)?;
    Ok(WorkspaceIndexLocation {
        root,
        manifest_path: workspace_manifest_path(&home),
        index_path: home.join(WORKSPACE_INDEX_DIRECTORY),
        home,
    })
}

pub(crate) fn find_nearest_workspace(
    start: &Path,
    index_dir: Option<&Path>,
) -> Result<Option<WorkspaceIndexLocation>, EngineError> {
    if let Some(index_dir) = index_dir {
        return saved_workspace_location(start, index_dir).map(Some);
    }
    find_nearest_workspace_location(start, |location| {
        Ok(location.manifest_path.is_file()
            || super::build::has_build(&location.home)
            || location.index_path.is_dir())
    })
}

fn saved_workspace_location(
    fallback_root: &Path,
    home: &Path,
) -> Result<WorkspaceIndexLocation, EngineError> {
    let home = canonical_path(home)?;
    let root =
        super::manifest::read_source_root(&home)?.unwrap_or_else(|| fallback_root.to_path_buf());
    workspace_index_location(&root, Some(&home))
}

pub(crate) fn index_location(
    root: Option<&Path>,
    index_dir: Option<&Path>,
) -> Result<WorkspaceIndexLocation, EngineError> {
    if let Some(root) = root {
        return workspace_index_location(root, index_dir);
    }
    if let Some(home) = index_dir {
        let home = canonical_path(home)?;
        let root = super::manifest::read_source_root(&home)?.ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "index directory {} has no usable sourceRoot; provide a source directory with `zg --index <source> --index-dir <index>` (old indexes require --rebuild)",
                home.display(),
            ))
        })?;
        return workspace_index_location(&root, Some(&home));
    }
    let root = resolve_workspace_root(None)?;
    find_nearest_workspace(&root, None)?.map_or_else(|| workspace_index_location(&root, None), Ok)
}

pub(crate) fn relative_source_root(home: &Path, root: &Path) -> Result<String, EngineError> {
    let home_parts = home.components().collect::<Vec<_>>();
    let root_parts = root.components().collect::<Vec<_>>();
    let shared = home_parts
        .iter()
        .zip(&root_parts)
        .take_while(|(a, b)| a == b)
        .count();
    if !home.is_absolute() || !root.is_absolute() || shared == 0 {
        return Err(EngineError::invalid_argument(
            "source and index directories must support a relative path; place them on the same volume to keep the workspace portable",
        ));
    }
    let mut parts = vec![".."; home_parts.len() - shared];
    for component in &root_parts[shared..] {
        let value = component
            .as_os_str()
            .to_str()
            .filter(|value| !value.is_empty() && !value.contains(['\\', ':', '\0']))
            .ok_or_else(|| {
                EngineError::invalid_argument(
                    "source directory must be representable as a portable relative path",
                )
            })?;
        parts.push(value);
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

pub(crate) fn resolve_source_root(home: &Path, relative: &str) -> Result<PathBuf, EngineError> {
    if relative.is_empty() || relative.contains(['\\', ':', '\0']) {
        return Err(EngineError::invalid_argument(
            "sourceRoot must be a portable relative path",
        ));
    }
    let mut root = home.to_path_buf();
    for part in relative.split('/') {
        match part {
            "" => {
                return Err(EngineError::invalid_argument(
                    "sourceRoot must be a relative path with nonempty components",
                ));
            }
            "." => {}
            ".." if root.pop() => {}
            ".." => {
                return Err(EngineError::invalid_argument(
                    "sourceRoot escapes the filesystem root",
                ));
            }
            name => root.push(name),
        }
    }
    canonical_path(&root)
}

pub(crate) fn validate_source_root(location: &WorkspaceIndexLocation) -> Result<(), EngineError> {
    let invalid = |error: std::io::Error| {
        EngineError::from_io(
            format!(
                "source directory {} resolved from index {} is unavailable; restore the directory layout or use `zg --index <source> --index-dir <index>` to update it",
                location.root.display(),
                location.home.display(),
            ),
            &error,
        )
    };
    if !fs::metadata(&location.root).map_err(&invalid)?.is_dir() {
        return Err(EngineError::invalid_argument(format!(
            "workspace root must be an existing directory: {} (resolved from index {})",
            location.root.display(),
            location.home.display(),
        )));
    }
    // Opening the directory also detects access failures that metadata alone misses.
    fs::read_dir(&location.root).map_err(invalid)?;
    Ok(())
}

/// Resolve existing aliases, including the existing prefix of a new directory.
pub(crate) fn canonical_path(path: &Path) -> Result<PathBuf, EngineError> {
    let absolute = absolute_path(path)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            // A Windows prefix is not a complete path until RootDir is appended.
            std::path::Component::Prefix(_) => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            component => {
                resolved.push(component.as_os_str());
                match fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(workspace_io("resolve workspace path", &error)),
                }
            }
        }
    }
    Ok(resolved)
}

/// Identify another index home while traversing source directories.
pub(crate) fn is_workspace_data_directory(directory: &Path) -> bool {
    let manifest = bounded_json(&directory.join("manifest.json"));
    if manifest.is_some_and(|value| value["format"] == "zg-workspace") {
        return true;
    }
    bounded_json(&directory.join("temp/build.json")).is_some_and(|value| {
        value["generation"]
            .as_str()
            .is_some_and(|generation| uuid::Uuid::parse_str(generation).is_ok())
            && value["name"].is_string()
            && value
                .get("previous_manifest")
                .is_some_and(|previous| previous.is_null() || previous.is_string())
            && value["had_index"].is_boolean()
            && value["publishing"].is_boolean()
    })
}

fn bounded_json(path: &Path) -> Option<serde_json::Value> {
    const MAX_MARKER_BYTES: u64 = 1_048_576;
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    serde_json::from_reader(file.take(MAX_MARKER_BYTES)).ok()
}

/// Refuse to claim unrelated files when an explicit data directory is first used.
pub(crate) fn ensure_workspace_home(location: &WorkspaceIndexLocation) -> Result<(), EngineError> {
    let home = &location.home;
    if !home
        .try_exists()
        .map_err(|error| workspace_io("inspect index directory", &error))?
    {
        return Ok(());
    }
    if !home.is_dir() {
        return Err(EngineError::invalid_argument(
            "index directory is not a directory",
        ));
    }
    validate_storage_paths(home)?;
    let json = |path: PathBuf| {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    };
    let manifest = json(home.join("manifest.json"));
    let known_manifest = manifest.as_ref().is_some_and(|value| {
        value["format"] == "zg-workspace"
            || (value["name"].is_string()
                && matches!(
                    value["indexPolicy"].as_str(),
                    Some("enabled" | "disabled" | "uninitialized")
                ))
    });
    let known_build = json(home.join("temp/build.json")).is_some_and(|value| {
        value["name"].is_string()
            && value["generation"]
                .as_str()
                .is_some_and(|generation| uuid::Uuid::parse_str(generation).is_ok())
    });
    let known_storage = json(home.join("index/schema.json")).is_some_and(|value| {
        value["tables"].as_array().is_some_and(|tables| {
            !tables.is_empty()
                && tables.iter().all(|table| {
                    table["kind"].is_string()
                        && table["embedding"]["model"]["provider"].is_string()
                        && table["embedding"]["model"]["name"].is_string()
                })
        })
    });
    if known_manifest || known_build || known_storage {
        return Ok(());
    }
    for entry in
        fs::read_dir(home).map_err(|error| workspace_io("inspect index directory", &error))?
    {
        let entry = entry.map_err(|error| workspace_io("inspect index directory entry", &error))?;
        let owned = match entry.file_name().to_str() {
            Some("locks") if entry.path().is_dir() => fs::read_dir(entry.path())
                .map_err(|error| workspace_io("inspect index locks", &error))?
                .all(|file| {
                    file.is_ok_and(|file| {
                        matches!(
                            file.file_name().to_str(),
                            Some("home" | "read-cache" | "write-intent" | "binding" | "source")
                        ) && file.file_type().is_ok_and(|kind| kind.is_file())
                    })
                }),
            Some("credentials.key") => entry
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == 32),
            Some("authorization.lock") => entry.file_type().is_ok_and(|kind| kind.is_file()),
            Some("authorization.json") => json(entry.path()).is_some_and(|value| {
                let records = value.as_array().cloned().unwrap_or_else(|| vec![value]);
                !records.is_empty()
                    && records.iter().all(|record| {
                        record["key_id"].is_string()
                            && record["grant"]["capability"] == "embedding"
                            && record["signature"].is_array()
                    })
            }),
            _ => false,
        };
        if !owned {
            return Err(EngineError::invalid_argument(format!(
                "index directory {} contains unrecognized data; choose a new or empty directory",
                home.display(),
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_storage_paths(home: &Path) -> Result<(), EngineError> {
    for name in [
        "manifest.json",
        "index",
        "temp",
        "temp/new",
        "temp/old",
        "temp/build.json",
        "locks",
        "credentials.key",
        "authorization.json",
        "files.zvec",
        "index.zvec",
    ] {
        match fs::symlink_metadata(home.join(name)) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(EngineError::invalid_argument(format!(
                    "workspace data path {} must not be a symbolic link",
                    home.join(name).display(),
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(workspace_io("inspect workspace data path", &error)),
        }
    }
    Ok(())
}

pub(crate) fn reset_workspace_index(location: &WorkspaceIndexLocation) -> Result<(), EngineError> {
    ensure_workspace_home(location)?;
    super::build::drop_build_storage(&location.home)?;
    delete_workspace_manifest(&location.home)?;
    match fs::remove_file(location.home.join("credentials.key")) {
        Ok(()) => crate::utils::sync_directory(&location.home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(workspace_io("remove workspace credentials key", &error)),
    }
}

fn find_nearest_workspace_location(
    start: &Path,
    predicate: impl Fn(&WorkspaceIndexLocation) -> Result<bool, EngineError>,
) -> Result<Option<WorkspaceIndexLocation>, EngineError> {
    let mut current = resolve_workspace_root(Some(start))?;
    loop {
        let location = workspace_index_location(&current, None)?;
        if predicate(&location)? {
            return saved_workspace_location(&location.root, &location.home).map(Some);
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
        let location =
            workspace_index_location(directory.path(), None).expect("workspace location");

        assert_eq!(
            location.root,
            fs::canonicalize(directory.path()).expect("canonical root")
        );
        assert_eq!(location.home, location.root.join(".zvec-grep"));
        assert_eq!(location.manifest_path, location.home.join("manifest.json"));
        assert_eq!(location.index_path, location.home.join("index"));
    }

    #[test]
    fn canonical_source_paths_round_trip_before_and_after_creating_index_home() {
        let directory = tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        fs::create_dir(&source).expect("source directory");
        let canonical_source = fs::canonicalize(&source).expect("canonical source");
        let home = directory.path().join("indexes/new/../active");
        let location = workspace_index_location(&source, Some(&home)).expect("location");

        assert_eq!(
            canonical_path(&canonical_source).expect("resolve canonical source again"),
            canonical_source
        );
        assert_eq!(
            workspace_index_location(&canonical_source, Some(&home)).expect("canonical input"),
            location
        );
        #[cfg(windows)]
        assert_eq!(
            workspace_index_location(dunce::simplified(&canonical_source), Some(&home))
                .expect("ordinary drive path"),
            location
        );
        let root = canonical_source
            .ancestors()
            .last()
            .expect("filesystem root");
        assert_eq!(
            canonical_path(root).expect("resolve root"),
            fs::canonicalize(root).expect("canonical root")
        );
        assert!(!location.home.exists());
        let relative = relative_source_root(&location.home, &location.root).expect("relative root");
        assert_eq!(relative, "../../source");
        assert_eq!(
            resolve_source_root(&location.home, &relative).expect("restore source root"),
            canonical_source
        );

        fs::create_dir_all(&location.home).expect("create index home");
        assert_eq!(
            workspace_index_location(&canonical_source, Some(&location.home))
                .expect("reopen location"),
            location
        );
    }

    #[test]
    fn finds_the_nearest_parent_manifest() {
        let directory = tempdir().expect("temporary directory");
        let nested = directory.path().join("one/two");
        fs::create_dir_all(&nested).expect("nested directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(home.join("manifest.json"), b"{}").expect("manifest marker");

        let nearest = find_nearest_workspace(&nested, None)
            .expect("nearest workspace lookup")
            .expect("parent workspace");

        assert_eq!(
            nearest.root,
            fs::canonicalize(directory.path()).expect("canonical workspace root")
        );
    }

    #[test]
    fn finds_an_interrupted_initial_build_before_its_first_manifest() {
        let directory = tempdir().expect("workspace");
        let nested = directory.path().join("src/nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(home.join("temp")).expect("workspace home");
        fs::write(home.join("temp/build.json"), b"{}").expect("pending build marker");
        assert!(!home.join("manifest.json").exists());

        let nearest = find_nearest_workspace(&nested, None)
            .expect("workspace lookup")
            .expect("pending parent workspace");
        assert_eq!(
            nearest.home,
            fs::canonicalize(home).expect("canonical home")
        );
    }
    #[test]
    fn explicit_data_directory_keeps_source_root_and_rejects_unowned_data() {
        let directory = tempdir().expect("directory");
        let source = directory.path().join("source");
        let home = directory.path().join("data");
        fs::create_dir(&source).expect("source");
        let resolved = find_nearest_workspace(&source, Some(&home))
            .expect("resolve")
            .expect("explicit selection");
        assert_eq!(resolved.root, canonical_path(&source).expect("root"));
        assert_eq!(resolved.home, canonical_path(&home).expect("home"));
        ensure_workspace_home(&resolved).expect("new directory");
        fs::create_dir(&home).expect("home");
        fs::write(home.join("photo.jpg"), b"unrelated").expect("user data");
        assert!(ensure_workspace_home(&resolved).is_err());
        assert!(reset_workspace_index(&resolved).is_err());
        assert_eq!(
            fs::read(home.join("photo.jpg")).expect("preserved"),
            b"unrelated"
        );
        assert!(workspace_index_location(&source, Some(&source)).is_err());
        assert!(workspace_index_location(&source, Some(directory.path())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn aliases_resolve_to_one_home_but_reserved_storage_symlinks_are_rejected() {
        let directory = tempdir().expect("directory");
        let source = directory.path().join("source");
        let data = directory.path().join("data");
        fs::create_dir(&source).expect("source");
        fs::create_dir(&data).expect("data");
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&data, &alias).expect("alias");
        let first =
            workspace_index_location(&source, Some(&data.join("nested/new"))).expect("location");
        let second = workspace_index_location(&source, Some(&alias.join("nested/new")))
            .expect("alias location");
        assert_eq!(first, second);
        std::os::unix::fs::symlink(&source, data.join("index")).expect("unsafe child link");
        let location = workspace_index_location(&source, Some(&data)).expect("location");
        assert!(ensure_workspace_home(&location).is_err());
        assert!(reset_workspace_index(&location).is_err());
        assert!(source.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn source_and_index_on_different_drives_cannot_form_a_portable_bundle() {
        let error = relative_source_root(Path::new(r"C:\bundle\index"), Path::new(r"D:\source"))
            .expect_err("different drives are not portable");
        assert!(error.message().contains("same volume"));
        assert_eq!(
            relative_source_root(
                Path::new(r"C:\bundle\index"),
                Path::new(r"C:\bundle\source")
            )
            .expect("same drive"),
            "../source"
        );
    }
}
