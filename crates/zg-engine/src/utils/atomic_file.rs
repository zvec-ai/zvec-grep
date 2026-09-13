//! Atomic replacement with file and parent-directory synchronization.

use std::{
    fs::{self, File, Permissions},
    io::{self, Write},
    path::Path,
};

use tempfile::{NamedTempFile, PersistError};

use crate::{EngineError, EngineResult};

/// Prepare a complete file, atomically publish it, then sync its parent.
/// Publication or subsequent sync errors do not roll back the destination.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> EngineResult<()> {
    let error = |operation, source| failure(path, operation, &source, None);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(error(
            "during path validation (destination not published)",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination needs a parent and file name",
            ),
        ));
    };
    let parent = fs::canonicalize(nonempty_directory(parent)).map_err(|e| {
        error(
            "while resolving parent directory (destination not published)",
            e,
        )
    })?;
    let destination = parent.join(name);
    let permissions = destination_permissions(&destination)
        .map_err(|e| error("while reading permissions (destination not published)", e))?;
    let directory = open_directory(&parent).map_err(|e| {
        error(
            "while opening parent directory (destination not published)",
            e,
        )
    })?;
    directory.sync_all().map_err(|e| {
        error(
            "during directory sync preflight (destination not published)",
            e,
        )
    })?;

    let mut temporary = create_temporary(&parent).map_err(|e| {
        error(
            "while creating temporary file (destination not published)",
            e,
        )
    })?;
    if let Err(source) = prepare(temporary.as_file_mut(), bytes, permissions.as_ref()) {
        return Err(failure(
            path,
            "while preparing temporary file (destination not published)",
            &source,
            temporary.close().err(),
        ));
    }

    let file = match publish(temporary, &destination, permissions.is_some()) {
        Ok(file) => file,
        Err(error) => {
            return Err(failure(
                path,
                "during publication (outcome unknown)",
                &error.error,
                error.file.close().err(),
            ));
        }
    };
    // Windows no-clobber publication resets file attributes.
    #[cfg(windows)]
    file.sync_all().map_err(|e| {
        error(
            "during file sync (destination published; durability unconfirmed)",
            e,
        )
    })?;
    drop(file);
    directory.sync_all().map_err(|e| {
        error(
            "during parent sync (destination published; durability unconfirmed)",
            e,
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

pub(crate) fn sync_directory(path: &Path) -> EngineResult<()> {
    open_directory(nonempty_directory(path))
        .and_then(|directory| directory.sync_all())
        .map_err(|source| failure(path, "during directory synchronization", &source, None))
}

/// Create missing directories, syncing each one and its parent.
/// Existing directories keep their permissions and must already be durable.
/// New Unix directories use 0700 (subject to umask); Windows inherits permissions.
/// A failed sync removes only our new, empty directory. If cleanup also fails,
/// sync the remaining directory and its parent before reusing it.
pub(crate) fn create_directories(path: &Path) -> EngineResult<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let error = |source| failure(path, "during directory initialization", &source, None);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a directory",
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(error(e)),
    }
    let parent = nonempty_directory(path.parent().unwrap_or(Path::new(".")));
    create_directories(parent)?;
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let created = match builder.create(path) {
        Ok(()) => true,
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path).map_err(error)?.is_dir() =>
        {
            false
        }
        Err(e) => return Err(error(e)),
    };
    // A competing creator winning mkdir does not prove that it synced the entry.
    for directory in [path, parent] {
        if let Err(source) = open_directory(directory).and_then(|file| file.sync_all()) {
            let cleanup = if created {
                fs::remove_dir(path).err()
            } else {
                None
            };
            return Err(failure(
                directory,
                "during new directory synchronization",
                &source,
                cleanup,
            ));
        }
    }
    Ok(())
}

#[track_caller]
fn failure(
    path: &Path,
    operation: &'static str,
    source: &io::Error,
    cleanup: Option<io::Error>,
) -> EngineError {
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
}
