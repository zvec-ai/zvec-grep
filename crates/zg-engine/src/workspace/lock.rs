use std::{
    fs::{File, OpenOptions, TryLockError},
    path::Path,
};

use crate::{EngineError, utils::create_directories};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Read,
    Write,
}

#[derive(Debug)]
pub(crate) struct FileLock {
    // Closing the file releases the lock. Keep its path in place so every
    // process continues to lock the same file.
    _file: File,
}

pub(crate) fn acquire_home_lock(
    home: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    acquire_read_write_lock(&home.join("locks/home"), mode, operation)
}

pub(crate) fn acquire_read_write_lock(
    lock_path: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    create_directories(parent)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(lock_path).map_err(|error| {
        EngineError::from_io(
            format!(
                "open workspace lock {} operation={operation}",
                lock_path.display()
            ),
            &error,
        )
    })?;
    let result = match mode {
        LockMode::Read => file.try_lock_shared(),
        LockMode::Write => file.try_lock(),
    };
    result.map_err(|error| match error {
        TryLockError::WouldBlock => EngineError::resource_busy(format!(
            "index unavailable: lock={} operation={operation}",
            lock_path.display(),
        )),
        TryLockError::Error(error) => EngineError::from_io(
            format!(
                "acquire workspace lock {} operation={operation}",
                lock_path.display()
            ),
            &error,
        ),
    })?;
    Ok(FileLock { _file: file })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn readers_share_the_lock_and_block_a_writer() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("locks/home");
        let first =
            acquire_read_write_lock(&path, LockMode::Read, "context").expect("first reader");
        let second = acquire_read_write_lock(&path, LockMode::Read, "info").expect("second reader");

        let error = acquire_read_write_lock(&path, LockMode::Write, "index")
            .expect_err("writer excludes both readers");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains("operation=index"));
        drop(first);
        assert!(acquire_read_write_lock(&path, LockMode::Write, "index").is_err());
        drop(second);
        assert!(acquire_read_write_lock(&path, LockMode::Write, "index").is_ok());
    }

    #[test]
    fn writer_blocks_readers_and_writers_until_released() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("locks/home");
        let writer = acquire_read_write_lock(&path, LockMode::Write, "index").expect("writer");

        for mode in [LockMode::Read, LockMode::Write] {
            let error = acquire_read_write_lock(&path, mode, "competing operation")
                .expect_err("writer excludes other readers and writers");
            assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        }
        drop(writer);
        assert!(acquire_read_write_lock(&path, LockMode::Read, "context").is_ok());
        assert!(acquire_read_write_lock(&path, LockMode::Write, "index").is_ok());
    }

    #[test]
    fn release_preserves_the_lock_file_and_contents() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("home.lock");
        fs::write(&path, b"existing lock file").expect("existing lock file");

        for mode in [LockMode::Write, LockMode::Read] {
            let lock =
                acquire_read_write_lock(&path, mode, "index").expect("acquire existing lock file");
            assert!(path.is_file());
            drop(lock);
            assert_eq!(
                fs::read(&path).expect("retained lock file"),
                b"existing lock file"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_lock_files_are_private_and_existing_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("home.lock");
        let lock = acquire_read_write_lock(&path, LockMode::Write, "index").expect("new lock file");
        drop(lock);
        assert_eq!(
            fs::metadata(&path)
                .expect("new lock metadata")
                .permissions()
                .mode()
                & 0o077,
            0,
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("custom lock permissions");
        let lock =
            acquire_read_write_lock(&path, LockMode::Write, "index").expect("existing lock file");
        drop(lock);
        assert_eq!(
            fs::metadata(&path)
                .expect("existing lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640,
        );
    }
}
