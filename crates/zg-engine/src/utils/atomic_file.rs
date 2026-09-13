//! Atomic writes with file and parent-directory synchronization.
//!
//! Existing files retain Unix mode bits or Windows readonly status, as expressed
//! by `std::fs::Permissions`. Windows readonly destinations are rejected. Owner,
//! group and ACL preservation are outside this contract; Windows inherits the
//! directory's ACL. New files use tempfile defaults (Unix 0600, subject to umask).
//! Callers must coordinate writers and permission changes, keep the parent stable,
//! and initialize missing parents with `create_directories`.

use std::{
    fs::{self, File, Permissions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use tempfile::{NamedTempFile, PersistError};

/// Publish a complete file, then synchronize its directory entry.
///
/// Symlinks and non-regular destinations are rejected. Unsupported synchronization
/// fails explicitly, including a preflight before publication. Errors identify
/// whether publication happened or its outcome is uncertain; no rollback follows
/// publication. Normal errors and unwinding clean up unpublished temporary files.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_with(path, bytes, |_| Ok(()))
}

fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    mut before: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let mut stage = "validate destination";
    let mut publication = "destination not published";
    let mut temporary = None;
    let result = (|| {
        let mut step = |name| {
            stage = name;
            before(name)
        };
        step("validate destination")?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
        })?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent")
        })?;
        let parent = fs::canonicalize(nonempty_directory(parent))?;
        let destination = parent.join(name);
        step("read destination permissions")?;
        let permissions = destination_permissions(&destination)?;
        step("open parent directory")?;
        let directory = open_directory(&parent)?;
        step("check parent directory synchronization")?;
        directory.sync_all()?;
        step("create temporary file")?;
        temporary = Some(create_temporary(&parent)?);
        let file = temporary.as_mut().expect("temporary file was created");
        step("write temporary file")?;
        file.write_all(bytes)?;
        step("restore permissions")?;
        // Windows readonly targets were rejected above; new temporary files are writable.
        #[cfg(unix)]
        if let Some(permissions) = &permissions {
            file.as_file().set_permissions(permissions.clone())?;
            if file.as_file().metadata()?.permissions() != *permissions {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "filesystem did not preserve destination permissions",
                ));
            }
        }
        step("sync temporary file")?;
        file.as_file().sync_all()?;
        step("publish destination")?;
        publication = "publication outcome unknown";
        let file = match publish(
            temporary
                .take()
                .expect("temporary file remains unpublished"),
            &destination,
            permissions.is_some(),
        ) {
            Ok(file) => file,
            Err(error) => {
                temporary = Some(error.file);
                return Err(error.error);
            }
        };
        publication = "destination published; durability unconfirmed";
        // Windows no-clobber publication resets file attributes. Sync those
        // metadata updates before synchronizing the directory entry below.
        #[cfg(windows)]
        {
            step("sync published file")?;
            file.sync_all()?;
        }
        drop(file);
        step("sync parent directory")?;
        directory.sync_all()
    })();
    result.map_err(|source| {
        let cleanup = temporary.and_then(|file| file.close().err());
        failure(
            path,
            format!("during {stage} ({publication})"),
            source,
            cleanup,
        )
    })
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
        // std::fs::rename keeps attributes, so create a normal file rather than
        // leaving FILE_ATTRIBUTE_TEMPORARY on the published destination.
        builder.make_in(parent, |path| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
        })
    }
    #[cfg(not(windows))]
    {
        builder.tempfile_in(parent)
    }
}

fn publish(
    temporary: NamedTempFile,
    destination: &Path,
    overwrite: bool,
) -> Result<File, PersistError> {
    let result = if overwrite {
        // Rust's Windows rename supports replacing a file held open by readers;
        // tempfile::persist only uses MoveFileExW, which can return AccessDenied.
        fs::rename(temporary.path(), destination)
    } else {
        // tempfile's Unix no-clobber operation can fall back to hard-link + unlink.
        // Require a native rename instead, and preserve a competing creator's file.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{CWD, RenameFlags, renameat_with};
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
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // FlushFileBuffers requires a write handle. The flag permits opening a
        // directory; it does not grant privileges or bypass its ACL.
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

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    open_directory(nonempty_directory(path))
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            failure(
                path,
                "during directory synchronization".into(),
                source,
                None,
            )
        })
}

/// Create missing directories and sync each new directory followed by its parent.
/// Existing directories retain their permissions and must already be durable.
/// Unix creation uses 0700 (subject to umask); Windows uses inherited permissions.
/// Failed synchronization removes only this call's own new, empty directory.
/// If cleanup also fails, sync the remaining directory and its parent before reuse.
pub(crate) fn create_directories(path: &Path) -> io::Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    create_directories_with(path, &mut |path| builder.create(path), &mut sync_directory)
        .map_err(|source| failure(path, "during directory initialization".into(), source, None))
}

fn create_directories_with(
    path: &Path,
    create: &mut impl FnMut(&Path) -> io::Result<()>,
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = nonempty_directory(path.parent().unwrap_or(Path::new(".")));
    create_directories_with(parent, create, sync)?;
    let created = match create(path) {
        Ok(()) => true,
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path)?.is_dir() =>
        {
            false
        }
        Err(error) => return Err(error),
    };
    // Winning a mkdir race does not establish that the other creator synced it.
    sync(path).and_then(|()| sync(parent)).map_err(|source| {
        let cleanup = if created {
            fs::remove_dir(path).err()
        } else {
            None
        };
        failure(
            path,
            "during new directory synchronization".into(),
            source,
            cleanup,
        )
    })
}

#[derive(Debug, thiserror::Error)]
#[error("atomic file write failed {operation} for '{}': {source}{cleanup}", path.display())]
struct FileOperationError {
    operation: String,
    path: PathBuf,
    source: io::Error,
    cleanup: String,
}

fn failure(
    path: &Path,
    operation: String,
    source: io::Error,
    cleanup: Option<io::Error>,
) -> io::Error {
    io::Error::new(
        source.kind(),
        FileOperationError {
            operation,
            path: path.to_owned(),
            source,
            cleanup: cleanup.map_or_else(String::new, |error| {
                format!("; cleanup also failed: {error}")
            }),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_entries(directory: &Path, expected: &[&str]) {
        let mut actual: Vec<_> = fs::read_dir(directory)
            .expect("read directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        actual.sort();
        let mut expected: Vec<_> = expected
            .iter()
            .map(|name| std::ffi::OsString::from(*name))
            .collect();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn creates_and_replaces_unicode_path_with_shorter_and_empty_contents() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("设置-🦀.json");
        for bytes in [
            "long contents\n".repeat(100).into_bytes(),
            b"short".to_vec(),
            vec![],
        ] {
            atomic_write(&path, &bytes).expect("atomic write");
            assert_eq!(fs::read(&path).expect("published contents"), bytes);
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;

                const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
                assert_eq!(
                    fs::metadata(&path)
                        .expect("read published file attributes")
                        .file_attributes()
                        & FILE_ATTRIBUTE_TEMPORARY,
                    0,
                    "published file must not retain the temporary attribute"
                );
            }
            assert_entries(directory.path(), &["设置-🦀.json"]);
        }
    }

    #[test]
    fn replacement_preserves_contents_visible_through_an_open_old_handle() {
        use std::io::Read;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        atomic_write(&path, b"original contents").expect("write original contents");
        let mut old_handle = File::open(&path).expect("open original file");

        atomic_write(&path, b"replacement contents").expect("replace with original handle open");
        assert_eq!(
            fs::read(&path).expect("read replacement contents"),
            b"replacement contents"
        );
        let mut original = Vec::new();
        old_handle
            .read_to_end(&mut original)
            .expect("read original contents through open handle");
        assert_eq!(original, b"original contents");
        assert_entries(directory.path(), &["record"]);
    }

    #[test]
    fn concurrent_reader_observes_only_complete_versions() {
        use std::sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
        };

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        let versions = [vec![b'a'; 8192], vec![b'b'; 4096]];
        atomic_write(&path, &versions[0]).expect("write initial version");
        let started = Barrier::new(2);
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                started.wait();
                let mut reads = 0;
                loop {
                    let observed = fs::read(&path).expect("read during replacement");
                    assert!(
                        versions.contains(&observed),
                        "reader observed partial contents"
                    );
                    reads += 1;
                    if finished.load(Ordering::Acquire) {
                        return reads;
                    }
                }
            });
            started.wait();
            let writes = std::panic::catch_unwind(|| {
                (0..32).try_for_each(|index| atomic_write(&path, &versions[index % 2]))
            });
            finished.store(true, Ordering::Release);
            let reads = reader.join().expect("reader did not panic");
            writes
                .expect("writer did not panic")
                .expect("publish every version");
            assert!(reads > 0);
        });
        assert_entries(directory.path(), &["record"]);
    }

    #[cfg(unix)]
    #[test]
    fn new_files_and_directories_are_private_and_existing_modes_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("private/nested");
        create_directories(&parent).expect("create private directories");
        for path in [&parent, &directory.path().join("private")] {
            assert_eq!(
                fs::metadata(path)
                    .expect("read directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let path = parent.join("record");
        atomic_write(&path, b"initial").expect("create private file");
        assert_eq!(
            fs::metadata(&path)
                .expect("read destination metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for mode in [0o640, 0o400, 0o2640, 0o4640] {
            fs::set_permissions(&parent, Permissions::from_mode(0o750)).expect("set parent mode");
            fs::set_permissions(&path, Permissions::from_mode(mode)).expect("set destination mode");
            create_directories(&parent).expect("existing directory");
            atomic_write(&path, b"replacement").expect("replace with original mode");
            assert_eq!(
                fs::read(&path).expect("read destination contents"),
                b"replacement"
            );
            assert_eq!(
                fs::metadata(&path)
                    .expect("read destination metadata")
                    .permissions()
                    .mode()
                    & 0o7777,
                mode
            );
            assert_eq!(
                fs::metadata(&parent)
                    .expect("read parent metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o750
            );
            assert_entries(&parent, &["record"]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn readonly_destination_keeps_contents_attributes_and_no_temporary_files() {
        use std::os::windows::fs::MetadataExt;

        struct RestorePermissions(PathBuf, Permissions);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, self.1.clone());
            }
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("readonly");
        fs::write(&path, b"original").expect("write original contents");
        let original = fs::metadata(&path)
            .expect("read destination metadata")
            .permissions();
        let _restore = RestorePermissions(path.clone(), original.clone());
        let mut readonly = original;
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).expect("set destination readonly");
        let attributes = fs::metadata(&path)
            .expect("read destination metadata")
            .file_attributes();
        let error = atomic_write(&path, b"replacement").expect_err("reject readonly");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("read destination permissions"));
        assert!(error.to_string().contains("destination not published"));
        assert_eq!(
            fs::read(&path).expect("read destination contents"),
            b"original"
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("read destination metadata")
                .file_attributes(),
            attributes
        );
        assert_entries(directory.path(), &["readonly"]);
    }

    #[test]
    fn rejects_directories_missing_parents_and_symlinks_with_path_context() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let nested = directory.path().join("directory");
        fs::create_dir(&nested).expect("create directory destination");
        let absent = directory.path().join("missing/record");
        for (path, stage) in [
            (&nested, "read destination permissions"),
            (&absent, "validate destination"),
        ] {
            let error = atomic_write(path, b"new").expect_err("invalid destination");
            let message = error.to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(message.contains(stage), "{message}");
            assert!(message.contains("destination not published"), "{message}");
        }
        assert!(!absent.exists());
        assert_entries(directory.path(), &["directory"]);
        #[cfg(unix)]
        {
            let target = directory.path().join("target");
            let link = directory.path().join("link");
            fs::write(&target, b"original").expect("write symlink target");
            std::os::unix::fs::symlink(&target, &link).expect("create symlink destination");
            let error = atomic_write(&link, b"new").expect_err("reject symlink");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(&link.display().to_string()));
            assert!(
                fs::symlink_metadata(&link)
                    .expect("read symlink metadata")
                    .is_symlink()
            );
            assert_eq!(fs::read(&target).expect("read symlink target"), b"original");
            assert_entries(directory.path(), &["directory", "link", "target"]);
        }
    }

    #[test]
    fn failures_before_publication_preserve_old_contents_and_remove_temporary_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        fs::write(&path, b"original").expect("write original contents");
        for failed_stage in [
            "validate destination",
            "read destination permissions",
            "open parent directory",
            "check parent directory synchronization",
            "create temporary file",
            "write temporary file",
            "restore permissions",
            "sync temporary file",
            "publish destination",
        ] {
            let error = atomic_write_with(&path, b"new", |stage| {
                if stage == failed_stage {
                    Err(io::Error::from_raw_os_error(5))
                } else {
                    Ok(())
                }
            })
            .expect_err("injected failure");
            let message = error.to_string();
            assert!(message.contains(failed_stage), "{message}");
            assert!(message.contains("destination not published"), "{message}");
            assert!(message.contains(&path.display().to_string()), "{message}");
            let source = &error
                .get_ref()
                .and_then(|source| source.downcast_ref::<FileOperationError>())
                .expect("structured file-operation error")
                .source;
            assert_eq!(source.raw_os_error(), Some(5));
            assert_eq!(source.kind(), io::Error::from_raw_os_error(5).kind());
            assert_eq!(error.kind(), source.kind());
            assert_eq!(
                fs::read(&path).expect("read destination contents"),
                b"original"
            );
            assert_entries(directory.path(), &["record"]);
        }
    }

    #[test]
    fn unwinding_cleans_unpublished_temporary_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        fs::write(&path, b"original").expect("write original contents");
        let result = std::panic::catch_unwind(|| {
            let _ = atomic_write_with(&path, b"new", |stage| {
                assert_ne!(stage, "restore permissions", "injected panic");
                Ok(())
            });
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).expect("read destination contents"),
            b"original"
        );
        assert_entries(directory.path(), &["record"]);
    }

    #[test]
    fn parent_sync_failure_reports_published_contents_without_rollback() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        fs::write(&path, b"original").expect("write original contents");
        let error = atomic_write_with(&path, b"published", |stage| {
            if stage == "sync parent directory" {
                Err(io::Error::other("injected sync failure"))
            } else {
                Ok(())
            }
        })
        .expect_err("post-publication sync failure");
        let message = error.to_string();
        assert!(message.contains("sync parent directory"), "{message}");
        assert!(
            message.contains("destination published; durability unconfirmed"),
            "{message}"
        );
        assert_eq!(
            fs::read(&path).expect("read destination contents"),
            b"published"
        );
        assert_entries(directory.path(), &["record"]);
    }

    #[test]
    fn competing_creator_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("record");
        let error = atomic_write_with(&path, b"our contents", |stage| {
            if stage == "publish destination" {
                fs::write(&path, b"competitor")?;
            }
            Ok(())
        })
        .expect_err("competing creator wins");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("publish destination"));
        assert_eq!(
            fs::read(&path).expect("read destination contents"),
            b"competitor"
        );
        assert_entries(directory.path(), &["record"]);
    }

    #[test]
    fn existing_directory_requires_neither_creation_nor_synchronization() {
        let directory = tempfile::tempdir().expect("temporary directory");
        create_directories_with(
            directory.path(),
            &mut |_| panic!("existing directory must not be created"),
            &mut |_| panic!("existing directory must already be durable"),
        )
        .expect("existing directory");
    }

    #[test]
    fn nested_directory_syncs_each_new_directory_before_its_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("first");
        let child = parent.join("second");
        let mut synchronized = Vec::new();
        create_directories_with(&child, &mut |path| fs::create_dir(path), &mut |path| {
            synchronized.push(path.to_owned());
            Ok(())
        })
        .expect("create nested directories");
        assert_eq!(
            synchronized,
            [parent.clone(), directory.path().to_owned(), child, parent]
        );
    }

    #[test]
    fn directory_sync_failure_cleans_own_creation_but_preserves_racing_creator() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for racing in [false, true] {
            let path = directory
                .path()
                .join(if racing { "racing" } else { "owned" });
            let error = create_directories_with(
                &path,
                &mut |path| {
                    fs::create_dir(path)?;
                    if racing {
                        Err(io::ErrorKind::AlreadyExists.into())
                    } else {
                        Ok(())
                    }
                },
                &mut |path| {
                    if path == directory.path() {
                        Err(io::Error::other("injected parent sync failure"))
                    } else {
                        Ok(())
                    }
                },
            )
            .expect_err("parent sync failure");
            assert!(
                error
                    .to_string()
                    .contains("atomic file write failed during new directory synchronization")
            );
            assert_eq!(path.exists(), racing);
            if !racing {
                create_directories(&path).expect("retry after cleanup");
                assert!(path.is_dir());
            }
        }
    }
}
