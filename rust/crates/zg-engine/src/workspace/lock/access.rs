//! Workspace lock admission, cache coordination, and interrupted index recovery.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::acquire_read_write_lock;
use super::{FileLock, LockMode, open_lock_file};
use crate::{
    EngineError,
    domain::WorkspaceLayout,
    utils::{directory_exists, reject_symlink},
    workspace::index,
};

const RETRY_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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

    pub(crate) async fn retry(
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
        layout: &WorkspaceLayout,
        mode: LockMode,
        operation: &str,
    ) -> Result<FileLock, EngineError> {
        let storage_dir = layout.storage_dir();
        self.check_cancelled()?;
        if mode == LockMode::Read {
            loop {
                if let Some(lock) = try_workspace_read(layout, operation)? {
                    return Ok(lock);
                }
                self.retry(storage_dir, operation).await?;
            }
        }
        check_workspace_parent(storage_dir, operation)?;
        // Writers share an intent lock while queued and while writing. Readers
        // need it exclusively only during admission, so active readers can drain
        // without a stream of new readers overtaking a waiting writer.
        let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
        self.wait_file(&intent, storage_dir, LockMode::Read, operation)
            .await?;
        let file = open_lock_file(layout.workspace_lock_path(), operation)?;
        self.wait_file(&file, storage_dir, LockMode::Write, operation)
            .await?;
        crate::storage::read_session::release_for_write(layout.storage_dir());
        let residency = open_lock_file(layout.read_cache_lock_path(), operation)?;
        // The same deadline covers active readers and idle cached handles.
        self.wait_file(&residency, storage_dir, LockMode::Write, operation)
            .await?;
        Ok(FileLock {
            _read_cache: Some(residency),
            _file: file,
            _intent: Some(intent),
        })
    }

    async fn wait_file(
        &self,
        file: &File,
        storage_dir: &Path,
        mode: LockMode,
        operation: &str,
    ) -> Result<(), EngineError> {
        loop {
            self.check_cancelled()?;
            if try_file_lock(file, mode)? {
                return Ok(());
            }
            self.retry(storage_dir, operation).await?;
        }
    }
}

/// Probe without creating paths that may have been removed with the workspace.
pub(crate) fn workspace_allows_cached_reads(layout: &WorkspaceLayout) -> bool {
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

/// A nonblocking admission probe lets queries also observe a newly ready writer.
pub(crate) fn try_workspace_read(
    layout: &WorkspaceLayout,
    operation: &str,
) -> Result<Option<FileLock>, EngineError> {
    check_workspace_parent(layout.storage_dir(), operation)?;
    let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
    if !try_file_lock(&intent, LockMode::Write)? {
        return Ok(None);
    }
    let file = open_lock_file(layout.workspace_lock_path(), operation)?;
    if !try_file_lock(&file, LockMode::Read)? {
        return Ok(None);
    }
    if index::needs_recovery(layout)? {
        drop(file);
        drop(intent);
        try_recover_publication(layout, operation)?;
        return Ok(None);
    }
    Ok(Some(FileLock {
        _read_cache: None,
        _file: file,
        _intent: None,
    }))
}

fn try_recover_publication(layout: &WorkspaceLayout, operation: &str) -> Result<(), EngineError> {
    let intent = open_lock_file(layout.write_intent_lock_path(), operation)?;
    if !try_file_lock(&intent, LockMode::Read)? {
        return Ok(());
    }
    let file = open_lock_file(layout.workspace_lock_path(), operation)?;
    if !try_file_lock(&file, LockMode::Write)? {
        return Ok(());
    }
    crate::storage::read_session::release_for_write(layout.storage_dir());
    let residency = open_lock_file(layout.read_cache_lock_path(), operation)?;
    if !try_file_lock(&residency, LockMode::Write)? {
        return Ok(());
    }
    index::recover_build(layout)
}

fn try_file_lock(file: &File, mode: LockMode) -> Result<bool, EngineError> {
    match match mode {
        LockMode::Read => file.try_lock_shared(),
        LockMode::Write => file.try_lock(),
    } {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => {
            Err(EngineError::from_io("acquire workspace lock", &error))
        }
    }
}

#[cfg(test)]
pub(crate) fn acquire_workspace_lock(
    layout: &WorkspaceLayout,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    check_workspace_parent(layout.storage_dir(), operation)?;
    let lock = acquire_read_write_lock(layout.workspace_lock_path(), mode, operation)?;
    if matches!(mode, LockMode::Write) {
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
        return Ok(FileLock {
            _read_cache: Some(cache_lock),
            ..lock
        });
    }
    Ok(lock)
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
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn cache_probe_rejects_a_symlinked_lock_directory() {
        let directory = tempdir().expect("workspace");
        let external = tempdir().expect("external locks");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let storage_dir = layout.storage_dir();
        fs::create_dir(storage_dir).expect("workspace storage directory");
        fs::write(external.path().join("home"), b"lock").expect("external lock");
        std::os::unix::fs::symlink(external.path(), layout.locks_dir())
            .expect("lock directory alias");

        assert!(!workspace_allows_cached_reads(&layout));
        assert!(acquire_workspace_lock(&layout, LockMode::Read, "query").is_err());
    }

    #[tokio::test]
    async fn queued_writer_waits_for_an_active_reader() {
        let directory = tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let reader = acquire_workspace_lock(&layout, LockMode::Read, "query").expect("reader");
        let (started, waiting) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            started.send(()).expect("announce writer");
            LockWait::new(None, None)
                .expect("wait budget")
                .acquire(&layout, LockMode::Write, "index")
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
    fn workspace_lock_requires_an_existing_source_root() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing");
        let file = directory.path().join("file");
        fs::write(&file, b"not a directory").expect("non-directory workspace root");

        for root in [&missing, &file] {
            let error = WorkspaceLayout::new(root.clone())
                .and_then(|layout| acquire_workspace_lock(&layout, LockMode::Write, "index"))
                .expect_err("workspace root must already exist as a directory");
            assert!(error.to_string().contains(&root.display().to_string()));
        }
        assert!(!missing.exists());
        assert_eq!(
            fs::read(file).expect("root file remains intact"),
            b"not a directory"
        );

        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let lock = acquire_workspace_lock(&layout, LockMode::Write, "index")
            .expect("create lock below an existing workspace root");
        assert!(layout.workspace_lock_path().is_file());
        drop(lock);
    }

    async fn pending_writer(layout: &WorkspaceLayout) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while try_workspace_read(layout, "probe")
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
    async fn writer_intent_blocks_new_readers_until_active_readers_drain() {
        let directory = tempfile::tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let reader = try_workspace_read(&layout, "first query")
            .expect("probe")
            .expect("read lock");
        let writer = tokio::spawn({
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&layout, LockMode::Write, "index")
                    .await
            }
        });
        pending_writer(&layout).await;
        assert!(!writer.is_finished(), "active reader protects storage");
        let late_reader = tokio::spawn({
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&layout, LockMode::Read, "late query")
                    .await
            }
        });
        tokio::task::yield_now().await;
        drop(reader);
        let writer = writer
            .await
            .expect("writer task")
            .expect("writer admitted first");
        assert!(
            !late_reader.is_finished(),
            "new reads cannot overtake the writer"
        );
        drop(writer);
        late_reader
            .await
            .expect("reader task")
            .expect("query waits for writer completion");
    }

    #[tokio::test]
    async fn cancelled_and_timed_out_writers_release_their_intent() {
        let directory = tempfile::tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let _reader = try_workspace_read(&layout, "query")
            .expect("probe")
            .expect("reader");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let layout = layout.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&layout, LockMode::Write, "index")
                    .await
            }
        });
        pending_writer(&layout).await;
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
            try_workspace_read(&layout, "after cancellation")
                .expect("probe")
                .is_some()
        );
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(&layout, LockMode::Write, "timed index")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains("timed out"));
        assert!(
            try_workspace_read(&layout, "after timeout")
                .expect("probe")
                .is_some()
        );
        let writer = tokio::spawn({
            let layout = layout.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&layout, LockMode::Write, "aborted index")
                    .await
            }
        });
        pending_writer(&layout).await;
        writer.abort();
        assert!(writer.await.expect_err("aborted").is_cancelled());
        assert!(
            try_workspace_read(&layout, "after abort")
                .expect("probe")
                .is_some()
        );
    }

    #[tokio::test]
    async fn reader_and_residency_waits_are_cancellable_and_bounded() {
        let directory = tempfile::tempdir().expect("workspace");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        let writer = LockWait::new(None, None)
            .expect("budget")
            .acquire(&layout, LockMode::Write, "index")
            .await
            .expect("writer");
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(&layout, LockMode::Read, "query")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        drop(writer);
        let _residency = acquire_read_write_lock(
            layout.read_cache_lock_path(),
            LockMode::Read,
            "remote cache",
        )
        .expect("residency");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let layout = layout.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&layout, LockMode::Write, "drain cache")
                    .await
            }
        });
        pending_writer(&layout).await;
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
            try_workspace_read(&layout, "after drain cancellation")
                .expect("probe")
                .is_some()
        );
    }
}
