//! Bounded resident index-job scheduling with per-root writer serialization.

use std::{
    collections::{HashMap, VecDeque},
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
    api::index::{IndexOptions, IndexResult, options::ScanRulesUpdate},
    api::info::InfoOptions,
};

const MAX_PERSISTED_ERROR_CHARS: usize = 512;
const REDACTED: &str = "[redacted]";
// Finished jobs remain available for late waiters without growing resident history forever.
const MAX_RETAINED_FINISHED_JOBS: usize = 256;

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
    finished_job_ids: VecDeque<Uuid>,
}

impl SchedulerState {
    fn remove_finished_job(&mut self, id: Uuid) {
        // Existing waiters own the job independently of these lookup tables.
        if let Some(job) = self.jobs.remove(&id)
            && self
                .latest_by_root
                .get(&job.canonical_root)
                .is_some_and(|latest| latest.id == id)
        {
            self.latest_by_root.remove(&job.canonical_root);
        }
    }
}

struct ScheduledJob {
    id: Uuid,
    canonical_root: PathBuf,
    snapshot: Mutex<IndexJobSnapshot>,
    options: Mutex<Option<IndexOptions>>,
    cancellation: CancellationToken,
    shared: AtomicBool,
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
        mut options: IndexOptions,
        reason: JobReason,
    ) -> Result<SubmitIndexJobResult, SchedulerError> {
        // Naming is an explicit caller mutation, never a background refresh setting.
        if reason == JobReason::Watch {
            options.name = None;
        }
        // Changing the saved corpus can affect paths outside a narrow watcher batch.
        if has_selection_update(&options) {
            options.changes.clear();
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(SchedulerError::Closed);
        }
        let mut state = lock(&self.inner.state);
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(SchedulerError::Closed);
        }
        if let Some(active) = state.active_by_root.get(&canonical_root).cloned() {
            // Once another caller relies on a job, request cancellation must not stop it.
            active.shared.store(true, Ordering::Release);
            let active_snapshot = lock(&active.snapshot).clone();
            let needs_followup = reason == JobReason::Watch
                || active_snapshot.reason == JobReason::Watch
                || options.rebuild
                || options.name.is_some()
                || has_selection_update(&options);
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
                    followup.shared.store(true, Ordering::Release);
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
        Ok(wait_for_job(&job, reporter).await)
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

    pub(crate) fn has_active_root(&self, canonical_root: &PathBuf) -> bool {
        let state = lock(&self.inner.state);
        state.active_by_root.contains_key(canonical_root)
            || state.followup_by_root.contains_key(canonical_root)
    }

    pub(crate) fn cancel_root(&self, canonical_root: &PathBuf) -> bool {
        let (active, followup) = {
            let mut state = lock(&self.inner.state);
            let active = state.active_by_root.get(canonical_root).cloned();
            let followup = state.followup_by_root.remove(canonical_root);
            if let Some(ref followup) = followup {
                followup.cancellation.cancel();
                finish_cancelled(followup, "indexing was cancelled");
                mark_finished(&self.inner, &mut state, followup);
            }
            (active, followup)
        };
        if let Some(active) = active {
            active.cancellation.cancel();
            true
        } else {
            followup.is_some()
        }
    }

    /// Cancel remaining work and release a root's retained history. Call after
    /// draining the root; otherwise active jobs enter retention when they finish.
    pub(crate) fn forget_root(&self, canonical_root: &PathBuf) {
        self.cancel_root(canonical_root);
        let mut state = lock(&self.inner.state);
        let forgotten = state
            .jobs
            .values()
            .filter(|job| {
                job.canonical_root == *canonical_root && job.finished.load(Ordering::Acquire)
            })
            .map(|job| job.id)
            .collect::<Vec<_>>();
        for id in forgotten {
            state.remove_finished_job(id);
        }
        let SchedulerState {
            jobs,
            finished_job_ids,
            ..
        } = &mut *state;
        finished_job_ids.retain(|id| jobs.contains_key(id));
    }

    pub(crate) async fn wait_for_root_idle(&self, canonical_root: &PathBuf) {
        self.wait_for_root_idle_with_progress(canonical_root, None)
            .await;
    }

    pub(crate) async fn wait_for_root_idle_with_progress(
        &self,
        canonical_root: &PathBuf,
        reporter: Option<zg_engine::api::index::progress::IndexProgressReporter>,
    ) {
        loop {
            let active = lock(&self.inner.state)
                .active_by_root
                .get(canonical_root)
                .cloned();
            let Some(active) = active else {
                return;
            };
            wait_for_job(&active, reporter.clone()).await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) {
            let active = {
                let mut state = lock(&self.inner.state);
                let active = state.active_by_root.values().cloned().collect::<Vec<_>>();
                let followups = state
                    .followup_by_root
                    .drain()
                    .map(|(_, job)| job)
                    .collect::<Vec<_>>();
                for job in followups {
                    job.cancellation.cancel();
                    finish_cancelled(
                        &job,
                        "indexing was cancelled because the daemon is shutting down",
                    );
                    mark_finished(&self.inner, &mut state, &job);
                }
                active
            };
            for job in active {
                job.cancellation.cancel();
            }
            self.inner.permits.close();
        }
        loop {
            let notified = self.inner.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
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
        shared: AtomicBool::new(reason == JobReason::Watch),
        result: Mutex::new(None),
        finished: AtomicBool::new(false),
        completed: Notify::new(),
    })
}

fn spawn_job(inner: Arc<SchedulerInner>, job: Arc<ScheduledJob>) {
    if let Some(signal) = lock(&job.options)
        .as_ref()
        .and_then(|options| options.signal.clone())
    {
        let inner = Arc::clone(&inner);
        let job = Arc::clone(&job);
        tokio::spawn(async move {
            let completed = job.completed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            if job.finished.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                () = completed => {},
                () = signal.cancelled() => {
                    let _state = lock(&inner.state);
                    if !job.shared.load(Ordering::Acquire) {
                        job.cancellation.cancel();
                    }
                }
            }
        });
    }
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
        let mut options = {
            // submit inspects state and merges options under this same lock.
            // Claiming must be atomic with that decision or a queued grant can be lost.
            let _state = lock(&inner.state);
            lock(&job.snapshot).state = JobState::Running;
            lock(&job.options)
                .take()
                .expect("a queued daemon job must retain its index options")
        };
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
        // Publish completion and retention before the root can be observed as idle.
        mark_finished(inner, &mut state, job);
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
}

fn has_selection_update(options: &IndexOptions) -> bool {
    options.reset_paths || options.scan != ScanRulesUpdate::default()
}

fn merge_options(current: &mut Option<IndexOptions>, mut incoming: IndexOptions) {
    let Some(current) = current.as_mut() else {
        return;
    };
    let full_scan = current.changes.is_empty()
        || incoming.changes.is_empty()
        || has_selection_update(current)
        || has_selection_update(&incoming);
    // A later reset discards earlier pending selection changes. Otherwise every
    // explicitly supplied field wins, including false, empty lists and null limits.
    if incoming.reset_paths {
        current.scan = ScanRulesUpdate::default();
    }
    current.reset_paths |= incoming.reset_paths;
    current.rebuild |= incoming.rebuild;
    if incoming.scan.globs.is_some() {
        current.scan.sensitive_globs = None;
        current.scan.insensitive_globs = None;
    }
    merge_update(&mut current.scan.globs, incoming.scan.globs.take());
    merge_update(
        &mut current.scan.sensitive_globs,
        incoming.scan.sensitive_globs.take(),
    );
    merge_update(
        &mut current.scan.insensitive_globs,
        incoming.scan.insensitive_globs.take(),
    );
    merge_update(
        &mut current.scan.file_types,
        incoming.scan.file_types.take(),
    );
    merge_update(
        &mut current.scan.excluded_file_types,
        incoming.scan.excluded_file_types.take(),
    );
    merge_update(&mut current.scan.hidden, incoming.scan.hidden.take());
    merge_update(&mut current.scan.no_ignore, incoming.scan.no_ignore.take());
    merge_update(
        &mut current.scan.follow_symlinks,
        incoming.scan.follow_symlinks.take(),
    );
    merge_update(
        &mut current.scan.nested_git,
        incoming.scan.nested_git.take(),
    );
    merge_update(
        &mut current.scan.ignore_files,
        incoming.scan.ignore_files.take(),
    );
    merge_update(&mut current.scan.max_depth, incoming.scan.max_depth.take());
    merge_update(
        &mut current.scan.max_file_size_bytes,
        incoming.scan.max_file_size_bytes.take(),
    );
    merge_runtime_options(current, &mut incoming);
    // An omitted name leaves a pending explicit rename intact.
    merge_update(&mut current.name, incoming.name.take());
    if incoming.changes.is_empty() {
        current.changes.clear();
        return;
    }
    if full_scan {
        current.changes.clear();
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

fn merge_runtime_options(current: &mut IndexOptions, incoming: &mut IndexOptions) {
    let destination_changed = incoming
        .embedding
        .as_ref()
        .is_some_and(|model| current.embedding.as_ref() != Some(model))
        || incoming
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| current.endpoint.as_ref() != Some(endpoint));
    if destination_changed {
        current.allow_remote = incoming.allow_remote;
        current.authorized_remote.clear();
        current.api_key = None;
        current.endpoint = None;
    }
    // Runtime/model settings are explicit updates too. A filter-only request
    // must not erase the model or credentials of an already queued manual job.
    merge_update(&mut current.root, incoming.root.take());
    merge_update(&mut current.embedding, incoming.embedding.take());
    merge_update(&mut current.api_key, incoming.api_key.take());
    merge_update(&mut current.endpoint, incoming.endpoint.take());
    merge_update(&mut current.device, incoming.device.take());
    merge_update(&mut current.runtime_device, incoming.runtime_device.take());
    merge_update(&mut current.model_cache, incoming.model_cache.take());
    merge_update(
        &mut current.lock_timeout_ms,
        incoming.lock_timeout_ms.take(),
    );
    merge_update(
        &mut current.embedding_concurrency,
        incoming.embedding_concurrency.take(),
    );
    merge_update(&mut current.signal, incoming.signal.take());
    merge_update(&mut current.on_progress, incoming.on_progress.take());
    // Consent is scoped to this coalesced job. Runtime watch templates clear it.
    current.allow_remote |= incoming.allow_remote;
    for target in incoming.authorized_remote.drain(..) {
        if !current.authorized_remote.contains(&target) {
            current.authorized_remote.push(target);
        }
    }
}

fn merge_update<T>(current: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *current = incoming;
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
    let mut redacted = redact_url_userinfo(message);
    redacted = redact_assigned_value(&redacted, "authorization", true);
    for scheme in ["bearer", "basic"] {
        redacted = redact_auth_scheme(&redacted, scheme);
    }
    for name in [
        "access_token",
        "access-token",
        "access token",
        "accesstoken",
        "refresh_token",
        "refresh-token",
        "refresh token",
        "refreshtoken",
        "id_token",
        "id-token",
        "id token",
        "idtoken",
        "api_key",
        "api-key",
        "api key",
        "apikey",
        "password",
        "secret",
        "token",
    ] {
        redacted = redact_assigned_value(&redacted, name, false);
    }
    redacted = redact_openai_keys(&redacted);
    let mut truncated = redacted
        .chars()
        .take(MAX_PERSISTED_ERROR_CHARS)
        .collect::<String>();
    if redacted.chars().count() > MAX_PERSISTED_ERROR_CHARS {
        truncated.push('…');
    }
    truncated
}

fn redact_url_userinfo(message: &str) -> String {
    let mut output = message.to_owned();
    let mut cursor = 0;
    loop {
        let Some(relative_marker) = output[cursor..].find("://") else {
            return output;
        };
        let marker = cursor + relative_marker;
        let mut scheme_start = marker;
        while scheme_start > 0
            && (output.as_bytes()[scheme_start - 1].is_ascii_alphanumeric()
                || matches!(output.as_bytes()[scheme_start - 1], b'+' | b'.' | b'-'))
        {
            scheme_start -= 1;
        }
        let valid_scheme = scheme_start < marker
            && output.as_bytes()[scheme_start].is_ascii_alphabetic()
            && (scheme_start == 0
                || !matches!(
                    output.as_bytes()[scheme_start - 1],
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'.' | b'-'
                ));
        let authority_start = marker + 3;
        let authority_end = output.as_bytes()[authority_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || *byte == b'/')
            .map_or(output.len(), |offset| authority_start + offset);
        if valid_scheme && let Some(relative_at) = output[authority_start..authority_end].find('@')
        {
            let userinfo_end = authority_start + relative_at;
            output.replace_range(authority_start..userinfo_end, REDACTED);
            cursor = authority_start + REDACTED.len() + 1;
        } else {
            cursor = authority_start;
        }
    }
}

fn redact_auth_scheme(message: &str, scheme: &str) -> String {
    let mut output = message.to_owned();
    let mut cursor = 0;
    loop {
        let lowercase = output.to_ascii_lowercase();
        let Some(relative_start) = lowercase[cursor..].find(scheme) else {
            return output;
        };
        let marker_start = cursor + relative_start;
        let marker_end = marker_start + scheme.len();
        let before_is_word =
            marker_start > 0 && is_identifier_byte(lowercase.as_bytes()[marker_start - 1]);
        let after_is_space = lowercase
            .as_bytes()
            .get(marker_end)
            .is_some_and(u8::is_ascii_whitespace);
        if before_is_word || !after_is_space {
            cursor = marker_end;
            continue;
        }
        let value_start = skip_ascii_whitespace(output.as_bytes(), marker_end);
        let value_end = quoted_or_token_end(output.as_bytes(), value_start);
        if value_start == value_end {
            cursor = marker_end;
            continue;
        }
        output.replace_range(value_start..value_end, REDACTED);
        cursor = value_start + REDACTED.len();
    }
}

fn redact_assigned_value(message: &str, name: &str, allow_spaces: bool) -> String {
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
        let mut separator = name_end;
        if output
            .as_bytes()
            .get(separator)
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
        {
            separator += 1;
        }
        separator = skip_ascii_whitespace(output.as_bytes(), separator);
        if !output
            .as_bytes()
            .get(separator)
            .is_some_and(|byte| matches!(byte, b'=' | b':'))
        {
            cursor = name_end;
            continue;
        }
        let value_start = skip_ascii_whitespace(output.as_bytes(), separator + 1);
        let value_end = if allow_spaces {
            quoted_or_line_end(output.as_bytes(), value_start)
        } else {
            quoted_or_assigned_value_end(output.as_bytes(), value_start)
        };
        if value_start == value_end {
            cursor = name_end;
            continue;
        }
        output.replace_range(value_start..value_end, REDACTED);
        cursor = value_start + REDACTED.len();
    }
}

fn redact_openai_keys(message: &str) -> String {
    let mut output = message.to_owned();
    let mut cursor = 0;
    loop {
        let lowercase = output.to_ascii_lowercase();
        let Some(relative_start) = lowercase[cursor..].find("sk-") else {
            return output;
        };
        let start = cursor + relative_start;
        if start > 0 && is_identifier_byte(lowercase.as_bytes()[start - 1]) {
            cursor = start + 3;
            continue;
        }
        let mut end = start + 3;
        while output
            .as_bytes()
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            end += 1;
        }
        if end - (start + 3) < 8 {
            cursor = end;
            continue;
        }
        output.replace_range(start..end, "sk-[redacted]");
        cursor = start + "sk-[redacted]".len();
    }
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn quoted_or_token_end(bytes: &[u8], cursor: usize) -> usize {
    quoted_end(bytes, cursor).unwrap_or_else(|| {
        let mut end = cursor;
        while bytes.get(end).is_some_and(|byte| {
            !byte.is_ascii_whitespace() && !matches!(byte, b'"' | b'\'' | b',' | b';' | b'&')
        }) {
            end += 1;
        }
        end
    })
}

fn quoted_or_assigned_value_end(bytes: &[u8], cursor: usize) -> usize {
    quoted_end(bytes, cursor).unwrap_or_else(|| {
        let mut end = cursor;
        while bytes
            .get(end)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'&')
        {
            end += 1;
        }
        end
    })
}

fn quoted_or_line_end(bytes: &[u8], cursor: usize) -> usize {
    quoted_end(bytes, cursor).unwrap_or_else(|| {
        let mut end = cursor;
        while bytes
            .get(end)
            .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
        {
            end += 1;
        }
        end
    })
}

fn quoted_end(bytes: &[u8], cursor: usize) -> Option<usize> {
    let quote @ (b'\'' | b'"') = *bytes.get(cursor)? else {
        return None;
    };
    let mut end = cursor + 1;
    while let Some(byte) = bytes.get(end) {
        if *byte == b'\\' {
            end = (end + 2).min(bytes.len());
        } else {
            end += 1;
            if *byte == quote {
                break;
            }
        }
    }
    Some(end)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

async fn wait_for_job(
    job: &ScheduledJob,
    reporter: Option<zg_engine::api::index::progress::IndexProgressReporter>,
) -> IndexJobCompletion {
    let mut previous = None;
    loop {
        let notified = job.completed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Observe completion before copying the snapshot so a terminal result can
        // never be returned with an earlier queued/running snapshot.
        let finished = job.finished.load(Ordering::Acquire);
        let snapshot = lock(&job.snapshot).clone();
        if let Some(reporter) = &reporter
            && snapshot.progress != previous
        {
            if let Some(progress) = &snapshot.progress {
                reporter.report(progress.clone());
            }
            previous.clone_from(&snapshot.progress);
        }
        if finished {
            return IndexJobCompletion {
                job: snapshot,
                result: lock(&job.result).clone(),
            };
        }
        tokio::select! {
            () = notified => {},
            () = tokio::time::sleep(std::time::Duration::from_millis(100)), if reporter.is_some() => {},
        }
    }
}

fn mark_finished(inner: &SchedulerInner, state: &mut SchedulerState, job: &ScheduledJob) {
    // Queued cancellations never pass their options to the executor. Release
    // credentials and caller callbacks instead of pinning them in job history.
    lock(&job.options).take();
    state.finished_job_ids.push_back(job.id);
    while state.finished_job_ids.len() > MAX_RETAINED_FINISHED_JOBS {
        if let Some(oldest) = state.finished_job_ids.pop_front() {
            state.remove_finished_job(oldest);
        }
    }
    job.finished.store(true, Ordering::Release);
    job.completed.notify_waiters();
    if inner.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
        inner.drained.notify_waiters();
    }
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
        api::index::{
            IndexOptions, IndexResult,
            options::{ScanRulesUpdate, WorkspaceChange},
        },
    };

    use super::{
        IndexExecutor, IndexJobScheduler, JobReason, JobState, SchedulerConfig, SchedulerError,
        redact_job_error_text,
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
                ..IndexResult::default()
            })
        }
    }

    struct ImmediateExecutor;

    #[async_trait]
    impl IndexExecutor for ImmediateExecutor {
        async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
            match options.name.as_deref() {
                Some("failed") => Err(EngineError::internal("fixture failure")),
                Some("cancelled") => Err(EngineError::cancelled("fixture cancellation")),
                _ => Ok(IndexResult::default()),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claiming_a_job_is_serialized_with_submission() {
        let scheduler =
            IndexJobScheduler::new(Arc::new(ImmediateExecutor), SchedulerConfig::default());
        let root = std::env::temp_dir().join("claim-serialization");
        let permits = scheduler
            .inner
            .permits
            .clone()
            .acquire_many_owned(2)
            .await
            .expect("permits");
        let submitted = scheduler
            .submit(root, IndexOptions::default(), JobReason::Watch)
            .expect("submit");
        {
            let state = super::lock(&scheduler.inner.state);
            let job = state.jobs.get(&submitted.job.id).expect("job");
            drop(permits);
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(
                super::lock(&job.snapshot).state,
                JobState::Queued,
                "a worker must not claim options while submit owns scheduler state"
            );
        }
        scheduler.wait(submitted.job.id).await.expect("completion");
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn finished_history_evicts_oldest_jobs_and_their_root_records() {
        let scheduler =
            IndexJobScheduler::new(Arc::new(ImmediateExecutor), SchedulerConfig::default());
        let mut completed = Vec::new();
        for index in 0..300 {
            let root = PathBuf::from(format!("/workspace-{index}"));
            let (name, expected) = match index % 3 {
                0 => ("succeeded", JobState::Succeeded),
                1 => ("failed", JobState::Failed),
                _ => ("cancelled", JobState::Cancelled),
            };
            let job = scheduler
                .submit(
                    root.clone(),
                    IndexOptions {
                        name: Some(name.into()),
                        ..IndexOptions::default()
                    },
                    JobReason::Manual,
                )
                .expect("submit");
            assert_eq!(
                scheduler
                    .wait(job.job.id)
                    .await
                    .expect("completion")
                    .job
                    .state,
                expected
            );
            completed.push((root, job.job.id, expected));
        }
        assert_eq!(super::lock(&scheduler.inner.state).jobs.len(), 256);
        for (index, (root, id, expected)) in completed.into_iter().enumerate() {
            if index < 44 {
                assert!(matches!(
                    scheduler.wait(id).await,
                    Err(SchedulerError::UnknownJob(_))
                ));
                assert!(scheduler.get_by_root(&root).is_none());
            } else {
                assert_eq!(
                    scheduler.wait(id).await.expect("retained job").job.state,
                    expected
                );
                assert_eq!(scheduler.get_by_root(&root).expect("root job").id, id);
            }
        }
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn eviction_preserves_active_queued_followup_jobs_and_existing_waiters() {
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
        let root = PathBuf::from("/workspace");
        let first = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("first job");
        executor.started.notified().await;
        let first_job = Arc::downgrade(&super::lock(&scheduler.inner.state).jobs[&first.job.id]);
        let waiter = scheduler.wait(first.job.id);
        tokio::pin!(waiter);
        tokio::select! {
            biased;
            _ = &mut waiter => panic!("job must still be running"),
            () = std::future::ready(()) => {},
        }
        executor.releases.add_permits(1);
        scheduler
            .wait(first.job.id)
            .await
            .expect("first completion");
        let active = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("replacement job");
        executor.started.notified().await;
        let followup = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Watch)
            .expect("followup");
        let queued_root = PathBuf::from("/queued");
        let queued = scheduler
            .submit(
                queued_root.clone(),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("queued job");
        for index in 0..300 {
            let cancelled_root = PathBuf::from(format!("/cancelled-{index}"));
            let job = scheduler
                .submit(
                    cancelled_root.clone(),
                    IndexOptions::default(),
                    JobReason::Manual,
                )
                .expect("queued cancellation");
            scheduler.cancel_root(&cancelled_root);
            let cancelled = scheduler.wait(job.job.id).await.expect("cancelled");
            assert_eq!(cancelled.job.state, JobState::Cancelled);
            assert!(
                super::lock(&super::lock(&scheduler.inner.state).jobs[&job.job.id].options)
                    .is_none()
            );
        }
        assert!(matches!(
            scheduler.wait(first.job.id).await,
            Err(SchedulerError::UnknownJob(_))
        ));
        assert_eq!(super::lock(&scheduler.inner.state).jobs.len(), 259);
        assert_eq!(scheduler.snapshot().running, 1);
        assert_eq!(scheduler.snapshot().queued, 2);
        assert_eq!(
            scheduler.get_by_root(&root).expect("active root").id,
            active.job.id
        );
        assert_eq!(
            scheduler.get_by_root(&queued_root).expect("queued root").id,
            queued.job.id
        );
        let completed = waiter.await.expect("attached waiter survives eviction");
        assert_eq!(completed.job.state, JobState::Succeeded);
        assert!(completed.result.is_some());
        assert!(
            first_job.upgrade().is_none(),
            "evicted job must be released after its waiter completes"
        );
        executor.releases.add_permits(3);
        for id in [active.job.id, followup.job.id, queued.job.id] {
            assert_eq!(
                scheduler.wait(id).await.expect("preserved job").job.state,
                JobState::Succeeded
            );
        }
        assert_eq!(
            scheduler.get_by_root(&root).expect("latest followup").id,
            followup.job.id
        );
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn forget_root_removes_only_its_finished_history_and_allows_reuse() {
        let scheduler =
            IndexJobScheduler::new(Arc::new(ImmediateExecutor), SchedulerConfig::default());
        let root = PathBuf::from("/forgotten");
        let other = PathBuf::from("/retained");
        let mut forgotten = Vec::new();
        for index in 0..3 {
            let job = scheduler
                .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
                .expect("submit");
            scheduler.wait(job.job.id).await.expect("finish");
            forgotten.push(job.job.id);
            let retained = scheduler
                .submit(other.clone(), IndexOptions::default(), JobReason::Manual)
                .expect("other root");
            scheduler.wait(retained.job.id).await.expect("finish other");
            assert_eq!(
                super::lock(&scheduler.inner.state).finished_job_ids.len(),
                (index + 1) * 2
            );
        }
        scheduler.wait_for_root_idle(&root).await;
        scheduler.forget_root(&root);
        scheduler.forget_root(&root);
        assert!(scheduler.get_by_root(&root).is_none());
        assert!(scheduler.get_by_root(&other).is_some());
        assert_eq!(
            super::lock(&scheduler.inner.state).finished_job_ids.len(),
            3
        );
        for id in forgotten {
            assert!(matches!(
                scheduler.wait(id).await,
                Err(SchedulerError::UnknownJob(_))
            ));
        }
        let replacement = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("reuse root");
        assert!(!replacement.reused);
        scheduler
            .wait(replacement.job.id)
            .await
            .expect("replacement finishes");
        assert_eq!(
            scheduler.get_by_root(&root).expect("replacement").id,
            replacement.job.id
        );
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_followups_are_retained_and_shutdown_finishes_before_returning() {
        let executor = Arc::new(GatedExecutor {
            started: Notify::new(),
            release: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(executor.clone(), SchedulerConfig::default());
        let root = PathBuf::from("/workspace");
        let active = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("running job");
        executor.started.notified().await;
        for _ in 0..300 {
            let followup = scheduler
                .submit(root.clone(), IndexOptions::default(), JobReason::Watch)
                .expect("followup");
            scheduler.cancel_root(&root);
            assert_eq!(
                scheduler
                    .wait(followup.job.id)
                    .await
                    .expect("cancelled followup")
                    .job
                    .state,
                JobState::Cancelled
            );
        }
        assert_eq!(
            super::lock(&scheduler.inner.state).finished_job_ids.len(),
            256
        );
        assert_eq!(super::lock(&scheduler.inner.state).jobs.len(), 257);
        assert_eq!(
            scheduler.get_by_root(&root).expect("running root").id,
            active.job.id
        );
        let followup = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Watch)
            .expect("shutdown followup");
        let shutting_down = scheduler.shutdown();
        tokio::pin!(shutting_down);
        tokio::select! {
            biased;
            () = &mut shutting_down => panic!("running executor has not finished"),
            () = std::future::ready(()) => {},
        }
        assert_eq!(
            scheduler
                .wait(followup.job.id)
                .await
                .expect("shutdown cancellation")
                .job
                .state,
            JobState::Cancelled
        );
        executor.release.notify_one();
        shutting_down.await;
        assert!(
            super::lock(&scheduler.inner.state)
                .active_by_root
                .is_empty()
        );
        assert_eq!(
            super::lock(&scheduler.inner.state).finished_job_ids.len(),
            256
        );
        assert_eq!(
            scheduler
                .wait(active.job.id)
                .await
                .expect("executor finished")
                .job
                .state,
            JobState::Succeeded
        );
        scheduler.forget_root(&root);
        assert!(super::lock(&scheduler.inner.state).jobs.is_empty());
        assert!(
            super::lock(&scheduler.inner.state)
                .finished_job_ids
                .is_empty()
        );
    }

    #[tokio::test]
    async fn forgetting_a_busy_root_keeps_active_work_and_attached_waiters_alive() {
        let executor = Arc::new(CancellationAwareExecutor {
            started: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(executor.clone(), SchedulerConfig::default());
        let root = PathBuf::from("/workspace");
        let active = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
            .expect("running job");
        executor.started.notified().await;
        let followup = scheduler
            .submit(root.clone(), IndexOptions::default(), JobReason::Watch)
            .expect("followup");
        let waiter = scheduler.wait(followup.job.id);
        tokio::pin!(waiter);
        tokio::select! {
            biased;
            _ = &mut waiter => panic!("followup is still queued"),
            () = std::future::ready(()) => {},
        }
        scheduler.forget_root(&root);
        assert_eq!(
            scheduler
                .get_by_root(&root)
                .expect("active job is preserved")
                .id,
            active.job.id
        );
        assert!(matches!(
            scheduler.wait(followup.job.id).await,
            Err(SchedulerError::UnknownJob(_))
        ));
        assert_eq!(
            waiter.await.expect("attached waiter").job.state,
            JobState::Cancelled
        );
        scheduler.wait_for_root_idle(&root).await;
        assert_eq!(
            scheduler
                .wait(active.job.id)
                .await
                .expect("cancelled active job")
                .job
                .state,
            JobState::Cancelled
        );
        scheduler.forget_root(&root);
        assert!(scheduler.get_by_root(&root).is_none());
        assert!(super::lock(&scheduler.inner.state).jobs.is_empty());
        scheduler.shutdown().await;
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
        assert!(completed.result.is_some());
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
                    name: Some("stale-workspace-name".into()),
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
                    name: Some("another-stale-name".into()),
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
        assert!(calls[1].name.is_none());
        assert_eq!(
            calls[1].changes,
            [zg_engine::api::index::options::WorkspaceChange::Delete(
                PathBuf::from("src/lib.rs")
            )]
        );
    }

    #[tokio::test]
    async fn selection_updates_survive_queued_and_running_manual_jobs_and_watch_merges() {
        for queued in [false, true] {
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
            if queued {
                scheduler
                    .submit(
                        PathBuf::from("/blocker"),
                        IndexOptions::default(),
                        JobReason::Manual,
                    )
                    .expect("occupy worker");
                executor.started.notified().await;
            }
            let root = PathBuf::from("/workspace");
            let first = scheduler
                .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
                .expect("initial index");
            if !queued {
                executor.started.notified().await;
            }
            let patch = scheduler
                .submit(
                    root.clone(),
                    IndexOptions {
                        scan: ScanRulesUpdate {
                            globs: Some(vec!["*.rs".into()]),
                            hidden: Some(true),
                            follow_symlinks: Some(true),
                            max_depth: Some(Some(3)),
                            ..Default::default()
                        },
                        changes: vec![WorkspaceChange::Upsert("limited.rs".into())],
                        ..Default::default()
                    },
                    JobReason::Manual,
                )
                .expect("selection update");
            assert_eq!(patch.job.id == first.job.id, queued);
            let merged = scheduler
                .submit(
                    root.clone(),
                    IndexOptions {
                        scan: ScanRulesUpdate {
                            globs: Some(Vec::new()),
                            hidden: Some(false),
                            max_depth: Some(None),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    JobReason::Manual,
                )
                .expect("second update");
            assert_eq!(merged.job.id, patch.job.id);
            scheduler
                .submit(
                    root,
                    IndexOptions {
                        changes: vec![WorkspaceChange::Upsert("changed.rs".into())],
                        ..Default::default()
                    },
                    JobReason::Watch,
                )
                .expect("watch update");
            executor.releases.add_permits(1);
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                executor.started.notified(),
            )
            .await
            .expect("selection update reaches executor");
            executor.releases.add_permits(1);
            scheduler
                .wait(patch.job.id)
                .await
                .expect("selection update completes");
            {
                let calls = executor.calls.lock().expect("recorded calls");
                let applied = calls.last().expect("selection execution");
                assert_merged_selection(applied);
            }
            scheduler.shutdown().await;
        }
    }

    fn assert_merged_selection(applied: &IndexOptions) {
        assert_eq!(applied.scan.globs, Some(Vec::new()));
        assert_eq!(applied.scan.hidden, Some(false));
        assert_eq!(applied.scan.follow_symlinks, Some(true));
        assert_eq!(applied.scan.max_depth, Some(None));
        assert!(
            applied.changes.is_empty(),
            "selection changes require a full scan"
        );
    }

    #[test]
    fn nested_git_updates_keep_false_and_omission_until_reset() {
        let mut pending = Some(IndexOptions {
            scan: ScanRulesUpdate {
                nested_git: Some(true),
                ..ScanRulesUpdate::default()
            },
            ..IndexOptions::default()
        });
        super::merge_options(
            &mut pending,
            IndexOptions {
                scan: ScanRulesUpdate {
                    nested_git: Some(false),
                    ..ScanRulesUpdate::default()
                },
                ..IndexOptions::default()
            },
        );
        super::merge_options(&mut pending, IndexOptions::default());
        let update = pending.as_ref().expect("pending configuration");
        assert_eq!(update.scan.nested_git, Some(false));
        assert!(super::has_selection_update(update));
        super::merge_options(
            &mut pending,
            IndexOptions {
                reset_paths: true,
                ..IndexOptions::default()
            },
        );
        let reset = pending.expect("pending reset");
        assert!(reset.reset_paths);
        assert_eq!(reset.scan.nested_git, None);
    }

    #[test]
    fn merged_refresh_preserves_target_scoped_once_consent() {
        let target = zg_engine::authorization::IndexAuthorization {
            root: std::env::temp_dir(),
            workspace_roots: vec![std::env::temp_dir()],
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://a.test/embeddings".into(),
            endpoint_host: "a.test".into(),
        };
        let mut pending = Some(IndexOptions::default());
        super::merge_options(
            &mut pending,
            IndexOptions {
                authorized_remote: vec![target.clone()],
                ..IndexOptions::default()
            },
        );
        super::merge_options(
            &mut pending,
            IndexOptions {
                authorized_remote: vec![target.clone()],
                runtime_device: Some(zg_engine::api::index::options::Device::Cpu),
                ..IndexOptions::default()
            },
        );
        let merged = pending.as_ref().expect("merged");
        assert_eq!(merged.authorized_remote, [target]);
        assert!(!merged.allow_remote);
        assert!(merged.device.is_none());
        assert_eq!(
            merged.runtime_device,
            Some(zg_engine::api::index::options::Device::Cpu)
        );
        super::merge_options(
            &mut pending,
            IndexOptions {
                endpoint: Some("https://b.test/embeddings".into()),
                ..IndexOptions::default()
            },
        );
        assert!(
            pending
                .expect("new destination")
                .authorized_remote
                .is_empty()
        );
    }

    #[test]
    fn changing_remote_destination_does_not_inherit_pending_credentials_or_consent() {
        use zg_engine::api::index::options::{Device, EmbeddingModelSpec};
        for model_changed in [false, true] {
            let mut pending = Some(IndexOptions {
                api_key: Some("provider-a-key".into()),
                endpoint: Some("https://a.test".into()),
                allow_remote: true,
                ..IndexOptions::default()
            });
            let incoming = if model_changed {
                IndexOptions {
                    embedding: Some(EmbeddingModelSpec {
                        reference: "provider-b/model".into(),
                        revision: None,
                        cache_dir: None,
                        endpoint: None,
                        device: Device::Auto,
                    }),
                    ..IndexOptions::default()
                }
            } else {
                IndexOptions {
                    endpoint: Some("https://b.test".into()),
                    ..IndexOptions::default()
                }
            };
            super::merge_options(&mut pending, incoming);
            let merged = pending.expect("merged destination");
            assert!(!merged.allow_remote);
            assert!(merged.api_key.is_none());
            assert_eq!(
                merged.endpoint.as_deref(),
                if model_changed {
                    None
                } else {
                    Some("https://b.test")
                }
            );
        }
    }

    #[test]
    fn filter_updates_preserve_pending_model_runtime_settings_and_job_consent() {
        use zg_engine::api::index::options::{Device, EmbeddingModelSpec};
        let mut pending = Some(IndexOptions {
            rebuild: true,
            allow_remote: true,
            api_key: Some("test-key".into()),
            endpoint: Some("https://example.test/embeddings".into()),
            device: Some(Device::Cpu),
            model_cache: Some("/models".into()),
            lock_timeout_ms: Some(1_000),
            embedding_concurrency: Some(4),
            embedding: Some(EmbeddingModelSpec {
                reference: "qwen/new-model".into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Cpu,
            }),
            ..IndexOptions::default()
        });
        super::merge_options(
            &mut pending,
            IndexOptions {
                scan: ScanRulesUpdate {
                    globs: Some(Vec::new()),
                    ..ScanRulesUpdate::default()
                },
                embedding_concurrency: Some(8),
                lock_timeout_ms: Some(2_000),
                ..IndexOptions::default()
            },
        );
        let merged = pending.expect("merged request");
        assert!(merged.rebuild && merged.allow_remote);
        assert_eq!(
            merged.embedding.expect("explicit model").reference,
            "qwen/new-model"
        );
        assert_eq!(merged.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            merged.endpoint.as_deref(),
            Some("https://example.test/embeddings")
        );
        assert_eq!(merged.device, Some(Device::Cpu));
        assert_eq!(merged.model_cache, Some("/models".into()));
        assert_eq!(merged.embedding_concurrency, Some(8));
        assert_eq!(merged.lock_timeout_ms, Some(2_000));
        assert_eq!(merged.scan.globs, Some(Vec::new()));
    }

    #[test]
    fn queued_glob_updates_keep_both_supplied_categories() {
        let mut pending = Some(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["*.rs".into()]),
                ..Default::default()
            },
            ..Default::default()
        });
        super::merge_options(
            &mut pending,
            IndexOptions {
                scan: ScanRulesUpdate {
                    insensitive_globs: Some(vec![zg_engine::api::index::options::GlobRule {
                        pattern: "*.MD".into(),
                        case_insensitive: true,
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut scan = zg_engine::api::index::options::ScanRules::default();
        pending.expect("merged request").scan.apply(&mut scan);
        assert_eq!(scan.globs.len(), 2);
        assert_eq!(scan.globs[0].pattern, "*.rs");
        assert_eq!(scan.globs[1].pattern, "*.MD");
        assert!(scan.globs[1].case_insensitive);
    }

    #[test]
    fn queued_complete_globs_discard_earlier_category_updates() {
        let mut pending = Some(IndexOptions {
            scan: ScanRulesUpdate {
                sensitive_globs: Some(vec!["old/**".into()]),
                insensitive_globs: Some(vec![zg_engine::api::index::options::GlobRule {
                    pattern: "!secret/**".into(),
                    case_insensitive: true,
                }]),
                ..Default::default()
            },
            ..Default::default()
        });
        super::merge_options(
            &mut pending,
            IndexOptions {
                scan: ScanRulesUpdate {
                    globs: Some(vec!["secret/**".into()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut scan = zg_engine::api::index::options::ScanRules::default();
        pending.expect("merged request").scan.apply(&mut scan);
        assert_eq!(
            scan.globs,
            vec![zg_engine::api::index::options::GlobRule::from("secret/**")]
        );
    }

    #[test]
    fn a_later_reset_discards_pending_selection_updates_but_remains_pending_for_later_patches() {
        use zg_engine::api::index::options::ScanRulesUpdate;

        let mut pending = Some(IndexOptions {
            scan: ScanRulesUpdate {
                globs: Some(vec!["old/**".into()]),
                hidden: Some(true),
                max_depth: Some(Some(3)),
                ..Default::default()
            },
            ..Default::default()
        });
        super::merge_options(
            &mut pending,
            IndexOptions {
                reset_paths: true,
                scan: ScanRulesUpdate {
                    follow_symlinks: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let reset = pending.as_ref().expect("pending reset");
        assert!(reset.reset_paths);
        assert_eq!(reset.scan.globs, None);
        assert_eq!(reset.scan.hidden, None);
        assert_eq!(reset.scan.max_depth, None);
        super::merge_options(
            &mut pending,
            IndexOptions {
                scan: ScanRulesUpdate {
                    globs: Some(Vec::new()),
                    max_depth: Some(None),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let update = pending.expect("pending update");
        assert!(update.reset_paths);
        assert_eq!(update.scan.globs, Some(Vec::new()));
        assert_eq!(update.scan.follow_symlinks, Some(false));
        assert_eq!(update.scan.max_depth, Some(None));
    }

    #[tokio::test]
    async fn explicit_names_reach_the_executor_after_queued_or_running_manual_jobs() {
        for (queued, narrow) in [(false, false), (false, true), (true, false), (true, true)] {
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
            if queued {
                scheduler
                    .submit(
                        PathBuf::from("/blocker"),
                        IndexOptions::default(),
                        JobReason::Manual,
                    )
                    .expect("occupy the worker");
                executor.started.notified().await;
            }
            let root = PathBuf::from("/workspace");
            let first = scheduler
                .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
                .expect("initial manual index");
            if !queued {
                executor.started.notified().await;
            }
            let renamed = scheduler
                .submit(
                    root.clone(),
                    IndexOptions {
                        root: Some(root.clone()),
                        name: Some("renamed-workspace".into()),
                        changes: if narrow {
                            vec![WorkspaceChange::Upsert("file.rs".into())]
                        } else {
                            Vec::new()
                        },
                        ..IndexOptions::default()
                    },
                    JobReason::Manual,
                )
                .expect("explicit rename");
            assert_eq!(renamed.job.id == first.job.id, queued);
            let refresh = scheduler
                .submit(root, IndexOptions::default(), JobReason::Watch)
                .expect("full background refresh merges with the pending rename");
            assert_eq!(refresh.job.id, renamed.job.id);

            executor.releases.add_permits(1);
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                executor.started.notified(),
            )
            .await
            .expect("explicit rename must reach the executor");
            executor.releases.add_permits(1);
            let completed = scheduler
                .wait(renamed.job.id)
                .await
                .expect("rename completes");
            assert_eq!(completed.job.state, JobState::Succeeded);
            assert_eq!(
                executor
                    .calls
                    .lock()
                    .expect("recorded requests")
                    .last()
                    .expect("rename was executed")
                    .name
                    .as_deref(),
                Some("renamed-workspace")
            );
            scheduler.shutdown().await;
        }
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

    #[tokio::test]
    async fn request_cancellation_removes_a_queued_job_without_starting_the_executor() {
        let executor = Arc::new(CancellationAwareExecutor {
            started: Notify::new(),
        });
        let scheduler = IndexJobScheduler::new(
            executor.clone(),
            SchedulerConfig {
                concurrency: 1,
                queue_capacity: 4,
            },
        );
        let first_root = std::env::temp_dir().join("mcp-running");
        scheduler
            .submit(
                first_root.clone(),
                IndexOptions::default(),
                JobReason::Manual,
            )
            .expect("first job");
        executor.started.notified().await;
        let signal = tokio_util::sync::CancellationToken::new();
        let queued = scheduler
            .submit(
                std::env::temp_dir().join("mcp-queued"),
                IndexOptions {
                    signal: Some(signal.clone()),
                    ..Default::default()
                },
                JobReason::Manual,
            )
            .expect("queued job");
        signal.cancel();
        let completed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            scheduler.wait(queued.job.id),
        )
        .await
        .expect("queued cancellation")
        .expect("completion");
        assert_eq!(completed.job.state, JobState::Cancelled);
        assert_eq!(scheduler.snapshot().running, 1);
        assert_eq!(scheduler.snapshot().queued, 0);
        scheduler.cancel_root(&first_root);
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn request_cancellation_stops_only_exclusive_jobs() {
        for reused in [false, true] {
            let executor = Arc::new(CancellationAwareExecutor {
                started: Notify::new(),
            });
            let scheduler = IndexJobScheduler::new(executor.clone(), SchedulerConfig::default());
            let root = std::env::temp_dir().join("mcp-cancellation");
            let signal = tokio_util::sync::CancellationToken::new();
            let submitted = scheduler
                .submit(
                    root.clone(),
                    IndexOptions {
                        signal: Some(signal.clone()),
                        ..Default::default()
                    },
                    JobReason::Manual,
                )
                .expect("submit");
            executor.started.notified().await;
            if reused {
                assert!(
                    scheduler
                        .submit(root.clone(), IndexOptions::default(), JobReason::Manual)
                        .expect("reuse")
                        .reused
                );
            }
            signal.cancel();
            if reused {
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(30),
                        scheduler.wait(submitted.job.id)
                    )
                    .await
                    .is_err()
                );
                scheduler.cancel_root(&root);
            }
            let completed = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                scheduler.wait(submitted.job.id),
            )
            .await
            .expect("must stop")
            .expect("completion");
            assert_eq!(completed.job.state, JobState::Cancelled);
            scheduler.shutdown().await;
        }
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

    #[test]
    fn persisted_error_redaction_matches_node_credential_forms() {
        let secret = "https://user:pass@example.test Basic basic-secret password='password secret' access-token=access-secret refresh_token=refresh-secret id token=id-secret secret=plain-secret sk-12345678";
        let redacted = redact_job_error_text(secret);

        for credential in [
            "user:pass",
            "basic-secret",
            "password secret",
            "access-secret",
            "refresh-secret",
            "id-secret",
            "plain-secret",
            "sk-12345678",
        ] {
            assert!(!redacted.contains(credential), "leaked {credential}");
        }
        assert_eq!(redacted.matches("[redacted]").count(), 8);
    }

    #[test]
    fn assigned_credentials_include_punctuation_until_whitespace_or_ampersand() {
        assert_eq!(
            redact_job_error_text(
                "password=abc,def;ghi status=401 api_key=jkl,mno;pqr&other=retained"
            ),
            "password=[redacted] status=401 api_key=[redacted]&other=retained"
        );
    }

    #[test]
    fn authorization_assignment_redacts_the_complete_line() {
        assert_eq!(
            redact_job_error_text("authorization: custom value with spaces\nvisible"),
            "authorization: [redacted]\nvisible"
        );
        assert_eq!(
            redact_job_error_text(r#"{"authorization": "Basic quoted secret", "safe": true}"#),
            r#"{"authorization": [redacted], "safe": true}"#
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
