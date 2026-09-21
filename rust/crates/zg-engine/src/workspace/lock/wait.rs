//! Cooperative home-lock admission with writer priority across processes.

use tokio_util::sync::CancellationToken;

use super::{
    Duration, EngineError, File, FileLock, Instant, LockMode, Path, TryLockError,
    check_home_parent, open_lock_file,
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

    pub(crate) async fn retry(&self, home: &Path, operation: &str) -> Result<(), EngineError> {
        self.check_cancelled()?;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(EngineError::resource_busy(format!(
                "timed out waiting for workspace lock: home={} operation={operation}",
                home.display()
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
        home: &Path,
        mode: LockMode,
        operation: &str,
    ) -> Result<FileLock, EngineError> {
        check_home_parent(home, operation)?;
        self.check_cancelled()?;
        // Writers share an intent lock while queued and while writing. Readers
        // need it exclusively only during admission, so active readers can drain
        // without a stream of new readers overtaking a waiting writer.
        let intent = open_lock_file(&home.join("locks/write-intent"), operation)?;
        self.wait_file(&intent, home, opposite(mode), operation)
            .await?;
        let file = open_lock_file(&home.join("locks/home"), operation)?;
        self.wait_file(&file, home, mode, operation).await?;
        if mode == LockMode::Read {
            return Ok(FileLock {
                _read_cache: None,
                _file: file,
                _intent: None,
            });
        }
        crate::storage::read_session::release_for_write(home);
        let residency = open_lock_file(&home.join("locks/read-cache"), operation)?;
        // The same deadline covers active readers and idle cached handles.
        self.wait_file(&residency, home, LockMode::Write, operation)
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
        home: &Path,
        mode: LockMode,
        operation: &str,
    ) -> Result<(), EngineError> {
        loop {
            self.check_cancelled()?;
            if try_file_lock(file, mode)? {
                return Ok(());
            }
            self.retry(home, operation).await?;
        }
    }
}

/// A nonblocking admission probe lets queries also observe a newly ready writer.
pub(crate) fn try_home_read(home: &Path, operation: &str) -> Result<Option<FileLock>, EngineError> {
    check_home_parent(home, operation)?;
    let intent = open_lock_file(&home.join("locks/write-intent"), operation)?;
    if !try_file_lock(&intent, LockMode::Write)? {
        return Ok(None);
    }
    let file = open_lock_file(&home.join("locks/home"), operation)?;
    Ok(try_file_lock(&file, LockMode::Read)?.then_some(FileLock {
        _read_cache: None,
        _file: file,
        _intent: None,
    }))
}

fn opposite(mode: LockMode) -> LockMode {
    match mode {
        LockMode::Read => LockMode::Write,
        LockMode::Write => LockMode::Read,
    }
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
mod tests {
    use super::*;
    use crate::workspace::lock::acquire_read_write_lock;

    async fn pending_writer(home: &Path) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while try_home_read(home, "probe").expect("probe").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer reserves admission");
    }

    #[tokio::test]
    async fn writer_intent_blocks_new_readers_until_active_readers_drain() {
        let directory = tempfile::tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let reader = try_home_read(&home, "first query")
            .expect("probe")
            .expect("read lock");
        let writer = tokio::spawn({
            let home = home.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&home, LockMode::Write, "index")
                    .await
            }
        });
        pending_writer(&home).await;
        assert!(!writer.is_finished(), "active reader protects storage");
        let late_reader = tokio::spawn({
            let home = home.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&home, LockMode::Read, "late query")
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
        let home = directory.path().join(".zvec-grep");
        let _reader = try_home_read(&home, "query")
            .expect("probe")
            .expect("reader");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let home = home.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&home, LockMode::Write, "index")
                    .await
            }
        });
        pending_writer(&home).await;
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
            try_home_read(&home, "after cancellation")
                .expect("probe")
                .is_some()
        );
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(&home, LockMode::Write, "timed index")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains("timed out"));
        assert!(
            try_home_read(&home, "after timeout")
                .expect("probe")
                .is_some()
        );
        let writer = tokio::spawn({
            let home = home.clone();
            async move {
                LockWait::new(None, None)
                    .expect("budget")
                    .acquire(&home, LockMode::Write, "aborted index")
                    .await
            }
        });
        pending_writer(&home).await;
        writer.abort();
        assert!(writer.await.expect_err("aborted").is_cancelled());
        assert!(
            try_home_read(&home, "after abort")
                .expect("probe")
                .is_some()
        );
    }

    #[tokio::test]
    async fn reader_and_residency_waits_are_cancellable_and_bounded() {
        let directory = tempfile::tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let writer = LockWait::new(None, None)
            .expect("budget")
            .acquire(&home, LockMode::Write, "index")
            .await
            .expect("writer");
        let error = LockWait::new(None, Some(10))
            .expect("budget")
            .acquire(&home, LockMode::Read, "query")
            .await
            .expect_err("timeout");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        drop(writer);
        let _residency = acquire_read_write_lock(
            &home.join("locks/read-cache"),
            LockMode::Read,
            "remote cache",
        )
        .expect("residency");
        let signal = CancellationToken::new();
        let writer = tokio::spawn({
            let home = home.clone();
            let signal = signal.clone();
            async move {
                LockWait::new(Some(&signal), None)
                    .expect("budget")
                    .acquire(&home, LockMode::Write, "drain cache")
                    .await
            }
        });
        pending_writer(&home).await;
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
            try_home_read(&home, "after drain cancellation")
                .expect("probe")
                .is_some()
        );
    }
}
