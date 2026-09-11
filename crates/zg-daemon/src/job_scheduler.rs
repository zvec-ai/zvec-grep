//! Bounded resident index-job scheduling with per-root writer serialization.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zg_engine::{
    EngineError, ErrorReport,
    api::index::{IndexOptions, IndexResult},
    api::info::InfoOptions,
};

const MAX_PERSISTED_ERROR_CHARS: usize = 512;
const REDACTED: &str = "[redacted]";

#[async_trait]
pub(crate) trait IndexExecutor: Send + Sync {
    async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError>;

    async fn drop_index(&self, _options: InfoOptions) -> Result<bool, EngineError> {
        Ok(false)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobReason {
    Manual,
    Watch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JobError {
    pub report: ErrorReport,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexJobSnapshot {
    pub id: Uuid,
    pub canonical_root: PathBuf,
    pub reason: JobReason,
    pub state: JobState,
    pub progress: Option<zg_engine::api::index::progress::IndexProgress>,
    pub error: Option<JobError>,
}

#[derive(Clone, Debug)]
pub(crate) struct SubmitIndexJobResult {
    pub job: IndexJobSnapshot,
    pub reused: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexJobCompletion {
    pub job: IndexJobSnapshot,
    pub result: Option<IndexResult>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SchedulerConfig {
    pub concurrency: usize,
    pub queue_capacity: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SchedulerSnapshot {
    pub queued: usize,
    pub running: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            concurrency: 2,
            queue_capacity: 64,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum SchedulerError {
    #[error("the daemon job scheduler is shutting down")]
    Closed,
    #[error("the daemon index queue is full")]
    QueueFull,
    #[error("unknown daemon index job {0}")]
    UnknownJob(Uuid),
}

#[derive(Clone)]
pub(crate) struct IndexJobScheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    executor: Arc<dyn IndexExecutor>,
    permits: Arc<Semaphore>,
    state: Mutex<SchedulerState>,
    outstanding: AtomicUsize,
    closed: AtomicBool,
    drained: Notify,
    config: SchedulerConfig,
}

#[derive(Default)]
struct SchedulerState {
    jobs: HashMap<Uuid, Arc<ScheduledJob>>,
    active_by_root: HashMap<PathBuf, Arc<ScheduledJob>>,
    followup_by_root: HashMap<PathBuf, Arc<ScheduledJob>>,
    latest_by_root: HashMap<PathBuf, Arc<ScheduledJob>>,
}

struct ScheduledJob {
    id: Uuid,
    canonical_root: PathBuf,
    snapshot: Mutex<IndexJobSnapshot>,
    options: Mutex<Option<IndexOptions>>,
    cancellation: CancellationToken,
    result: Mutex<Option<IndexResult>>,
    finished: AtomicBool,
    completed: Notify,
}

impl IndexJobScheduler {
    pub(crate) fn new(executor: Arc<dyn IndexExecutor>, config: SchedulerConfig) -> Self {
        let concurrency = config.concurrency.max(1);
        Self {
            inner: Arc::new(SchedulerInner {
                executor,
                permits: Arc::new(Semaphore::new(concurrency)),
                state: Mutex::new(SchedulerState::default()),
                outstanding: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                drained: Notify::new(),
                config: SchedulerConfig {
                    concurrency,
                    queue_capacity: config.queue_capacity,
                },
            }),
        }
    }

    pub(crate) fn submit(
        &self,
        canonical_root: PathBuf,
        options: IndexOptions,
        reason: JobReason,
    ) -> Result<SubmitIndexJobResult, SchedulerError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(SchedulerError::Closed);
        }
        let mut state = lock(&self.inner.state);
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(SchedulerError::Closed);
        }
        if let Some(active) = state.active_by_root.get(&canonical_root).cloned() {
            let active_snapshot = lock(&active.snapshot).clone();
            let needs_followup = reason == JobReason::Watch
                || active_snapshot.reason == JobReason::Watch
                || options.rebuild;
            if needs_followup && active_snapshot.state == JobState::Queued {
                let queued_full_manual = active_snapshot.reason == JobReason::Manual
                    && reason == JobReason::Watch
                    && lock(&active.options)
                        .as_ref()
                        .is_some_and(|options| options.changes.is_empty());
                if !queued_full_manual {
                    merge_options(&mut lock(&active.options), options);
                }
                if reason == JobReason::Manual {
                    lock(&active.snapshot).reason = JobReason::Manual;
                }
                return Ok(SubmitIndexJobResult {
                    job: lock(&active.snapshot).clone(),
                    reused: true,
                });
            }
            if needs_followup {
                if let Some(followup) = state.followup_by_root.get(&canonical_root) {
                    merge_options(&mut lock(&followup.options), options);
                    if reason == JobReason::Manual {
                        lock(&followup.snapshot).reason = JobReason::Manual;
                    }
                    return Ok(SubmitIndexJobResult {
                        job: lock(&followup.snapshot).clone(),
                        reused: true,
                    });
                }
                self.reserve_slot()?;
                let followup = create_job(canonical_root.clone(), options, reason);
                let snapshot = lock(&followup.snapshot).clone();
                state.jobs.insert(followup.id, Arc::clone(&followup));
                state.followup_by_root.insert(canonical_root, followup);
                return Ok(SubmitIndexJobResult {
                    job: snapshot,
                    reused: true,
                });
            }
            return Ok(SubmitIndexJobResult {
                job: active_snapshot,
                reused: true,
            });
        }

        self.reserve_slot()?;
        let job = create_job(canonical_root.clone(), options, reason);
        let snapshot = lock(&job.snapshot).clone();
        state.jobs.insert(job.id, Arc::clone(&job));
        state
            .active_by_root
            .insert(canonical_root, Arc::clone(&job));
        state
            .latest_by_root
            .insert(job.canonical_root.clone(), Arc::clone(&job));
        drop(state);
        spawn_job(Arc::clone(&self.inner), job);

        Ok(SubmitIndexJobResult {
            job: snapshot,
            reused: false,
        })
    }

    pub(crate) async fn wait(&self, id: Uuid) -> Result<IndexJobCompletion, SchedulerError> {
        self.wait_with_progress(id, None).await
    }

    pub(crate) async fn wait_with_progress(
        &self,
        id: Uuid,
        reporter: Option<zg_engine::api::index::progress::IndexProgressReporter>,
    ) -> Result<IndexJobCompletion, SchedulerError> {
        let job = lock(&self.inner.state)
            .jobs
            .get(&id)
            .cloned()
            .ok_or(SchedulerError::UnknownJob(id))?;
        let mut previous = None;
        loop {
            let notified = job.completed.notified();
            let snapshot = lock(&job.snapshot).clone();
            if let Some(reporter) = &reporter
                && snapshot.progress != previous
            {
                if let Some(progress) = &snapshot.progress {
                    reporter.report(progress.clone());
                }
                previous.clone_from(&snapshot.progress);
            }
            if job.finished.load(Ordering::Acquire) {
                return Ok(IndexJobCompletion {
                    job: snapshot,
                    result: lock(&job.result).clone(),
                });
            }
            tokio::select! {
                () = notified => {},
                () = tokio::time::sleep(std::time::Duration::from_millis(100)), if reporter.is_some() => {},
            }
        }
    }

    pub(crate) fn snapshot(&self) -> SchedulerSnapshot {
        let state = lock(&self.inner.state);
        let mut snapshot = SchedulerSnapshot::default();
        for job in state.jobs.values() {
            match lock(&job.snapshot).state {
                JobState::Queued => snapshot.queued += 1,
                JobState::Running => snapshot.running += 1,
                JobState::Succeeded | JobState::Failed | JobState::Cancelled => {}
            }
        }
        snapshot
    }

    pub(crate) fn get_by_root(&self, canonical_root: &PathBuf) -> Option<IndexJobSnapshot> {
        lock(&self.inner.state)
            .latest_by_root
            .get(canonical_root)
            .map(|job| lock(&job.snapshot).clone())
    }

    pub(crate) fn cancel_root(&self, canonical_root: &PathBuf) -> bool {
        let (active, followup) = {
            let mut state = lock(&self.inner.state);
            (
                state.active_by_root.get(canonical_root).cloned(),
                state.followup_by_root.remove(canonical_root),
            )
        };
        if let Some(ref followup) = followup {
            followup.cancellation.cancel();
            finish_cancelled(followup, "indexing was cancelled");
            if self.inner.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.inner.drained.notify_waiters();
            }
            mark_finished(followup);
        }
        if let Some(active) = active {
            active.cancellation.cancel();
            true
        } else {
            followup.is_some()
        }
    }

    pub(crate) async fn wait_for_root_idle(&self, canonical_root: &PathBuf) {
        loop {
            let active = lock(&self.inner.state)
                .active_by_root
                .get(canonical_root)
                .cloned();
            let Some(active) = active else {
                return;
            };
            let _ = self.wait(active.id).await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) {
            let (active, followups) = {
                let mut state = lock(&self.inner.state);
                let active = state.active_by_root.values().cloned().collect::<Vec<_>>();
                let followups = state
                    .followup_by_root
                    .drain()
                    .map(|(_, job)| job)
                    .collect::<Vec<_>>();
                (active, followups)
            };
            for job in active {
                job.cancellation.cancel();
            }
            for job in followups {
                job.cancellation.cancel();
                finish_cancelled(
                    &job,
                    "indexing was cancelled because the daemon is shutting down",
                );
                if self.inner.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.inner.drained.notify_waiters();
                }
                mark_finished(&job);
            }
            self.inner.permits.close();
        }
        loop {
            let notified = self.inner.drained.notified();
            if self.inner.outstanding.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn reserve_slot(&self) -> Result<(), SchedulerError> {
        let maximum_outstanding = self
            .inner
            .config
            .concurrency
            .saturating_add(self.inner.config.queue_capacity);
        let previous = self.inner.outstanding.fetch_add(1, Ordering::AcqRel);
        if previous >= maximum_outstanding {
            self.inner.outstanding.fetch_sub(1, Ordering::AcqRel);
            return Err(SchedulerError::QueueFull);
        }
        Ok(())
    }
}

fn create_job(
    canonical_root: PathBuf,
    options: IndexOptions,
    reason: JobReason,
) -> Arc<ScheduledJob> {
    let id = Uuid::new_v4();
    Arc::new(ScheduledJob {
        id,
        canonical_root: canonical_root.clone(),
        snapshot: Mutex::new(IndexJobSnapshot {
            id,
            canonical_root,
            reason,
            state: JobState::Queued,
            progress: None,
            error: None,
        }),
        options: Mutex::new(Some(options)),
        cancellation: CancellationToken::new(),
        result: Mutex::new(None),
        finished: AtomicBool::new(false),
        completed: Notify::new(),
    })
}

fn spawn_job(inner: Arc<SchedulerInner>, job: Arc<ScheduledJob>) {
    tokio::spawn(async move {
        let permit = tokio::select! {
            () = job.cancellation.cancelled() => {
                finish_cancelled(&job, "indexing was cancelled");
                finish_job(&inner, &job);
                return;
            }
            permit = Arc::clone(&inner.permits).acquire_owned() => permit,
        };
        let Ok(_permit) = permit else {
            finish_cancelled(&job, "the daemon job scheduler closed");
            finish_job(&inner, &job);
            return;
        };
        if inner.closed.load(Ordering::Acquire) {
            finish_cancelled(&job, "the daemon job scheduler closed");
            finish_job(&inner, &job);
            return;
        }
        lock(&job.snapshot).state = JobState::Running;
        let mut options = lock(&job.options)
            .take()
            .expect("a queued daemon job must retain its index options");
        options.signal = Some(job.cancellation.clone());
        let weak_job = Arc::downgrade(&job);
        options.on_progress = Some(
            zg_engine::api::index::progress::IndexProgressReporter::new(move |progress| {
                if let Some(job) = weak_job.upgrade() {
                    lock(&job.snapshot).progress = Some(progress);
                }
            })
            .prioritize_model_progress(),
        );
        let outcome = inner.executor.index(options).await;
        match outcome {
            Ok(result) => {
                *lock(&job.result) = Some(result);
                lock(&job.snapshot).state = JobState::Succeeded;
            }
            Err(error) => {
                let state = if error.code() == EngineError::CANCELLED {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                };
                let mut snapshot = lock(&job.snapshot);
                snapshot.state = state;
                snapshot.error = Some(job_error(error));
            }
        }
        finish_job(&inner, &job);
    });
}

fn finish_job(inner: &Arc<SchedulerInner>, job: &Arc<ScheduledJob>) {
    if inner.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
        inner.drained.notify_waiters();
    }
    let followup = {
        let mut state = lock(&inner.state);
        if state
            .active_by_root
            .get(&job.canonical_root)
            .is_some_and(|active| active.id == job.id)
        {
            state.active_by_root.remove(&job.canonical_root);
        }
        let followup = state.followup_by_root.remove(&job.canonical_root);
        if let Some(followup) = &followup {
            state
                .active_by_root
                .insert(job.canonical_root.clone(), Arc::clone(followup));
            state
                .latest_by_root
                .insert(job.canonical_root.clone(), Arc::clone(followup));
        }
        followup
    };
    if let Some(followup) = followup {
        if inner.closed.load(Ordering::Acquire) {
            finish_cancelled(&followup, "the daemon job scheduler closed");
            finish_job(inner, &followup);
        } else {
            spawn_job(Arc::clone(inner), followup);
        }
    }
    mark_finished(job);
}

fn merge_options(current: &mut Option<IndexOptions>, mut incoming: IndexOptions) {
    let Some(current) = current.as_mut() else {
        return;
    };
    if incoming.changes.is_empty() {
        *current = incoming;
        return;
    }
    if current.changes.is_empty() {
        current.changes = incoming.changes;
        return;
    }
    if incoming.changes.iter().any(|change| {
        matches!(
            change,
            zg_engine::api::index::options::WorkspaceChange::Rescan
        )
    }) {
        current.changes = vec![zg_engine::api::index::options::WorkspaceChange::Rescan];
        return;
    }
    if current.changes.iter().any(|change| {
        matches!(
            change,
            zg_engine::api::index::options::WorkspaceChange::Rescan
        )
    }) {
        return;
    }
    for change in incoming.changes.drain(..) {
        if let Some(path) = change_path(&change) {
            current
                .changes
                .retain(|existing| change_path(existing) != Some(path));
        }
        current.changes.push(change);
    }
}

fn change_path(
    change: &zg_engine::api::index::options::WorkspaceChange,
) -> Option<&std::path::Path> {
    use zg_engine::api::index::options::WorkspaceChange;
    match change {
        WorkspaceChange::Upsert(path)
        | WorkspaceChange::Delete(path)
        | WorkspaceChange::RescanDirectory(path)
        | WorkspaceChange::DeletePrefix(path) => Some(path),
        WorkspaceChange::Rescan => None,
    }
}

#[track_caller]
fn finish_cancelled(job: &ScheduledJob, message: &str) {
    let mut snapshot = lock(&job.snapshot);
    snapshot.state = JobState::Cancelled;
    snapshot.error = Some(job_error(EngineError::cancelled(message)));
}

fn job_error(error: EngineError) -> JobError {
    let retryable = error.is_retryable();
    let mut report = error.into_report();
    report.message = redact_job_error_text(&report.message);
    report.help = report.help.map(|help| redact_job_error_text(&help));
    JobError { report, retryable }
}

fn redact_job_error_text(message: &str) -> String {
    let mut redacted = redact_bearer_credentials(message);
    for name in [
        "authorization",
        "api_key",
        "api-key",
        "api key",
        "apikey",
        "token",
    ] {
        redacted = redact_assigned_value(&redacted, name);
    }
    let mut truncated = redacted
        .chars()
        .take(MAX_PERSISTED_ERROR_CHARS)
        .collect::<String>();
    if redacted.chars().count() > MAX_PERSISTED_ERROR_CHARS {
        truncated.push('…');
    }
    truncated
}

fn redact_bearer_credentials(message: &str) -> String {
    let mut output = message.to_owned();
    let mut cursor = 0;
    loop {
        let lowercase = output.to_ascii_lowercase();
        let Some(relative_start) = lowercase[cursor..].find("bearer") else {
            return output;
        };
        let marker_start = cursor + relative_start;
        let marker_end = marker_start + "bearer".len();
        let before_is_word =
            marker_start > 0 && lowercase.as_bytes()[marker_start - 1].is_ascii_alphanumeric();
        let after_is_space = lowercase
            .as_bytes()
            .get(marker_end)
            .is_some_and(u8::is_ascii_whitespace);
        if before_is_word || !after_is_space {
            cursor = marker_end;
            continue;
        }
        let value_start = skip_ascii_whitespace(output.as_bytes(), marker_end);
        let value_end = credential_end(output.as_bytes(), value_start);
        if value_start == value_end {
            cursor = marker_end;
            continue;
        }
        output.replace_range(value_start..value_end, REDACTED);
        cursor = value_start + REDACTED.len();
    }
}

fn redact_assigned_value(message: &str, name: &str) -> String {
    let mut output = message.to_owned();
    let mut cursor = 0;
    loop {
        let lowercase = output.to_ascii_lowercase();
        let Some(relative_start) = lowercase[cursor..].find(name) else {
            return output;
        };
        let name_start = cursor + relative_start;
        let name_end = name_start + name.len();
        let before_is_word =
            name_start > 0 && is_identifier_byte(lowercase.as_bytes()[name_start - 1]);
        let after_is_word = lowercase
            .as_bytes()
            .get(name_end)
            .copied()
            .is_some_and(is_identifier_byte);
        if before_is_word || after_is_word {
            cursor = name_end;
            continue;
        }
        let separator = skip_ascii_whitespace(output.as_bytes(), name_end);
        if !output
            .as_bytes()
            .get(separator)
            .is_some_and(|byte| matches!(byte, b'=' | b':'))
        {
            cursor = name_end;
            continue;
        }
        let value_start = skip_ascii_whitespace(output.as_bytes(), separator + 1);
        let value_end = credential_end(output.as_bytes(), value_start);
        if value_start == value_end {
            cursor = name_end;
            continue;
        }
        output.replace_range(value_start..value_end, REDACTED);
        cursor = value_start + REDACTED.len();
    }
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn credential_end(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b',' | b';'))
    {
        cursor += 1;
    }
    cursor
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn mark_finished(job: &ScheduledJob) {
    job.finished.store(true, Ordering::Release);
    job.completed.notify_waiters();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use tokio::sync::Notify;
    use zg_engine::{
        EngineError,
        api::index::{IndexOptions, IndexResult},
    };

    use super::{
        IndexExecutor, IndexJobScheduler, JobReason, JobState, SchedulerConfig, SchedulerError,
    };

    struct GatedExecutor {
        started: Notify,
        release: Notify,
    }

    struct RecordingExecutor {
        calls: Mutex<Vec<IndexOptions>>,
        started: Notify,
        releases: tokio::sync::Semaphore,
    }

    #[async_trait]
    impl IndexExecutor for RecordingExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            self.calls
                .lock()
                .expect("calls lock should be available")
                .push(options);
            self.started.notify_one();
            self.releases
                .acquire()
                .await
                .expect("release semaphore should remain open")
                .forget();
            Ok(IndexResult::default())
        }
    }

    #[async_trait]
    impl IndexExecutor for GatedExecutor {
        async fn index(&self, _options: IndexOptions) -> Result<IndexResult, EngineError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(IndexResult {
                generation: 7,
                ..IndexResult::default()
            })
        }
    }

    #[tokio::test]
    async fn submit_returns_before_work_finishes_and_wait_returns_the_terminal_job() {
        let executor = Arc::new(GatedExecutor {
            started: Notify::new(),
            release: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 1,
                queue_capacity: 8,
            },
        );
        let submitted = scheduler
            .submit(
                PathBuf::from("/workspace"),
                IndexOptions {
                    root: Some(PathBuf::from("/workspace")),
                    ..IndexOptions::default()
                },
                JobReason::Manual,
            )
            .expect("job should be accepted");

        assert!(matches!(
            submitted.job.state,
            JobState::Queued | JobState::Running
        ));
        executor.started.notified().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                scheduler.wait(submitted.job.id)
            )
            .await
            .is_err()
        );

        executor.release.notify_waiters();
        let completed = scheduler
            .wait(submitted.job.id)
            .await
            .expect("job should exist");
        assert_eq!(completed.job.state, JobState::Succeeded);
        assert_eq!(completed.result.map(|result| result.generation), Some(7));
    }

    #[tokio::test]
    async fn concurrent_submissions_for_one_root_reuse_the_active_writer() {
        let executor = Arc::new(GatedExecutor {
            started: Notify::new(),
            release: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 2,
                queue_capacity: 8,
            },
        );
        let options = IndexOptions {
            root: Some(PathBuf::from("/workspace")),
            ..IndexOptions::default()
        };
        let first = scheduler
            .submit(
                PathBuf::from("/workspace"),
                options.clone(),
                JobReason::Manual,
            )
            .expect("first job should be accepted");
        executor.started.notified().await;
        let second = scheduler
            .submit(PathBuf::from("/workspace"), options, JobReason::Manual)
            .expect("duplicate job should be accepted");

        assert!(second.reused);
        assert_eq!(second.job.id, first.job.id);
        executor.release.notify_one();
        assert_eq!(
            scheduler
                .wait(first.job.id)
                .await
                .expect("job should complete")
                .job
                .state,
            JobState::Succeeded
        );
    }

    #[tokio::test]
    async fn watcher_changes_arriving_during_a_writer_run_as_one_followup_job() {
        let executor = Arc::new(RecordingExecutor {
            calls: Mutex::new(Vec::new()),
            started: Notify::new(),
            releases: tokio::sync::Semaphore::new(0),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 2,
                queue_capacity: 8,
            },
        );
        let root = PathBuf::from("/workspace");
        let first = scheduler
            .submit(
                root.clone(),
                IndexOptions {
                    root: Some(root.clone()),
                    ..IndexOptions::default()
                },
                JobReason::Manual,
            )
            .expect("manual job should be accepted");
        executor.started.notified().await;
        let followup = scheduler
            .submit(
                root.clone(),
                IndexOptions {
                    root: Some(root.clone()),
                    changes: vec![zg_engine::api::index::options::WorkspaceChange::Upsert(
                        PathBuf::from("src/lib.rs"),
                    )],
                    ..IndexOptions::default()
                },
                JobReason::Watch,
            )
            .expect("watcher followup should be accepted");
        let merged = scheduler
            .submit(
                root.clone(),
                IndexOptions {
                    root: Some(root.clone()),
                    changes: vec![zg_engine::api::index::options::WorkspaceChange::Delete(
                        PathBuf::from("src/lib.rs"),
                    )],
                    ..IndexOptions::default()
                },
                JobReason::Watch,
            )
            .expect("second watcher batch should merge");

        assert!(followup.reused);
        assert_ne!(followup.job.id, first.job.id);
        assert_eq!(merged.job.id, followup.job.id);
        executor.releases.add_permits(1);
        executor.started.notified().await;
        executor.releases.add_permits(1);
        let completed = scheduler
            .wait(followup.job.id)
            .await
            .expect("followup should complete");
        assert_eq!(completed.job.state, JobState::Succeeded);
        let calls = executor.calls.lock().expect("calls should be readable");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].changes,
            [zg_engine::api::index::options::WorkspaceChange::Delete(
                PathBuf::from("src/lib.rs")
            )]
        );
    }

    #[tokio::test]
    async fn global_budget_queues_other_roots_and_reports_real_counts() {
        let executor = Arc::new(RecordingExecutor {
            calls: Mutex::new(Vec::new()),
            started: Notify::new(),
            releases: tokio::sync::Semaphore::new(0),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 1,
                queue_capacity: 8,
            },
        );
        let first = scheduler
            .submit(
                PathBuf::from("/first"),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("first job should be accepted");
        executor.started.notified().await;
        let second = scheduler
            .submit(
                PathBuf::from("/second"),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("second root should be queued");

        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.running, 1);
        assert_eq!(snapshot.queued, 1);
        executor.releases.add_permits(1);
        executor.started.notified().await;
        executor.releases.add_permits(1);
        assert_eq!(
            scheduler
                .wait(first.job.id)
                .await
                .expect("first job should finish")
                .job
                .state,
            JobState::Succeeded
        );
        assert_eq!(
            scheduler
                .wait(second.job.id)
                .await
                .expect("second job should finish")
                .job
                .state,
            JobState::Succeeded
        );
    }

    #[tokio::test]
    async fn queue_capacity_rejects_excess_jobs_without_unbounding_waiters() {
        let executor = Arc::new(RecordingExecutor {
            calls: Mutex::new(Vec::new()),
            started: Notify::new(),
            releases: tokio::sync::Semaphore::new(0),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 1,
                queue_capacity: 1,
            },
        );
        let first = scheduler
            .submit(
                PathBuf::from("/first"),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("running job should be accepted");
        executor.started.notified().await;
        let second = scheduler
            .submit(
                PathBuf::from("/second"),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("one queued job should be accepted");
        assert!(matches!(
            scheduler.submit(
                PathBuf::from("/third"),
                IndexOptions::default(),
                JobReason::Manual,
            ),
            Err(SchedulerError::QueueFull)
        ));

        executor.releases.add_permits(1);
        executor.started.notified().await;
        executor.releases.add_permits(1);
        scheduler
            .wait(first.job.id)
            .await
            .expect("first should finish");
        scheduler
            .wait(second.job.id)
            .await
            .expect("second should finish");
    }

    struct CancellationAwareExecutor {
        started: Notify,
    }

    #[async_trait]
    impl IndexExecutor for CancellationAwareExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            self.started.notify_one();
            options
                .signal
                .expect("scheduler should attach cancellation")
                .cancelled()
                .await;
            Err(EngineError::cancelled("indexing was cancelled"))
        }
    }

    struct ProgressExecutor;

    #[async_trait]
    impl IndexExecutor for ProgressExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            options
                .on_progress
                .expect("scheduler should attach progress")
                .report(zg_engine::api::index::progress::IndexProgress {
                    phase: zg_engine::api::index::progress::IndexProgressPhase::Indexing,
                    files_total: Some(10),
                    files_indexed: Some(4),
                    files_failed: Some(1),
                    detail: Some("embedding".to_owned()),
                    embedding: None,
                });
            Ok(IndexResult::default())
        }
    }

    struct SecretBearingErrorExecutor;

    #[async_trait]
    impl IndexExecutor for SecretBearingErrorExecutor {
        async fn index(&self, _options: IndexOptions) -> Result<IndexResult, EngineError> {
            Err(EngineError::internal(
                "authorization: Bearer super-secret api_key=also-secret token = third-secret",
            ))
        }
    }

    struct DownloadProgressExecutor {
        finish: Notify,
    }

    #[async_trait]
    impl IndexExecutor for DownloadProgressExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            use zg_engine::api::index::progress::{
                IndexEmbeddingProgress, IndexEmbeddingStage, IndexProgress, IndexProgressPhase,
            };
            let reporter = options.on_progress.expect("reporter");
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: Some(1),
                files_indexed: Some(0),
                files_failed: Some(0),
                detail: None,
                embedding: Some(IndexEmbeddingProgress {
                    stage: Some(IndexEmbeddingStage::Downloading),
                    downloaded_bytes: Some(128),
                    total_bytes: Some(256),
                    ..IndexEmbeddingProgress::default()
                }),
            });
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: Some(1),
                files_indexed: Some(0),
                files_failed: Some(0),
                detail: None,
                embedding: None,
            });
            self.finish.notified().await;
            Ok(IndexResult::default())
        }
    }

    #[tokio::test]
    async fn reused_job_streams_download_progress_before_completion() {
        use zg_engine::api::index::progress::IndexProgressReporter;
        let executor = Arc::new(DownloadProgressExecutor {
            finish: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(executor.clone(), SchedulerConfig::default());
        let root = std::env::temp_dir().join("progress-workspace");
        let original = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("submit");
        let reused = scheduler
            .submit(root, IndexOptions::default(), JobReason::Manual)
            .expect("reuse");
        assert_eq!(original.job.id, reused.job.id);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let reporter = IndexProgressReporter::new(move |progress| {
            let _ = sender.send(progress);
        });
        let waiting = scheduler.clone();
        let task = tokio::spawn(async move {
            waiting
                .wait_with_progress(reused.job.id, Some(reporter))
                .await
        });
        let progress = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
            .await
            .expect("live progress timeout")
            .expect("progress");
        assert_eq!(
            progress.embedding.expect("download").downloaded_bytes,
            Some(128)
        );
        assert!(!task.is_finished());
        executor.finish.notify_one();
        assert_eq!(
            task.await
                .expect("wait task")
                .expect("completion")
                .job
                .state,
            JobState::Succeeded
        );
    }

    #[tokio::test]
    async fn persisted_job_errors_redact_credentials() {
        let scheduler = IndexJobScheduler::new(
            Arc::new(SecretBearingErrorExecutor),
            SchedulerConfig::default(),
        );
        let submitted = scheduler
            .submit(
                PathBuf::from("/workspace"),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("job should be accepted");
        let completed = scheduler
            .wait(submitted.job.id)
            .await
            .expect("job should complete");
        let message = completed
            .job
            .error
            .expect("failed job should retain a safe error")
            .report
            .message;

        assert!(!message.contains("super-secret"));
        assert!(!message.contains("also-secret"));
        assert!(!message.contains("third-secret"));
        assert!(message.contains("[redacted]"));
    }

    #[tokio::test]
    async fn latest_progress_is_available_from_the_root_job_snapshot() {
        let scheduler =
            IndexJobScheduler::new(Arc::new(ProgressExecutor), SchedulerConfig::default());
        let root = PathBuf::from("/workspace");
        let submitted = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("job should be accepted");
        scheduler
            .wait(submitted.job.id)
            .await
            .expect("job should complete");

        let progress = scheduler
            .get_by_root(&root)
            .and_then(|job| job.progress)
            .expect("progress should be retained");
        assert_eq!(progress.files_total, Some(10));
        assert_eq!(progress.files_indexed, Some(4));
        assert_eq!(progress.files_failed, Some(1));
    }

    #[tokio::test]
    async fn cancel_root_reaches_the_running_engine_and_shutdown_rejects_new_work() {
        let executor = Arc::new(CancellationAwareExecutor {
            started: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(executor.clone(), SchedulerConfig::default());
        let root = PathBuf::from("/workspace");
        let submitted = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("job should be accepted");
        executor.started.notified().await;

        assert!(scheduler.cancel_root(&root));
        let completed = scheduler
            .wait(submitted.job.id)
            .await
            .expect("cancelled job should remain queryable");
        assert_eq!(completed.job.state, JobState::Cancelled);
        scheduler.shutdown().await;
        assert!(
            scheduler
                .submit(root, IndexOptions::default(), JobReason::Manual)
                .is_err()
        );
    }
}
