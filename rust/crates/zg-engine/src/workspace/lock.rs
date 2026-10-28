//! File locks and workspace access coordination.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError,
    domain::WorkspaceLayout,
    utils::{directory_exists, reject_symlink},
    workspace::index,
};

const RETRY_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub(crate) struct LockGuard {
    // Release the cache barrier before allowing new workspace readers.
    _read_cache: Option<File>,
    // Closing the file releases the lock. Keep its path in place so every
    // process continues to lock the same file.
    _file: File,
    // Keep writer intent until both residency and the workspace lock are released.
    _intent: Option<File>,
}

pub(crate) fn try_acquire_file_lock(
    lock_path: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<LockGuard, EngineError> {
    let file = open_lock_file(lock_path, operation)?;
    let result = match mode {
        LockMode::Shared => file.try_lock_shared(),
        LockMode::Exclusive => file.try_lock(),
    };
    result.map_err(|error| match error {
        TryLockError::WouldBlock => EngineError::resource_busy(format!(
            "lock is busy: lock={} operation={operation}",
            lock_path.display(),
        )),
        TryLockError::Error(error) => EngineError::from_io(
            format!(
                "acquire file lock {} operation={operation}",
                lock_path.display()
            ),
            &error,
        ),
    })?;
    Ok(LockGuard {
        _file: file,
        _read_cache: None,
        _intent: None,
    })
}

pub(crate) struct LockWait<'a> {
    signal: Option<&'a CancellationToken>,
    deadline: Instant,
}

impl<'a> LockWait<'a> {
    pub(crate) fn new(
        signal: Option<&'a CancellationToken>,
        timeout_ms: Option<u64>,
    ) -> Result<Self, EngineError> {
        let timeout = timeout_ms.map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        Ok(Self {
            signal,
            deadline: Instant::now().checked_add(timeout).ok_or_else(|| {
                EngineError::invalid_argument("workspace lock timeout is too large")
            })?,
        })
    }

    pub(crate) fn check_cancelled(&self) -> Result<(), EngineError> {
        if self.signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(EngineError::cancelled("workspace lock wait was cancelled"));
        }
        Ok(())
    }

    pub(crate) async fn wait_for_retry(
        &self,
        storage_dir: &Path,
        operation: &str,
    ) -> Result<(), EngineError> {
        self.check_cancelled()?;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(EngineError::resource_busy(format!(
                "timed out waiting for workspace lock: storage_dir={} operation={operation}",
                storage_dir.display()
            )));
        }
        let cancelled = async {
            match self.signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = cancelled => Err(EngineError::cancelled("workspace lock wait was cancelled")),
            () = tokio::time::sleep(RETRY_INTERVAL.min(remaining)) => Ok(()),
        }
    }

    pub(crate) async fn acquire(
        &self,
        source_root: &Path,
        layout: &WorkspaceLayout,
        mode: LockMode,
        operation: &str,
    ) -> Result<LockGuard, EngineError> {
        let storage_dir = layout.storage_dir();
        self.check_cancelled()?;
        if mode == LockMode::Shared {
            loop {
                if let Some(lock) = try_acquire_workspace_read(source_root, layout, operation)? {
                    return Ok(lock);
                }
                self.wait_for_retry(storage_dir, operation).await?;
            }
        }
        check_workspace_parent(storage_dir, operation)?;
        // Writers share an intent lock while queued and while writing. Readers
        // need it exclusively only during admission, so active readers can drain
        // without a stream of new readers overtaking a waiting writer.
        let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
        self.acquire_file(&intent, storage_dir, LockMode::Shared, operation)
            .await?;
        let file = open_lock_file(layout.workspace_lock_path(), operation)?;
        self.acquire_file(&file, storage_dir, LockMode::Exclusive, operation)
            .await?;
        crate::storage::read_session::release_for_write(layout.storage_dir());
        let residency = open_lock_file(layout.read_cache_lock_path(), operation)?;
        // The same deadline covers active readers and idle cached handles.
        self.acquire_file(&residency, storage_dir, LockMode::Exclusive, operation)
            .await?;
        Ok(LockGuard {
            _read_cache: Some(residency),
            _file: file,
            _intent: Some(intent),
        })
    }

    async fn acquire_file(
        &self,
        file: &File,
        storage_dir: &Path,
        mode: LockMode,
        operation: &str,
    ) -> Result<(), EngineError> {
        loop {
            self.check_cancelled()?;
            if try_lock_handle(file, mode)? {
                return Ok(());
            }
            self.wait_for_retry(storage_dir, operation).await?;
        }
    }
}

/// A nonblocking admission probe lets queries also observe a newly ready writer.
pub(crate) fn try_acquire_workspace_read(
    source_root: &Path,
    layout: &WorkspaceLayout,
    operation: &str,
) -> Result<Option<LockGuard>, EngineError> {
    check_workspace_parent(layout.storage_dir(), operation)?;
    let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
    if !try_lock_handle(&intent, LockMode::Exclusive)? {
        return Ok(None);
    }
    let file = open_lock_file(layout.workspace_lock_path(), operation)?;
    if !try_lock_handle(&file, LockMode::Shared)? {
        return Ok(None);
    }
    if index::needs_recovery(layout)? {
        drop(file);
        drop(intent);
        try_recover_publication(source_root, layout, operation)?;
        return Ok(None);
    }
    Ok(Some(LockGuard {
        _read_cache: None,
        _file: file,
        _intent: None,
    }))
}

/// Probe without creating paths that may have been removed with the workspace.
pub(crate) fn can_retain_read_cache(layout: &WorkspaceLayout) -> bool {
    if !directory_exists(layout.storage_dir()).is_ok_and(|exists| exists)
        || reject_symlink(layout.locks_dir()).is_err()
        || reject_symlink(layout.workspace_lock_path()).is_err()
    {
        return false;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(layout.workspace_lock_path())
    else {
        return false;
    };
    file.try_lock_shared().is_ok()
}

fn try_recover_publication(
    source_root: &Path,
    layout: &WorkspaceLayout,
    operation: &str,
) -> Result<(), EngineError> {
    let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
    if !try_lock_handle(&intent, LockMode::Shared)? {
        return Ok(());
    }
    let file = open_lock_file(layout.workspace_lock_path(), operation)?;
    if !try_lock_handle(&file, LockMode::Exclusive)? {
        return Ok(());
    }
    crate::storage::read_session::release_for_write(layout.storage_dir());
    let residency = open_lock_file(layout.read_cache_lock_path(), operation)?;
    if !try_lock_handle(&residency, LockMode::Exclusive)? {
        return Ok(());
    }
    crate::workspace::manifest::require_compatible_manifest(source_root, layout)?;
    index::recover_build(layout)
}

fn try_lock_handle(file: &File, mode: LockMode) -> Result<bool, EngineError> {
    match match mode {
        LockMode::Shared => file.try_lock_shared(),
        LockMode::Exclusive => file.try_lock(),
    } {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(EngineError::from_io("acquire file lock", &error)),
    }
}

fn open_lock_file(lock_path: &Path, operation: &str) -> Result<File, EngineError> {
    for path in [Some(lock_path), lock_path.parent()].into_iter().flatten() {
        reject_symlink(path)?;
    }
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        EngineError::from_io(
            format!(
                "create lock directory {} operation={operation}",
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
                "open lock file {} operation={operation}",
                lock_path.display()
            ),
            &error,
        )
    })
}

fn check_workspace_parent(storage_dir: &Path, operation: &str) -> Result<(), EngineError> {
    let error = |source| {
        EngineError::from_io(
            format!(
                "check workspace storage parent {} operation={operation}",
                storage_dir.display()
            ),
            &source,
        )
    };
    let parent = storage_dir.parent().ok_or_else(|| {
        error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace storage directory has no parent directory",
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
    reject_symlink(storage_dir)
}

#[cfg(test)]
pub(crate) fn acquire_workspace_lock(
    layout: &WorkspaceLayout,
    mode: LockMode,
    operation: &str,
) -> Result<LockGuard, EngineError> {
    check_workspace_parent(layout.storage_dir(), operation)?;
    let lock = try_acquire_file_lock(layout.workspace_lock_path(), mode, operation)?;
    if matches!(mode, LockMode::Exclusive) {
        crate::storage::read_session::release_for_write(layout.storage_dir());
        let cache_path = layout.read_cache_lock_path();
        let cache_lock = open_lock_file(cache_path, operation)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match cache_lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    // Remote cache maintenance observes our exclusive workspace lock
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
        return Ok(LockGuard {
            _read_cache: Some(cache_lock),
            ..lock
        });
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn workspace_lock_lifecycle() {
        let directory = tempdir().expect("workspace");
        let source_root = directory.path();
        let layout = WorkspaceLayout::new(source_root.join(".zvec-grep")).expect("layout");
        let path = layout.workspace_lock_path();
        let first = try_acquire_workspace_read(source_root, &layout, "first query")
            .expect("probe")
            .expect("first reader");
        let second = try_acquire_workspace_read(source_root, &layout, "second query")
            .expect("probe")
            .expect("second reader");
        assert!(path.is_file());
        let error = try_acquire_file_lock(path, LockMode::Exclusive, "index")
            .expect_err("readers block the writer");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains("operation=index"));

        let writer = tokio::spawn({
            let source_root = source_root.to_path_buf();
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&source_root, &layout, LockMode::Exclusive, "index")
                    .await
            }
        });
        pending_writer(source_root, &layout).await;
        assert!(!writer.is_finished(), "active readers protect storage");
        let late_reader = tokio::spawn({
            let source_root = source_root.to_path_buf();
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&source_root, &layout, LockMode::Shared, "late query")
                    .await
            }
        });
        tokio::task::yield_now().await;
        drop(first);
        assert!(try_acquire_file_lock(path, LockMode::Exclusive, "index").is_err());
        assert!(
            !writer.is_finished(),
            "one remaining reader still blocks the writer"
        );
        drop(second);
        let writer = writer
            .await
            .expect("writer task")
            .expect("writer admitted first");
        assert!(
            !late_reader.is_finished(),
            "new reads cannot overtake the writer"
        );
        for mode in [LockMode::Shared, LockMode::Exclusive] {
            let error = try_acquire_file_lock(path, mode, "competing operation")
                .expect_err("writer excludes other readers and writers");
            assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        }
        drop(writer);
        let reader = late_reader
            .await
            .expect("reader task")
            .expect("reader admitted after writer completion");
        drop(reader);
        LockWait::new(None, Some(0))
            .expect("budget")
            .acquire(source_root, &layout, LockMode::Exclusive, "next index")
            .await
            .expect("all locks released for the next writer");
    }

    #[test]
    fn lock_file_preservation() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("home.lock");
        let lock =
            try_acquire_file_lock(&path, LockMode::Exclusive, "index").expect("new lock file");
        drop(lock);
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&path)
                    .expect("new lock metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
                .expect("custom lock permissions");
        }
        fs::write(&path, b"existing lock file").expect("existing lock file");
        for mode in [LockMode::Exclusive, LockMode::Shared] {
            let lock =
                try_acquire_file_lock(&path, mode, "index").expect("acquire existing lock file");
            assert!(path.is_file());
            drop(lock);
            assert_eq!(
                fs::read(&path).expect("retained lock file"),
                b"existing lock file"
            );
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&path)
                    .expect("existing lock metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn invalid_lock_paths() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing");
        let file = directory.path().join("file");
        for root in [&missing, &file] {
            fs::create_dir(root).expect("initial source directory");
            let layout = WorkspaceLayout::new(root.join(".zvec-grep")).expect("layout");
            fs::remove_dir(root).expect("remove source directory after constructing layout");
            if root == &file {
                fs::write(root, b"not a directory").expect("replace source directory with a file");
            }
            let error = acquire_workspace_lock(&layout, LockMode::Exclusive, "index")
                .expect_err("workspace root must already exist as a directory");
            assert!(error.to_string().contains(&root.display().to_string()));
        }
        assert!(!missing.exists());
        assert_eq!(
            fs::read(file).expect("root file remains intact"),
            b"not a directory"
        );

        #[cfg(unix)]
        {
            let external = tempdir().expect("external locks");
            let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
            fs::create_dir(layout.storage_dir()).expect("workspace storage directory");
            fs::write(external.path().join("home"), b"lock").expect("external lock");
            std::os::unix::fs::symlink(external.path(), layout.locks_dir())
                .expect("lock directory alias");
            assert!(!can_retain_read_cache(&layout));
            assert!(acquire_workspace_lock(&layout, LockMode::Shared, "query").is_err());
        }
    }

    async fn pending_writer(source_root: &Path, layout: &WorkspaceLayout) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while try_acquire_workspace_read(source_root, layout, "probe")
                .expect("probe")
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer reserves admission");
    }

    #[tokio::test]
    async fn interrupted_writer_releases_locks() {
        let directory = tempdir().expect("workspace");
        let source_root = directory.path();
        let layout = WorkspaceLayout::new(source_root.join(".zvec-grep")).expect("layout");
        let _reader = try_acquire_workspace_read(source_root, &layout, "query")
            .expect("probe")
            .expect("reader");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let source_root = source_root.to_path_buf();
            let layout = layout.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&source_root, &layout, LockMode::Exclusive, "index")
                    .await
            }
        });
        pending_writer(source_root, &layout).await;
        signal.cancel();
        assert_eq!(
            writer
                .await
                .expect("writer task")
                .expect_err("cancelled writer")
                .code(),
            EngineError::CANCELLED
        );
        assert!(
            try_acquire_workspace_read(source_root, &layout, "after cancellation")
                .expect("probe")
                .is_some()
        );
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(source_root, &layout, LockMode::Exclusive, "timed index")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains("timed out"));
        assert!(
            try_acquire_workspace_read(source_root, &layout, "after timeout")
                .expect("probe")
                .is_some()
        );
        let writer = tokio::spawn({
            let source_root = source_root.to_path_buf();
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&source_root, &layout, LockMode::Exclusive, "aborted index")
                    .await
            }
        });
        pending_writer(source_root, &layout).await;
        writer.abort();
        assert!(writer.await.expect_err("aborted").is_cancelled());
        assert!(
            try_acquire_workspace_read(source_root, &layout, "after abort")
                .expect("probe")
                .is_some()
        );
    }

    #[tokio::test]
    async fn blocked_reads_and_cache_drain() {
        let directory = tempdir().expect("workspace");
        let source_root = directory.path();
        let layout = WorkspaceLayout::new(source_root.join(".zvec-grep")).expect("layout");
        let writer = LockWait::new(None, None)
            .expect("budget")
            .acquire(source_root, &layout, LockMode::Exclusive, "index")
            .await
            .expect("writer");
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(source_root, &layout, LockMode::Shared, "query")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        drop(writer);
        let _residency = try_acquire_file_lock(
            layout.read_cache_lock_path(),
            LockMode::Shared,
            "remote cache",
        )
        .expect("residency");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let source_root = source_root.to_path_buf();
            let layout = layout.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&source_root, &layout, LockMode::Exclusive, "drain cache")
                    .await
            }
        });
        pending_writer(source_root, &layout).await;
        assert!(!writer.is_finished(), "cached handles block the writer");
        assert!(
            !can_retain_read_cache(&layout),
            "writer holds the workspace lock while draining caches"
        );
        signal.cancel();
        assert_eq!(
            writer
                .await
                .expect("writer task")
                .expect_err("cancel residency wait")
                .code(),
            EngineError::CANCELLED
        );
        assert!(
            try_acquire_workspace_read(source_root, &layout, "after drain cancellation")
                .expect("probe")
                .is_some()
        );
    }
}
