//! Publication of downloaded model cache artifacts.

use std::{fs, io, path::Path};

#[cfg(windows)]
use uuid::Uuid;

use crate::{EngineError, EngineResult, utils::sync_directory};

/// Publishes a downloaded model artifact and syncs its file and cache directory.
///
/// Both paths must be in the same directory. The caller must flush buffered writes
/// before calling, and owns cleanup if publication fails. Existing regular files
/// are replaced; the prepared file's permissions are retained. Blocking filesystem
/// operations run off the async executor, without buffering the file in memory.
/// The temporary file must be writable. Once blocking work starts, cancelling the
/// future does not stop publication; cancellation does not imply an unchanged target.
pub(super) async fn publish_downloaded_file(
    temporary: &Path,
    destination: &Path,
) -> EngineResult<()> {
    let temporary = temporary.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || publish_downloaded_file_sync(&temporary, &destination))
        .await
        .map_err(|error| EngineError::internal(format!("file publication task failed: {error}")))?
}

fn publish_downloaded_file_sync(temporary: &Path, destination: &Path) -> EngineResult<()> {
    let failure = |operation: &str, source: &io::Error| {
        EngineError::from_io(
            format!(
                "file publication failed {operation} from '{}' to '{}'",
                temporary.display(),
                destination.display()
            ),
            source,
        )
    };
    let parent = |path: &Path| -> io::Result<_> {
        if !path.file_name().is_some_and(|name| {
            path.as_os_str()
                .as_encoded_bytes()
                .ends_with(name.as_encoded_bytes())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "publication paths must end with a file name",
            ));
        }
        fs::canonicalize(
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
    };
    let directory = parent(destination).map_err(|e| {
        failure(
            "while resolving destination (destination not published)",
            &e,
        )
    })?;
    let temporary_directory = parent(temporary).map_err(|e| {
        failure(
            "while resolving temporary file (destination not published)",
            &e,
        )
    })?;
    if temporary_directory != directory {
        return Err(failure(
            "during path validation (destination not published)",
            &io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary file and destination must share a parent directory",
            ),
        ));
    }
    let metadata = fs::symlink_metadata(temporary).map_err(|e| {
        failure(
            "while reading temporary file (destination not published)",
            &e,
        )
    })?;
    if !metadata.is_file() {
        return Err(failure(
            "during path validation (destination not published)",
            &io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary file must be a regular file",
            ),
        ));
    }
    validate_destination(destination)
        .map_err(|e| failure("while checking destination (destination not published)", &e))?;
    sync_directory(&directory)?;
    // Windows FlushFileBuffers requires write access. Opening with no truncate
    // also leaves an unpublished temporary file available for caller cleanup.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(temporary)
        .map_err(|e| {
            failure(
                "while opening temporary file (destination not published)",
                &e,
            )
        })?;
    file.sync_all()
        .map_err(|e| failure("during file sync (destination not published)", &e))?;
    drop(file);
    rename_replacing(temporary, destination)
        .map_err(|e| failure("during rename (outcome unknown)", &e))?;
    sync_directory(&directory).map_err(|error| {
        EngineError::from_report(crate::ErrorReport {
            message: format!(
                "file publication to '{}' completed; durability unconfirmed: {error}",
                destination.display()
            ),
            ..error.into_report()
        })
    })
}

#[cfg(not(windows))]
fn rename_replacing(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn rename_replacing(temporary: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(temporary, destination) {
        Ok(()) => return Ok(()),
        Err(error)
            if !destination.is_file()
                || !matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                ) =>
        {
            return Err(error);
        }
        Err(_) => {}
    }

    // Windows does not atomically replace every existing regular file with
    // rename. Preserve the previous entry under a unique name until the
    // verified replacement is installed, then roll it back on failure.
    let displaced = destination.with_extension(format!(
        "replaced-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::rename(destination, &displaced)?;
    match fs::rename(temporary, destination) {
        Ok(()) => {
            fs::remove_file(displaced)?;
            Ok(())
        }
        Err(install_error) => {
            if !destination.exists() {
                if let Err(restore_error) = fs::rename(&displaced, destination) {
                    return Err(io::Error::other(format!(
                        "replacement failed ({install_error}); restoring the previous artifact also failed ({restore_error})"
                    )));
                }
            }
            Err(install_error)
        }
    }
}

fn validate_destination(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            #[cfg(windows)]
            if metadata.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination is readonly",
                ));
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination must be a regular file, not a symlink or directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, ffi::OsString, fs::File, io::Read};

    use super::*;

    fn assert_entries(directory: &Path, expected: &[&str]) {
        let entries: BTreeSet<_> = fs::read_dir(directory)
            .expect("read directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        assert_eq!(entries, expected.iter().map(OsString::from).collect());
    }

    #[tokio::test]
    async fn publishes_streamed_files_and_preserves_open_readers() {
        use tokio::io::AsyncWriteExt;

        let root = tempfile::tempdir().expect("temporary directory");
        let destination = root.path().join("模型.bin");
        let temporary = root.path().join("模型.bin.part");
        for contents in ["original", "replacement 中文 😀"] {
            let mut reader = File::open(&destination).ok();
            let mut output = tokio::fs::File::create(&temporary)
                .await
                .expect("stream destination");
            for chunk in contents.as_bytes().chunks(3) {
                output.write_all(chunk).await.expect("stream chunk");
            }
            output.flush().await.expect("flush stream");
            drop(output);
            publish_downloaded_file(&temporary, &destination)
                .await
                .expect("publish stream");
            assert_eq!(
                fs::read(&destination).expect("published contents"),
                contents.as_bytes()
            );
            if let Some(reader) = &mut reader {
                let mut previous = String::new();
                reader
                    .read_to_string(&mut previous)
                    .expect("existing reader");
                assert_eq!(previous, "original");
            }
            assert_entries(root.path(), &["模型.bin"]);
        }
    }

    #[tokio::test]
    async fn publication_rejects_invalid_paths_without_removing_temporary_files() {
        let root = tempfile::tempdir().expect("temporary directory");
        let other = root.path().join("other");
        fs::create_dir(&other).expect("other directory");
        let temporary = root.path().join("record.part");
        let destination = root.path().join("record");
        fs::write(&temporary, b"prepared").expect("temporary contents");
        fs::write(&destination, b"original").expect("existing destination");

        for (source, target) in [
            (temporary.clone(), other.join("record")),
            (temporary.clone(), other.clone()),
            (other, destination.clone()),
            (root.path().join("missing"), destination.clone()),
            (temporary.clone(), destination.join(".")),
        ] {
            let error = publish_downloaded_file(&source, &target)
                .await
                .expect_err("invalid publication");
            assert!(
                error.message().contains("destination not published"),
                "{error}"
            );
            assert_eq!(
                fs::read(&temporary).expect("unpublished contents"),
                b"prepared"
            );
            assert_eq!(
                fs::read(&destination).expect("unchanged destination"),
                b"original"
            );
        }
        assert_entries(root.path(), &["other", "record", "record.part"]);
    }
}
