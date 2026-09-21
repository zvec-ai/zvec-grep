use std::{
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    io,
    path::Path,
    time::{Duration, Instant},
};

use crate::EngineError;

mod wait;
pub(crate) use wait::{LockWait, try_home_read};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Read,
    Write,
}

#[derive(Debug)]
pub(crate) struct FileLock {
    // Release the cache barrier before allowing new home readers.
    _read_cache: Option<File>,
    // Closing the file releases the lock. Keep its path in place so every
    // process continues to lock the same file.
    _file: File,
    // Keep writer intent until both residency and the home lock are released.
    _intent: Option<File>,
}

#[cfg(test)]
pub(crate) fn acquire_home_lock(
    home: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    check_home_parent(home, operation)?;
    let lock = acquire_read_write_lock(&home.join("locks/home"), mode, operation)?;
    if matches!(mode, LockMode::Write) {
        crate::storage::read_session::release_for_write(home);
        let cache_path = home.join("locks/read-cache");
        let cache_lock = open_lock_file(&cache_path, operation)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match cache_lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    // Remote cache maintenance observes our exclusive home lock
                    // and closes idle native handles before releasing residency.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(EngineError::resource_busy(format!(
                        "timed out draining index read caches: lock={} operation={operation}",
                        cache_path.display()
                    )));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(EngineError::from_io("drain index read caches", &error));
                }
            }
        }
        return Ok(FileLock {
            _read_cache: Some(cache_lock),
            ..lock
        });
    }
    Ok(lock)
}

fn check_home_parent(home: &Path, operation: &str) -> Result<(), EngineError> {
    let error = |source| {
        EngineError::from_io(
            format!(
                "check parent directory of workspace home {} operation={operation}",
                home.display()
            ),
            &source,
        )
    };
    let parent = home.parent().ok_or_else(|| {
        error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace home has no parent directory",
        ))
    })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if !fs::metadata(parent).map_err(error)?.is_dir() {
        return Err(error(io::Error::new(
            io::ErrorKind::NotADirectory,
            "workspace root is not a directory",
        )));
    }
    Ok(())
}

pub(crate) fn acquire_read_write_lock(
    lock_path: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    let file = open_lock_file(lock_path, operation)?;
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
    Ok(FileLock {
        _file: file,
        _read_cache: None,
        _intent: None,
    })
}

/// Probe without creating paths that may have been removed with the workspace.
pub(crate) fn home_allows_cached_reads(home: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join("locks/home"))
    else {
        return false;
    };
    file.try_lock_shared().is_ok()
}

/// Serialize short metadata updates shared by otherwise independent workspaces.
pub(crate) fn acquire_exclusive_lock(
    lock_path: &Path,
    operation: &str,
) -> Result<FileLock, EngineError> {
    let file = open_lock_file(lock_path, operation)?;
    file.lock().map_err(|error| {
        EngineError::from_io(
            format!(
                "acquire workspace lock {} operation={operation}",
                lock_path.display()
            ),
            &error,
        )
    })?;
    Ok(FileLock {
        _file: file,
        _read_cache: None,
        _intent: None,
    })
}

fn open_lock_file(lock_path: &Path, operation: &str) -> Result<File, EngineError> {
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        builder.mode(0o700);
    }
    builder.create(parent).map_err(|error| {
        EngineError::from_io(
            format!(
                "create workspace lock directory {} operation={operation}",
                parent.display()
            ),
            &error,
        )
    })?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    options.open(lock_path).map_err(|error| {
        EngineError::from_io(
            format!(
                "open workspace lock {} operation={operation}",
                lock_path.display()
            ),
            &error,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn queued_writer_waits_for_an_active_reader() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let reader = acquire_home_lock(&home, LockMode::Read, "query").expect("reader");
        let (started, waiting) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            started.send(()).expect("announce writer");
            LockWait::new(None, None)
                .expect("wait budget")
                .acquire(&home, LockMode::Write, "index")
                .await
        });
        waiting.await.expect("writer started");
        tokio::task::yield_now().await;
        drop(reader);
        writer
            .await
            .expect("writer task")
            .expect("writer waits instead of failing busy");
    }

    #[test]
    fn home_lock_requires_an_existing_workspace_root() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing");
        let file = directory.path().join("file");
        fs::write(&file, b"not a directory").expect("non-directory workspace root");

        for root in [&missing, &file] {
            let home = root.join(".zvec-grep");
            let error = acquire_home_lock(&home, LockMode::Write, "index")
                .expect_err("workspace root must already exist as a directory");
            assert!(error.to_string().contains(&home.display().to_string()));
            assert!(error.to_string().contains("operation=index"));
        }
        assert!(!missing.exists());
        assert_eq!(
            fs::read(file).expect("root file remains intact"),
            b"not a directory"
        );

        let home = directory.path().join(".zvec-grep");
        let lock = acquire_home_lock(&home, LockMode::Write, "index")
            .expect("create lock below an existing workspace root");
        assert!(home.join("locks/home").is_file());
        drop(lock);
    }

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
