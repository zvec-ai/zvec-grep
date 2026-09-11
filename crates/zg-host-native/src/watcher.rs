use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use notify::{
    Config as NotifyConfig, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode,
    Watcher, event::RemoveKind,
};
use tokio::{
    sync::{Mutex, Notify, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    HostError,
    api::{
        TaskControl, WatchRequest, WorkspaceChange, WorkspaceChangeBatch,
        WorkspaceWatchSessionPort, WorkspaceWatcherFactoryPort,
    },
    change_set::ChangeSet,
    pattern::normalize_relative_path,
    policy::{FileTypeResolver, PathInterest, RootPolicy},
};

const DEFAULT_RAW_EVENT_CAPACITY: usize = 4_096;
const DEFAULT_BATCH_CAPACITY: usize = 16;

#[derive(Clone, Debug)]
pub struct NativeWatcherConfig {
    pub debounce: Duration,
    pub max_wait: Duration,
    pub reconcile_interval: Option<Duration>,
    pub resume_check_interval: Option<Duration>,
    pub resume_threshold: Duration,
    pub max_changed_paths: usize,
    pub raw_event_capacity: usize,
    pub batch_capacity: usize,
    /// Uses notify's polling backend when set; `None` selects the native backend.
    pub poll_interval: Option<Duration>,
}

impl Default for NativeWatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(750),
            max_wait: Duration::from_secs(5),
            reconcile_interval: Some(Duration::from_hours(1)),
            resume_check_interval: Some(Duration::from_secs(30)),
            resume_threshold: Duration::from_secs(90),
            max_changed_paths: 1_000,
            raw_event_capacity: DEFAULT_RAW_EVENT_CAPACITY,
            batch_capacity: DEFAULT_BATCH_CAPACITY,
            poll_interval: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NativeWatcherFactory {
    resolver: FileTypeResolver,
    config: NativeWatcherConfig,
}

impl NativeWatcherFactory {
    #[must_use]
    pub fn new() -> Self {
        Self {
            resolver: FileTypeResolver::new(),
            config: NativeWatcherConfig::default(),
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: NativeWatcherConfig) -> Self {
        self.config = config;
        self
    }
}

impl Default for NativeWatcherFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkspaceWatcherFactoryPort for NativeWatcherFactory {
    async fn watch(
        &self,
        request: &WatchRequest,
        control: &TaskControl,
    ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError> {
        check_control(control)?;
        let root = request.root.clone();
        let resolver = self.resolver.clone();
        let policy_task = tokio::task::spawn_blocking(move || RootPolicy::new(root, &resolver));
        let policy = tokio::select! {
            () = control.cancellation.cancelled() => {
                return Err(HostError::cancelled("workspace watcher initialization was cancelled"));
            },
            () = deadline_wait(control.deadline) => {
                return Err(HostError::deadline_exceeded(
                    "workspace watcher initialization exceeded its deadline",
                ));
            },
            result = policy_task => result
                .map_err(|error| HostError::internal(format!("watch policy worker failed: {error}")))??,
        };
        let metadata = std::fs::metadata(policy.root_path()).map_err(|error| {
            HostError::invalid_argument(format!(
                "watch root {} could not be inspected: {error}",
                policy.root_path().display()
            ))
        })?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(HostError::invalid_argument(format!(
                "watch root {} must be a file or directory",
                policy.root_path().display()
            )));
        }

        let config = normalize_config(self.config.clone());
        let (raw_sender, raw_receiver) = mpsc::channel(config.raw_event_capacity);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_notify = Arc::new(Notify::new());
        let watcher = create_watcher(
            policy.root_path(),
            policy.root().recursive,
            &raw_sender,
            &overflowed,
            &overflow_notify,
            config.poll_interval,
        )?;
        let (batch_sender, batch_receiver) = mpsc::channel(config.batch_capacity);
        let close = CancellationToken::new();
        let (flush_sender, flush_receiver) = mpsc::channel(1);
        let task = tokio::spawn(watch_loop(WatchLoop {
            flush_receiver,
            policy,
            root_is_file: metadata.is_file(),
            config,
            watcher: Some(watcher),
            raw_sender,
            raw_receiver,
            overflowed,
            overflow_notify,
            batch_sender,
            close: close.clone(),
        }));
        Ok(Arc::new(NativeWatchSession {
            inner: Arc::new(WatchSessionInner {
                receiver: Mutex::new(batch_receiver),
                flush_sender,
                close,
                task: StdMutex::new(Some(task)),
            }),
        }))
    }
}

#[derive(Debug)]
struct NativeWatchSession {
    inner: Arc<WatchSessionInner>,
}

#[derive(Debug)]
struct WatchSessionInner {
    flush_sender: mpsc::Sender<oneshot::Sender<()>>,
    receiver: Mutex<mpsc::Receiver<WorkspaceChangeBatch>>,
    close: CancellationToken,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for WatchSessionInner {
    fn drop(&mut self) {
        self.close.cancel();
        if let Ok(mut task) = self.task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

#[async_trait]
impl WorkspaceWatchSessionPort for NativeWatchSession {
    async fn flush(&self) -> Result<(), HostError> {
        let (sender, receiver) = oneshot::channel();
        self.inner
            .flush_sender
            .send(sender)
            .await
            .map_err(|_| HostError::resource_closed("workspace watcher has stopped"))?;
        receiver
            .await
            .map_err(|_| HostError::resource_closed("workspace watcher flush was interrupted"))
    }

    async fn next_changes(&self, control: &TaskControl) -> Result<WorkspaceChangeBatch, HostError> {
        check_control(control)?;
        if self.inner.close.is_cancelled() {
            return Err(HostError::resource_closed(
                "workspace watch session has been closed",
            ));
        }
        let mut receiver = self.inner.receiver.lock().await;
        tokio::select! {
            () = self.inner.close.cancelled() => Err(HostError::resource_closed(
                "workspace watch session has been closed",
            )),
            () = control.cancellation.cancelled() => Err(HostError::cancelled(
                "waiting for workspace changes was cancelled",
            )),
            () = deadline_wait(control.deadline) => Err(HostError::deadline_exceeded(
                "waiting for workspace changes exceeded its deadline",
            )),
            batch = receiver.recv() => batch.ok_or_else(|| HostError::resource_closed(
                "workspace watch session is no longer available",
            )),
        }
    }

    async fn close(&self) -> Result<(), HostError> {
        self.inner.close.cancel();
        let task = lock_task(&self.inner.task).take();
        if let Some(task) = task {
            task.await.map_err(|error| {
                HostError::internal(format!("native watcher task failed: {error}"))
            })?;
        }
        Ok(())
    }
}

struct WatchLoop {
    flush_receiver: mpsc::Receiver<oneshot::Sender<()>>,
    policy: RootPolicy,
    root_is_file: bool,
    config: NativeWatcherConfig,
    watcher: Option<NativeWatcher>,
    raw_sender: mpsc::Sender<notify::Result<Event>>,
    raw_receiver: mpsc::Receiver<notify::Result<Event>>,
    overflowed: Arc<AtomicBool>,
    overflow_notify: Arc<Notify>,
    batch_sender: mpsc::Sender<WorkspaceChangeBatch>,
    close: CancellationToken,
}

#[allow(clippy::too_many_lines)]
async fn watch_loop(mut state: WatchLoop) {
    let mut changes = ChangeSet::new(state.config.max_changed_paths);
    let mut debounce_deadline = None;
    let mut max_wait_deadline = None;
    let mut reconcile_deadline = state
        .config
        .reconcile_interval
        .map(|interval| Instant::now() + interval);
    let mut resume_deadline = state
        .config
        .resume_check_interval
        .map(|interval| Instant::now() + interval);
    let mut last_resume_check = Instant::now();
    let mut retry_deadline = None;
    let mut stable_deadline = Some(Instant::now() + Duration::from_secs(1));
    let mut consecutive_errors = 0_u32;
    let mut recovery_reconcile_pending = false;

    loop {
        tokio::select! {
            () = state.close.cancelled() => break,
            Some(acknowledge) = state.flush_receiver.recv() => {
                // Reconciliation covers native events still pending delivery or normalization.
                changes.require_full_rescan();
                if !flush_changes(&mut changes, &state.batch_sender, &state.close).await {
                    break;
                }
                debounce_deadline = None;
                max_wait_deadline = None;
                let _ = acknowledge.send(());
            }
            () = sleep_until_option(debounce_deadline) => {
                if !flush_changes(&mut changes, &state.batch_sender, &state.close).await {
                    break;
                }
                debounce_deadline = None;
                max_wait_deadline = None;
            }
            () = sleep_until_option(max_wait_deadline) => {
                if !flush_changes(&mut changes, &state.batch_sender, &state.close).await {
                    break;
                }
                debounce_deadline = None;
                max_wait_deadline = None;
            }
            () = sleep_until_option(reconcile_deadline) => {
                changes.require_full_rescan();
                schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                reconcile_deadline = state.config.reconcile_interval.map(|interval| Instant::now() + interval);
            }
            () = sleep_until_option(resume_deadline) => {
                let now = Instant::now();
                if now.duration_since(last_resume_check) > state.config.resume_threshold {
                    changes.require_full_rescan();
                    schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                }
                last_resume_check = now;
                resume_deadline = state.config.resume_check_interval.map(|interval| now + interval);
            }
            () = sleep_until_option(stable_deadline) => {
                consecutive_errors = 0;
                recovery_reconcile_pending = false;
                stable_deadline = None;
            }
            () = sleep_until_option(retry_deadline) => {
                match create_watcher(
                    state.policy.root_path(),
                    state.policy.root().recursive,
                    &state.raw_sender,
                    &state.overflowed,
                    &state.overflow_notify,
                    state.config.poll_interval,
                ) {
                    Ok(watcher) => {
                        state.watcher = Some(watcher);
                        retry_deadline = None;
                        stable_deadline = Some(Instant::now() + Duration::from_secs(1));
                    }
                    Err(error) => {
                        warn!(%error, "failed to recover native watcher");
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        retry_deadline = Some(Instant::now() + retry_delay(consecutive_errors));
                    }
                }
            }
            () = state.overflow_notify.notified() => {
                if state.overflowed.swap(false, Ordering::AcqRel) {
                    changes.require_full_rescan();
                    schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                }
            }
            raw = state.raw_receiver.recv() => {
                let Some(raw) = raw else { break; };
                match raw {
                    Ok(event) => {
                        let policy = state.policy.clone();
                        let root_is_file = state.root_is_file;
                        match tokio::task::spawn_blocking(move || normalize_event(&policy, root_is_file, &event)).await {
                            Ok(event_changes) => {
                                for change in event_changes {
                                    changes.add(change);
                                }
                                if !changes.is_empty() {
                                    schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                                }
                            }
                            Err(error) => {
                                warn!(%error, "native watcher event worker failed");
                                changes.require_full_rescan();
                                schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                            }
                        }
                    }
                    Err(error) => {
                        warn!(%error, "native watcher backend failed");
                        state.watcher.take();
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        stable_deadline = None;
                        retry_deadline = Some(Instant::now() + retry_delay(consecutive_errors));
                        if !recovery_reconcile_pending {
                            recovery_reconcile_pending = true;
                            changes.require_full_rescan();
                            schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                        }
                    }
                }
            }
        }
    }
    state.watcher.take();
}

fn create_watcher(
    root: &Path,
    recursive: bool,
    raw_sender: &mpsc::Sender<notify::Result<Event>>,
    overflowed: &Arc<AtomicBool>,
    overflow_notify: &Arc<Notify>,
    poll_interval: Option<Duration>,
) -> Result<NativeWatcher, HostError> {
    let sender = raw_sender.clone();
    let overflowed = Arc::clone(overflowed);
    let overflow_notify = Arc::clone(overflow_notify);
    let handler = move |event| {
        if sender.try_send(event).is_err() {
            overflowed.store(true, Ordering::Release);
            overflow_notify.notify_one();
        }
    };
    let recursive_mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    let watch_error = |error| {
        HostError::storage_failure(
            "native-watcher",
            format!("could not watch {}: {error}", root.display()),
        )
    };

    if let Some(interval) = poll_interval {
        let mut watcher = PollWatcher::new(
            handler,
            NotifyConfig::default()
                .with_poll_interval(interval)
                .with_compare_contents(true),
        )
        .map_err(|error| HostError::storage_failure("native-watcher", error.to_string()))?;
        watcher.watch(root, recursive_mode).map_err(watch_error)?;
        Ok(NativeWatcher::Poll { _watcher: watcher })
    } else {
        let mut watcher = notify::recommended_watcher(handler)
            .map_err(|error| HostError::storage_failure("native-watcher", error.to_string()))?;
        watcher.watch(root, recursive_mode).map_err(watch_error)?;
        Ok(NativeWatcher::Recommended { _watcher: watcher })
    }
}

enum NativeWatcher {
    Recommended { _watcher: RecommendedWatcher },
    Poll { _watcher: PollWatcher },
}

fn normalize_event(policy: &RootPolicy, root_is_file: bool, event: &Event) -> Vec<WorkspaceChange> {
    match event.kind {
        EventKind::Access(_) => Vec::new(),
        EventKind::Any | EventKind::Other => vec![WorkspaceChange::Rescan],
        EventKind::Remove(RemoveKind::File) => event
            .paths
            .iter()
            .filter_map(|path| normalize_removed_path(policy, root_is_file, path, false))
            .collect(),
        EventKind::Remove(_) => event
            .paths
            .iter()
            .filter_map(|path| normalize_removed_path(policy, root_is_file, path, true))
            .collect(),
        EventKind::Create(_) | EventKind::Modify(_) => event
            .paths
            .iter()
            .filter_map(|path| normalize_present_or_removed_path(policy, root_is_file, path))
            .collect(),
    }
}

fn normalize_present_or_removed_path(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
) -> Option<WorkspaceChange> {
    normalize_present_or_removed_path_with(policy, root_is_file, path, |path| {
        std::fs::metadata(path).ok()
    })
}

fn normalize_present_or_removed_path_with(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
    metadata: impl FnOnce(&Path) -> Option<std::fs::Metadata>,
) -> Option<WorkspaceChange> {
    if path == policy.root_path() && !root_is_file {
        return Some(WorkspaceChange::Rescan);
    }
    match normalize_gitignore_change(policy, root_is_file, path) {
        GitignoreChange::NotGitignore => {}
        GitignoreChange::Ignored => return None,
        GitignoreChange::Reconcile(change) => return Some(change),
    }
    let interest = policy.classify_path_interest(path);
    if interest.is_empty() {
        return None;
    }
    let metadata = metadata(path);
    if metadata.is_none() {
        return normalize_removed_path_with_interest(policy, root_is_file, path, true, interest);
    }
    normalize_present_path(
        policy,
        root_is_file,
        path,
        metadata.is_some_and(|value| value.is_dir()),
        interest,
    )
}

fn normalize_present_path(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
    is_directory: bool,
    interest: PathInterest,
) -> Option<WorkspaceChange> {
    let relative = relative_change_path(policy, root_is_file, path)?;
    if !policy.path_interest_can_affect_index(path, is_directory, interest) {
        return None;
    }
    Some(if is_directory {
        WorkspaceChange::RescanDirectory(relative)
    } else {
        WorkspaceChange::Upsert(relative)
    })
}

fn normalize_removed_path(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
    prefix: bool,
) -> Option<WorkspaceChange> {
    if path == policy.root_path() && !root_is_file {
        return Some(WorkspaceChange::Rescan);
    }
    match normalize_gitignore_change(policy, root_is_file, path) {
        GitignoreChange::NotGitignore => {}
        GitignoreChange::Ignored => return None,
        GitignoreChange::Reconcile(change) => return Some(change),
    }
    let interest = policy.classify_path_interest(path);
    if interest.is_empty() {
        return None;
    }
    normalize_removed_path_with_interest(policy, root_is_file, path, prefix, interest)
}

fn normalize_removed_path_with_interest(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
    prefix: bool,
    interest: PathInterest,
) -> Option<WorkspaceChange> {
    let relative = relative_change_path(policy, root_is_file, path)?;
    if !policy.path_interest_can_affect_index(path, false, interest) {
        return None;
    }
    Some(if prefix {
        WorkspaceChange::DeletePrefix(relative)
    } else {
        WorkspaceChange::Delete(relative)
    })
}

enum GitignoreChange {
    NotGitignore,
    Ignored,
    Reconcile(WorkspaceChange),
}

fn normalize_gitignore_change(
    policy: &RootPolicy,
    root_is_file: bool,
    path: &Path,
) -> GitignoreChange {
    if path.file_name().is_none_or(|name| name != ".gitignore") {
        return GitignoreChange::NotGitignore;
    }
    if root_is_file && path == policy.root_path() {
        return GitignoreChange::Reconcile(WorkspaceChange::RescanDirectory(PathBuf::new()));
    }
    let Some(parent) = path.parent() else {
        return GitignoreChange::Ignored;
    };
    if !policy.path_can_affect_index(parent, true) {
        return GitignoreChange::Ignored;
    }
    policy.invalidate_gitignore_rules(parent);
    relative_change_path(policy, root_is_file, path).map_or(GitignoreChange::Ignored, |relative| {
        GitignoreChange::Reconcile(WorkspaceChange::RescanDirectory(parent_scope(&relative)))
    })
}

fn relative_change_path(policy: &RootPolicy, root_is_file: bool, path: &Path) -> Option<PathBuf> {
    if root_is_file && path == policy.root_path() {
        return policy.root_path().file_name().map(PathBuf::from);
    }
    path.strip_prefix(policy.root_path())
        .ok()
        .and_then(|relative| {
            (!relative.as_os_str().is_empty())
                .then(|| PathBuf::from(normalize_relative_path(relative)))
        })
}

fn parent_scope(path: &Path) -> PathBuf {
    path.parent().unwrap_or_else(|| Path::new("")).to_path_buf()
}

fn schedule_flush(
    config: &NativeWatcherConfig,
    debounce_deadline: &mut Option<Instant>,
    max_wait_deadline: &mut Option<Instant>,
) {
    let now = Instant::now();
    *debounce_deadline = Some(now + config.debounce);
    max_wait_deadline.get_or_insert(now + config.max_wait);
}

async fn flush_changes(
    changes: &mut ChangeSet,
    sender: &mpsc::Sender<WorkspaceChangeBatch>,
    close: &CancellationToken,
) -> bool {
    if changes.is_empty() {
        return true;
    }
    let batch = changes.take_batch();
    tokio::select! {
        () = close.cancelled() => false,
        result = sender.send(batch) => result.is_ok(),
    }
}

async fn sleep_until_option(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn deadline_wait(deadline: Option<std::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(Instant::from_std(deadline)).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn retry_delay(consecutive_errors: u32) -> Duration {
    let exponent = consecutive_errors.saturating_sub(1).min(6);
    Duration::from_millis((100_u64.saturating_mul(2_u64.pow(exponent))).min(5_000))
}

fn normalize_config(mut config: NativeWatcherConfig) -> NativeWatcherConfig {
    config.max_changed_paths = config.max_changed_paths.max(1);
    config.raw_event_capacity = config.raw_event_capacity.max(1);
    config.batch_capacity = config.batch_capacity.max(1);
    config
}

fn check_control(control: &TaskControl) -> Result<(), HostError> {
    if control.cancellation.is_cancelled() {
        return Err(HostError::cancelled(
            "native watcher operation was cancelled",
        ));
    }
    if control
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        return Err(HostError::deadline_exceeded(
            "native watcher operation exceeded its deadline",
        ));
    }
    Ok(())
}

fn lock_task(
    mutex: &StdMutex<Option<JoinHandle<()>>>,
) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tempfile::tempdir;

    use crate::{DiscoveryOptions, RootSpec};

    use super::{FileTypeResolver, RootPolicy, normalize_present_or_removed_path_with};

    #[test]
    fn definitely_ignored_watcher_paths_skip_metadata() {
        let root = tempdir().expect("watch root");
        let ignored = root.path().join("node_modules/pkg/index.js");
        let policy = RootPolicy::new(
            RootSpec {
                path: root.path().to_path_buf(),
                recursive: true,
                discovery: DiscoveryOptions::default(),
            },
            &FileTypeResolver::new(),
        )
        .expect("watch policy");
        let metadata_calls = AtomicUsize::new(0);

        let change = normalize_present_or_removed_path_with(&policy, false, &ignored, |_| {
            metadata_calls.fetch_add(1, Ordering::AcqRel);
            None
        });

        assert!(change.is_none());
        assert_eq!(metadata_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn ignored_gitignore_changes_do_not_schedule_reconcile_or_read_metadata() {
        let root = tempdir().expect("watch root");
        let ignored = root.path().join("node_modules/pkg/.gitignore");
        let policy = RootPolicy::new(
            RootSpec {
                path: root.path().to_path_buf(),
                recursive: true,
                discovery: DiscoveryOptions::default(),
            },
            &FileTypeResolver::new(),
        )
        .expect("watch policy");
        let metadata_calls = AtomicUsize::new(0);

        let change = normalize_present_or_removed_path_with(&policy, false, &ignored, |_| {
            metadata_calls.fetch_add(1, Ordering::AcqRel);
            None
        });

        assert!(change.is_none());
        assert_eq!(metadata_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn explicit_includes_keep_ignored_directory_files_watchable() {
        let root = tempdir().expect("watch root");
        let included = root.path().join("node_modules/pkg/index.js");
        fs::create_dir_all(included.parent().expect("included parent")).expect("included parent");
        fs::write(&included, "module.exports = 1;\n").expect("included fixture");
        let policy = RootPolicy::new(
            RootSpec {
                path: root.path().to_path_buf(),
                recursive: true,
                discovery: DiscoveryOptions {
                    include_paths: vec!["node_modules/pkg/index.js".to_owned()],
                    ..DiscoveryOptions::default()
                },
            },
            &FileTypeResolver::new(),
        )
        .expect("watch policy");
        let metadata_calls = AtomicUsize::new(0);

        let change = normalize_present_or_removed_path_with(&policy, false, &included, |path| {
            metadata_calls.fetch_add(1, Ordering::AcqRel);
            fs::metadata(path).ok()
        });

        assert_eq!(
            change,
            Some(crate::WorkspaceChange::Upsert(
                "node_modules/pkg/index.js".into()
            ))
        );
        assert_eq!(metadata_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn root_gitignore_changes_reconcile_without_reading_target_metadata() {
        let root = tempdir().expect("watch root");
        let gitignore = root.path().join(".gitignore");
        let policy = RootPolicy::new(
            RootSpec {
                path: root.path().to_path_buf(),
                recursive: true,
                discovery: DiscoveryOptions::default(),
            },
            &FileTypeResolver::new(),
        )
        .expect("watch policy");
        let metadata_calls = AtomicUsize::new(0);

        let change = normalize_present_or_removed_path_with(&policy, false, &gitignore, |_| {
            metadata_calls.fetch_add(1, Ordering::AcqRel);
            None
        });

        assert_eq!(
            change,
            Some(crate::WorkspaceChange::RescanDirectory(PathBuf::new()))
        );
        assert_eq!(metadata_calls.load(Ordering::Acquire), 0);
    }
}
