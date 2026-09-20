//! Common filesystem utilities.

use std::{
    fs::{self, File, Permissions},
    io::{self, Write},
    path::Path,
};

use tempfile::{NamedTempFile, PersistError};

use crate::{EngineError, EngineResult};

pub(crate) fn sync_directory(path: &Path) -> EngineResult<()> {
    open_directory(nonempty_directory(path))
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            EngineError::from_io(
                format!("failed to sync directory '{}'", path.display()),
                &source,
            )
        })
}

/// Prepare a complete file, atomically publish it, then sync its parent.
#[expect(
    clippy::too_many_lines,
    reason = "keep publication stages and error context together"
)]
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> EngineResult<()> {
    let failure = |operation: &str, source: &io::Error, cleanup: Option<io::Error>| {
        let cleanup = cleanup.map_or_else(String::new, |error| {
            format!(" (cleanup also failed: {error})")
        });
        EngineError::from_io(
            format!(
                "atomic file write failed {operation} for '{}'{cleanup}",
                path.display()
            ),
            source,
        )
    };
    // file_name() ignores trailing separators and '/.'; preserve the input's meaning.
    let input = path.as_os_str().as_encoded_bytes();
    let name = path
        .file_name()
        .filter(|name| input.ends_with(name.as_encoded_bytes()));
    // Normalize ordinary Windows names before canonicalize introduces a verbatim prefix.
    #[cfg(windows)]
    let normalized = std::path::absolute(path).map_err(|e| {
        failure(
            "while resolving destination path (destination not published)",
            &e,
            None,
        )
    })?;
    #[cfg(windows)]
    let (parent, name) = (
        normalized.parent(),
        name.and_then(|_| normalized.file_name()),
    );
    #[cfg(not(windows))]
    let parent = path.parent();
    let (Some(parent), Some(name)) = (parent, name) else {
        return Err(failure(
            "during path validation (destination not published)",
            &io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination must end with a file name, not a separator or '.'",
            ),
            None,
        ));
    };
    let parent = fs::canonicalize(nonempty_directory(parent)).map_err(|e| {
        failure(
            "while resolving parent directory (destination not published)",
            &e,
            None,
        )
    })?;
    let destination = parent.join(name);
    let permissions = destination_permissions(&destination).map_err(|e| {
        failure(
            "while reading permissions (destination not published)",
            &e,
            None,
        )
    })?;
    let directory = open_directory(&parent).map_err(|e| {
        failure(
            "while opening parent directory (destination not published)",
            &e,
            None,
        )
    })?;
    directory.sync_all().map_err(|e| {
        failure(
            "during directory sync preflight (destination not published)",
            &e,
            None,
        )
    })?;

    let mut temporary = create_temporary(&parent).map_err(|e| {
        failure(
            "while creating temporary file (destination not published)",
            &e,
            None,
        )
    })?;
    if let Err(source) = prepare(temporary.as_file_mut(), bytes, permissions.as_ref()) {
        return Err(failure(
            "while preparing temporary file (destination not published)",
            &source,
            temporary.close().err(),
        ));
    }

    let file = publish(temporary, &destination, permissions.is_some()).map_err(|error| {
        failure(
            "during publication (outcome unknown)",
            &error.error,
            error.file.close().err(),
        )
    })?;
    // Windows no-clobber publication resets file attributes.
    #[cfg(windows)]
    file.sync_all().map_err(|e| {
        failure(
            "during file sync (destination published; durability unconfirmed)",
            &e,
            None,
        )
    })?;
    drop(file);
    directory.sync_all().map_err(|e| {
        failure(
            "during parent sync (destination published; durability unconfirmed)",
            &e,
            None,
        )
    })
}

fn prepare(file: &mut File, bytes: &[u8], permissions: Option<&Permissions>) -> io::Result<()> {
    file.write_all(bytes)?;
    #[cfg(unix)]
    if let Some(permissions) = permissions {
        file.set_permissions(permissions.clone())?;
        if file.metadata()?.permissions() != *permissions {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "filesystem did not preserve destination permissions",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = permissions; // Windows readonly destinations were already rejected.
    file.sync_all()
}

fn destination_permissions(path: &Path) -> io::Result<Option<Permissions>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let permissions = metadata.permissions();
            #[cfg(windows)]
            if permissions.readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination is readonly",
                ));
            }
            Ok(Some(permissions))
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination must be a regular file, not a symlink or directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn create_temporary(parent: &Path) -> io::Result<NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".atomic-");
    #[cfg(windows)]
    {
        // Keep tempfile's naming and cleanup, but omit FILE_ATTRIBUTE_TEMPORARY:
        // std::fs::rename preserves attributes when publishing the file.
        builder.make_in(parent, |path| File::create_new(path))
    }
    #[cfg(not(windows))]
    {
        builder.tempfile_in(parent) // Unix 0600, restricted by umask.
    }
}

fn publish(
    temporary: NamedTempFile,
    destination: &Path,
    overwrite: bool,
) -> Result<File, PersistError> {
    let result = if overwrite {
        // Unlike tempfile::persist, Windows std::fs::rename supports open readers.
        fs::rename(temporary.path(), destination)
    } else {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{CWD, RenameFlags, renameat_with};
            // Require a native rename; no hard-link/unlink fallback.
            renameat_with(
                CWD,
                temporary.path(),
                CWD,
                destination,
                RenameFlags::NOREPLACE,
            )
            .map_err(Into::into)
        }
        #[cfg(windows)]
        {
            return temporary.persist_noclobber(destination);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic creation is unsupported",
            ))
        }
    };
    if let Err(error) = result {
        return Err(PersistError {
            file: temporary,
            error,
        });
    }
    let mut temporary = temporary;
    temporary.disable_cleanup(true);
    Ok(temporary.into_file())
}

fn nonempty_directory(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    // Check before opening so special files such as FIFOs cannot block the call.
    if !fs::metadata(path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "path is not a directory",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // Directory access for FlushFileBuffers; ordinary ACL checks still apply.
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, ffi::OsString, io::Read};

    use super::*;

    fn assert_entries(directory: &Path, expected: &[&str]) {
        let entries: BTreeSet<_> = fs::read_dir(directory)
            .expect("read directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        assert_eq!(entries, expected.iter().map(OsString::from).collect());
    }

    fn assert_error(error: &EngineError, operation: &str, path: &Path) {
        assert!(error.message().contains(operation), "{error}");
        assert!(
            error.message().contains(&format!("'{}'", path.display())),
            "{error}"
        );
        assert!(error.origin().file.ends_with("utils/filesystem.rs"));
        assert!(error.origin().line > 0);
    }

    #[test]
    fn creates_and_replaces_english_and_chinese_files_without_residue() {
        let root = tempfile::tempdir().expect("temporary directory");
        for (directory, name, initial, replacement) in [
            (
                "english",
                "record.txt",
                "long original English contents",
                "替换内容",
            ),
            (
                "中文目录",
                "中文 记录.txt",
                "初始",
                "long replacement English contents",
            ),
        ] {
            let parent = root.path().join(directory);
            fs::create_dir(&parent).expect("prepare parent directory");
            let path = parent.join(name);
            for contents in [initial, replacement] {
                atomic_write(&path, contents.as_bytes()).expect("atomic write");
                assert_eq!(fs::read(&path).expect("read contents"), contents.as_bytes());
                assert_entries(&parent, &[name]);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
                    assert_eq!(
                        fs::metadata(&path)
                            .expect("file metadata")
                            .file_attributes()
                            & FILE_ATTRIBUTE_TEMPORARY,
                        0,
                    );
                }
            }
        }
    }

    #[test]
    fn creates_empty_files_and_replaces_contents_with_empty_bytes() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("record");
        for contents in [b"".as_slice(), b"previous contents", b""] {
            atomic_write(&path, contents).expect("atomic write");
            assert_eq!(fs::read(&path).expect("read contents"), contents);
            assert_entries(root.path(), &["record"]);
        }
    }

    #[test]
    fn rejects_invalid_paths_without_creating_parents() {
        let root = tempfile::tempdir().expect("temporary directory");
        let file = root.path().join("file");
        fs::write(&file, b"unchanged").expect("existing parent file");
        let missing = root.path().join("missing/record");
        let not_directory = file.join("record");
        let trailing_separator = file.join("");
        let trailing_dot = file.join(".");
        let missing_with_separator = root.path().join("new/");
        let missing_with_dot = root.path().join("new/.");
        let filesystem_root = root.path().ancestors().last().expect("filesystem root");
        for path in [
            Path::new(""),
            Path::new("."),
            Path::new(".."),
            filesystem_root,
            &missing,
            &not_directory,
            &trailing_separator,
            &trailing_dot,
            &missing_with_separator,
            &missing_with_dot,
        ] {
            let error = atomic_write(path, b"new").expect_err("invalid destination");
            assert_error(&error, "atomic file write failed", path);
            assert!(error.message().contains("destination not published"));
            assert_eq!(
                fs::read(&file).expect("unchanged parent file"),
                b"unchanged"
            );
            assert_entries(root.path(), &["file"]);
        }
    }

    #[test]
    fn rejects_directory_destinations() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("directory");
        fs::create_dir(&path).expect("existing directory");
        let error = atomic_write(&path, b"new").expect_err("directory destination");
        assert_error(
            &error,
            "atomic file write failed while reading permissions",
            &path,
        );
        assert!(path.is_dir());
        assert_entries(&path, &[]);
        assert_entries(root.path(), &["directory"]);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_without_changing_the_link_or_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary directory");
        let target = root.path().join("target");
        let missing = root.path().join("missing");
        let link = root.path().join("link");
        fs::write(&target, b"unchanged").expect("symlink target");
        for destination in [&target, &missing] {
            symlink(destination, &link).expect("create symlink");
            let error = atomic_write(&link, b"new").expect_err("symlink destination");
            assert_error(
                &error,
                "atomic file write failed while reading permissions",
                &link,
            );
            assert_eq!(fs::read_link(&link).expect("unchanged link"), *destination);
            assert_eq!(fs::read(&target).expect("unchanged target"), b"unchanged");
            assert_entries(root.path(), &["link", "target"]);
            fs::remove_file(&link).expect("remove test symlink");
        }
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_files_and_preserves_existing_modes() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary directory");
        fs::set_permissions(root.path(), Permissions::from_mode(0o750)).expect("parent mode");
        let path = root.path().join("record");
        atomic_write(&path, b"initial").expect("create private file");
        let mode = fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode & !0o600, 0); // umask may further restrict the new file.
        for mode in [0o640, 0o444] {
            fs::set_permissions(&path, Permissions::from_mode(mode)).expect("existing file mode");
            atomic_write(&path, b"replacement").expect("replace with preserved mode");
            assert_eq!(
                fs::metadata(&path)
                    .expect("file metadata")
                    .permissions()
                    .mode()
                    & 0o7777,
                mode
            );
            assert_eq!(
                fs::read(&path).expect("replacement contents"),
                b"replacement"
            );
            assert_eq!(
                fs::metadata(root.path())
                    .expect("parent metadata")
                    .permissions()
                    .mode()
                    & 0o7777,
                0o750
            );
            assert_entries(root.path(), &["record"]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn preserves_windows_path_normalization() {
        let root = tempfile::tempdir().expect("temporary directory");
        let parent = dunce::simplified(root.path());
        let destination = parent.join("record");
        for name in ["record.", "record "] {
            let path = parent.join(name);
            for contents in [b"initial".as_slice(), b"replacement"] {
                atomic_write(&path, contents).expect("write normalized path");
                assert_eq!(fs::read(&path).expect("read original path"), contents);
                assert_eq!(
                    fs::read(&destination).expect("read normalized path"),
                    contents
                );
                assert_entries(root.path(), &["record"]);
            }
            fs::remove_file(&destination).expect("remove test file");
        }

        let literal = fs::canonicalize(root.path())
            .expect("verbatim parent")
            .join("literal.");
        atomic_write(&literal, b"literal name").expect("write verbatim path");
        assert_eq!(
            fs::read(&literal).expect("read verbatim path"),
            b"literal name"
        );
        assert_entries(root.path(), &["literal."]);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_readonly_files_without_changing_contents_or_permissions() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("record");
        fs::write(&path, b"unchanged").expect("existing file");
        let original = fs::metadata(&path).expect("file metadata").permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly.clone()).expect("readonly file");
        let result = atomic_write(&path, b"new");
        let metadata = fs::metadata(&path);
        fs::set_permissions(&path, original).expect("restore permissions for cleanup");
        let error = result.expect_err("readonly destination");
        assert_eq!(error.code(), EngineError::PERMISSION_DENIED);
        assert_error(
            &error,
            "atomic file write failed while reading permissions",
            &path,
        );
        assert_eq!(metadata.expect("unchanged file").permissions(), readonly);
        assert_eq!(fs::read(&path).expect("unchanged contents"), b"unchanged");
        assert_entries(root.path(), &["record"]);
    }

    #[test]
    fn replacement_preserves_contents_visible_to_an_open_reader() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("record");
        fs::write(&path, b"original").expect("existing file");
        let mut reader = File::open(&path).expect("old reader");
        atomic_write(&path, b"replacement").expect("replace with reader open");
        let mut contents = Vec::new();
        reader
            .read_to_end(&mut contents)
            .expect("read old contents");
        assert_eq!(contents, b"original");
        assert_eq!(fs::read(&path).expect("read new contents"), b"replacement");
        assert_entries(root.path(), &["record"]);
    }

    #[test]
    fn no_clobber_publication_preserves_a_competing_file() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("record");
        let mut temporary = create_temporary(root.path()).expect("temporary file");
        prepare(temporary.as_file_mut(), b"our contents", None).expect("prepare contents");
        fs::write(&path, b"competitor").expect("competing creator");
        let error = publish(temporary, &path, false).expect_err("must not replace competitor");
        error.file.close().expect("clean up unpublished file");
        assert_eq!(error.error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).expect("competitor contents"), b"competitor");
        assert_entries(root.path(), &["record"]);
    }

    #[test]
    fn syncs_existing_directories_and_reports_missing_ones() {
        let root = tempfile::tempdir().expect("temporary directory");
        sync_directory(root.path()).expect("sync existing directory");
        let file = root.path().join("record");
        fs::write(&file, b"unchanged").expect("ordinary file");
        let error = sync_directory(&file).expect_err("not a directory");
        assert_error(&error, "failed to sync directory", &file);
        assert_eq!(fs::read(&file).expect("unchanged file"), b"unchanged");
        let missing = root.path().join("missing");
        let error = sync_directory(&missing).expect_err("missing directory");
        assert_eq!(error.code(), EngineError::NOT_FOUND);
        assert_error(&error, "failed to sync directory", &missing);
        assert!(!error.message().contains("atomic file write"));
        assert_entries(root.path(), &["record"]);
    }
}
