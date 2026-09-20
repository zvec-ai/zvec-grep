use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
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
    Watcher,
    event::{ModifyKind, RemoveKind, RenameMode},
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
        RootSpec, TaskControl, WatchRequest, WorkspaceChange, WorkspaceChangeBatch,
        WorkspaceWatchSessionPort, WorkspaceWatcherFactoryPort,
    },
    change_set::ChangeSet,
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
    config: NativeWatcherConfig,
}

impl NativeWatcherFactory {
    #[must_use]
    pub fn new() -> Self {
        Self {
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
        if !root.path.is_absolute() {
            return Err(HostError::invalid_argument(format!(
                "watch root must be absolute: {}",
                root.path.display()
            )));
        }
        let metadata = fs::metadata(&root.path).map_err(|error| {
            HostError::invalid_argument(format!(
                "watch root {} could not be inspected: {error}",
                root.path.display()
            ))
        })?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(HostError::invalid_argument(format!(
                "watch root {} must be a file or directory",
                root.path.display()
            )));
        }

        let config = normalize_config(self.config.clone());
        let (raw_sender, raw_receiver) = mpsc::channel(config.raw_event_capacity);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_notify = Arc::new(Notify::new());
        let watcher_root = root.clone();
        let watcher_sender = raw_sender.clone();
        let watcher_overflow = Arc::clone(&overflowed);
        let watcher_notify = Arc::clone(&overflow_notify);
        let root_is_file = metadata.is_file();
        let poll_interval = config.poll_interval;
        let initialization = tokio::task::spawn_blocking(move || {
            create_watcher(
                &watcher_root,
                root_is_file,
                &watcher_sender,
                &watcher_overflow,
                &watcher_notify,
                poll_interval,
            )
        });
        let watcher = tokio::select! {
            () = control.cancellation.cancelled() => return Err(HostError::cancelled("watcher initialization was cancelled")),
            () = deadline_wait(control.deadline) => return Err(HostError::deadline_exceeded("watcher initialization exceeded its deadline")),
            result = initialization => result.map_err(watcher_error)??,
        };
        let (batch_sender, batch_receiver) = mpsc::channel(config.batch_capacity);
        let close = CancellationToken::new();
        let (flush_sender, flush_receiver) = mpsc::channel(1);
        let task = tokio::spawn(watch_loop(WatchLoop {
            flush_receiver,
            root,
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
    root: RootSpec,
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
        let mut refresh_registration = false;
        tokio::select! {
            () = state.close.cancelled() => break,
            Some(acknowledge) = state.flush_receiver.recv() => {
                // Reconciliation covers native events still pending delivery or normalization.
                refresh_registration = true;
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
                refresh_registration = true;
                changes.require_full_rescan();
                schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                reconcile_deadline = state.config.reconcile_interval.map(|interval| Instant::now() + interval);
            }
            () = sleep_until_option(resume_deadline) => {
                let now = Instant::now();
                if now.duration_since(last_resume_check) > state.config.resume_threshold {
                    refresh_registration = true;
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
                let root = state.root.clone();
                let root_is_file = state.root_is_file;
                let sender = state.raw_sender.clone();
                let overflowed = Arc::clone(&state.overflowed);
                let notify = Arc::clone(&state.overflow_notify);
                let interval = state.config.poll_interval;
                let recovered = tokio::task::spawn_blocking(move || create_watcher(
                    &root, root_is_file, &sender, &overflowed, &notify, interval,
                )).await.map_err(watcher_error).and_then(|result| result);
                match recovered {
                    Ok(watcher) => {
                        state.watcher = Some(watcher);
                        retry_deadline = None;
                        stable_deadline = Some(Instant::now() + Duration::from_secs(1));
                        // Reconcile changes missed between backend failure and recovery.
                        changes.require_full_rescan();
                        schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
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
                    refresh_registration = true;
                    changes.require_full_rescan();
                    schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
                }
            }
            raw = state.raw_receiver.recv() => {
                let Some(raw) = raw else { break; };
                match raw {
                    Ok(event) => {
                        let root = state.root.clone();
                        let root_is_file = state.root_is_file;
                        let watcher = state.watcher.take();
                        let result = tokio::task::spawn_blocking(move || {
                            let mut watcher = watcher;
                            let events = watcher.as_ref().map_or_else(|| vec![event.clone()], |watcher| watcher.aliases.translated_events(&event));
                            let result = events.iter().try_fold(NormalizedEvent { changes: Vec::new(), refresh_registration: false }, |mut combined, translated| {
                                let normalized = normalize_event(&root, root_is_file, translated)?;
                                combined.changes.extend(normalized.changes);
                                combined.refresh_registration |= normalized.refresh_registration;
                                Ok::<_, HostError>(combined)
                            });
                            let result = result.and_then(|normalized| {
                                if let Some(watcher) = &mut watcher
                                    && (normalized.refresh_registration || watcher.registered_path_changed(&event)) {
                                    root.policy.invalidate()?;
                                    watcher.refresh(&root, root_is_file)?;
                                }
                                Ok(normalized.changes)
                            });
                            (watcher, result)
                        }).await;
                        match result {
                            Ok((watcher, Ok(event_changes))) => {
                                state.watcher = watcher;
                                for change in event_changes {
                                    changes.add(change);
                                }
                            }
                            Ok((watcher, Err(error))) => {
                                warn!(%error, "native watcher event policy failed");
                                state.watcher = watcher;
                                retry_deadline = Some(Instant::now() + retry_delay(consecutive_errors));
                                changes.require_full_rescan();
                            }
                            Err(error) => {
                                warn!(%error, "native watcher event worker failed");
                                retry_deadline = Some(Instant::now() + retry_delay(consecutive_errors));
                                changes.require_full_rescan();
                            }
                        }
                        if !changes.is_empty() {
                            schedule_flush(&state.config, &mut debounce_deadline, &mut max_wait_deadline);
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
        if refresh_registration && let Err(error) = refresh_watcher(&mut state).await {
            warn!(%error, "could not reconcile watcher registrations");
            consecutive_errors = consecutive_errors.saturating_add(1);
            retry_deadline = Some(Instant::now() + retry_delay(consecutive_errors));
        }
    }
    state.watcher.take();
}

async fn refresh_watcher(state: &mut WatchLoop) -> Result<(), HostError> {
    let Some(mut watcher) = state.watcher.take() else {
        return Ok(());
    };
    let root = state.root.clone();
    let root_is_file = state.root_is_file;
    let (watcher, result) = tokio::task::spawn_blocking(move || {
        let result = root
            .policy
            .invalidate()
            .and_then(|()| watcher.refresh(&root, root_is_file));
        (watcher, result)
    })
    .await
    .map_err(watcher_error)?;
    state.watcher = Some(watcher);
    result
}

fn create_watcher(
    root: &RootSpec,
    root_is_file: bool,
    raw_sender: &mpsc::Sender<notify::Result<Event>>,
    overflowed: &Arc<AtomicBool>,
    overflow_notify: &Arc<Notify>,
    poll_interval: Option<Duration>,
) -> Result<NativeWatcher, HostError> {
    root.policy.invalidate()?;
    let sender = raw_sender.clone();
    let overflowed = Arc::clone(overflowed);
    let overflow_notify = Arc::clone(overflow_notify);
    let handler = move |event| {
        if sender.try_send(event).is_err() {
            overflowed.store(true, Ordering::Release);
            overflow_notify.notify_one();
        }
    };
    let backend = if let Some(interval) = poll_interval {
        WatchBackend::Poll(
            PollWatcher::new(
                handler,
                NotifyConfig::default()
                    .with_poll_interval(interval)
                    .with_compare_contents(true)
                    .with_follow_symlinks(root.follow),
            )
            .map_err(watcher_error)?,
        )
    } else {
        WatchBackend::Recommended(
            RecommendedWatcher::new(
                handler,
                NotifyConfig::default().with_follow_symlinks(root.follow),
            )
            .map_err(watcher_error)?,
        )
    };
    let mut watcher = NativeWatcher {
        backend,
        registrations: BTreeMap::new(),
        aliases: WatchAliases::default(),
    };
    watcher.refresh(root, root_is_file)?;
    Ok(watcher)
}

fn watcher_error(error: impl std::fmt::Display) -> HostError {
    HostError::storage_failure("native-watcher", error.to_string())
}

struct NativeWatcher {
    backend: WatchBackend,
    registrations: BTreeMap<PathBuf, DirectoryIdentity>,
    aliases: WatchAliases,
}

#[derive(Default)]
struct WatchAliases {
    directories: BTreeMap<PathBuf, PathBuf>,
    files: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl WatchAliases {
    fn translated_events(&self, event: &Event) -> Vec<Event> {
        if event.paths.is_empty() {
            return vec![event.clone()];
        }
        event
            .paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let mut translated = event.clone();
                if matches!(
                    event.kind,
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both))
                ) {
                    translated.kind = EventKind::Modify(ModifyKind::Name(if index == 0 {
                        RenameMode::From
                    } else {
                        RenameMode::To
                    }));
                }
                let mut paths = BTreeSet::new();
                let directory_alias = self
                    .directories
                    .iter()
                    .filter(|(target, _)| path.starts_with(target))
                    .max_by_key(|(target, _)| target.components().count());
                if let Some((target, alias)) = directory_alias {
                    paths.insert(
                        alias.join(path.strip_prefix(target).expect("matched directory prefix")),
                    );
                } else {
                    paths.insert(path.clone());
                }
                if let Some(aliases) = self.files.get(path) {
                    paths.extend(aliases.iter().cloned());
                }
                if is_topology_event(event.kind) {
                    // A missing target's ancestors are watched until its parent reappears.
                    for (target, aliases) in &self.files {
                        if target.starts_with(path) {
                            paths.extend(aliases.iter().cloned());
                        }
                    }
                    for (target, alias) in &self.directories {
                        if target.starts_with(path) {
                            paths.insert(alias.clone());
                        }
                    }
                }
                translated.paths = paths.into_iter().collect();
                translated
            })
            .collect()
    }
}

#[derive(Default)]
struct WatchPlan {
    directories: BTreeSet<PathBuf>,
    aliases: WatchAliases,
}

/// Stable directory identity without retaining an extra OS file handle per watch.
#[derive(Eq, PartialEq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

impl DirectoryIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                created: metadata.created().ok(),
            }
        }
    }
}

enum WatchBackend {
    Recommended(RecommendedWatcher),
    Poll(PollWatcher),
}

impl NativeWatcher {
    fn registered_path_changed(&self, event: &Event) -> bool {
        is_topology_event(event.kind)
            && event.paths.iter().any(|path| {
                self.registrations.contains_key(path)
                    || self
                        .aliases
                        .directories
                        .iter()
                        .any(|(target, alias)| target.starts_with(path) || alias == path)
                    || self
                        .aliases
                        .files
                        .iter()
                        .any(|(target, aliases)| target.starts_with(path) || aliases.contains(path))
            })
    }

    fn refresh(&mut self, root: &RootSpec, root_is_file: bool) -> Result<(), HostError> {
        let desired = watch_directories(root, root_is_file, &mut |path| self.register(path))?;
        let stale: Vec<_> = self
            .registrations
            .keys()
            .filter(|path| !desired.directories.contains(*path))
            .cloned()
            .collect();
        for path in stale {
            self.unregister(&path);
        }
        self.aliases = desired.aliases;
        Ok(())
    }

    fn unregister(&mut self, path: &Path) {
        // Removed directories can have already been dropped by the backend.
        let _ = match &mut self.backend {
            WatchBackend::Recommended(watcher) => watcher.unwatch(path),
            WatchBackend::Poll(watcher) => watcher.unwatch(path),
        };
        self.registrations.remove(path);
    }

    fn register(&mut self, path: &Path) -> Result<(), HostError> {
        let identity = match fs::metadata(path) {
            Ok(metadata) => DirectoryIdentity::from_metadata(&metadata),
            Err(_) if !path.exists() => return Ok(()),
            Err(error) => return Err(watcher_error(error)),
        };
        if self.registrations.get(path) == Some(&identity) {
            return Ok(());
        }
        if self.registrations.contains_key(path) {
            self.unregister(path);
        }
        let result = match &mut self.backend {
            WatchBackend::Recommended(watcher) => watcher.watch(path, RecursiveMode::NonRecursive),
            WatchBackend::Poll(watcher) => watcher.watch(path, RecursiveMode::NonRecursive),
        };
        match result {
            Ok(()) => {
                self.registrations.insert(path.to_path_buf(), identity);
                Ok(())
            }
            Err(_) if !path.exists() => Ok(()),
            Err(error) => Err(watcher_error(format!(
                "could not watch {}: {error}",
                path.display()
            ))),
        }
    }
}

/// Discover only traversable directories before asking the OS to allocate watches.
fn watch_directories(
    root: &RootSpec,
    root_is_file: bool,
    register: &mut impl FnMut(&Path) -> Result<(), HostError>,
) -> Result<WatchPlan, HostError> {
    let mut plan = WatchPlan::default();
    // Watching the parent preserves root replacement and recreation detection.
    add_existing_parent(&root.path, &mut plan.directories);
    for control_path in root.policy.control_paths() {
        add_existing_parent(&control_path, &mut plan.directories);
    }
    for path in &plan.directories {
        register(path)?;
    }
    if root_is_file {
        if fs::symlink_metadata(&root.path).is_ok_and(|metadata| metadata.is_symlink()) {
            add_file_link(root, &root.path, &mut plan, register)?;
        }
    } else {
        let mut visited = BTreeSet::new();
        collect_watch_directories(root, &root.path, 0, &mut visited, &mut plan, register)?;
    }
    // Traversal may discover additional rule files (including symlink targets).
    for control in root.policy.control_paths() {
        let mut parents = BTreeSet::new();
        add_existing_parent(&control, &mut parents);
        for parent in parents {
            if plan.directories.insert(parent.clone()) {
                register(&parent)?;
            }
        }
    }
    Ok(plan)
}

fn add_existing_parent(path: &Path, directories: &mut BTreeSet<PathBuf>) {
    let mut parent = path.parent();
    while let Some(path) = parent {
        if path.is_dir() {
            directories.insert(path.to_path_buf());
            break;
        }
        parent = path.parent();
    }
}

fn collect_watch_directories(
    root: &RootSpec,
    path: &Path,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    plan: &mut WatchPlan,
    register: &mut impl FnMut(&Path) -> Result<(), HostError>,
) -> Result<(), HostError> {
    if !path.is_dir() || !root.policy.can_descend(path)? {
        return Ok(());
    }
    let Ok(canonical) = fs::canonicalize(path) else {
        return Ok(());
    };
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    if canonical != path {
        plan.aliases
            .directories
            .insert(canonical, path.to_path_buf());
    }
    // Register before enumerating children so concurrent directory creation is queued.
    if plan.directories.insert(path.to_path_buf()) {
        register(path)?;
    }
    if root.max_depth.is_some_and(|maximum| depth + 1 > maximum) {
        return Ok(());
    }
    let descend = root.recursive && root.max_depth.is_none_or(|maximum| depth + 1 < maximum);
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && descend {
            collect_watch_directories(root, &entry.path(), depth + 1, visited, plan, register)?;
        } else if root.follow && file_type.is_symlink() {
            if entry.path().is_dir() {
                if descend {
                    collect_watch_directories(
                        root,
                        &entry.path(),
                        depth + 1,
                        visited,
                        plan,
                        register,
                    )?;
                }
            } else {
                add_file_link(root, &entry.path(), plan, register)?;
            }
        }
    }
    Ok(())
}

fn add_file_link(
    root: &RootSpec,
    alias: &Path,
    plan: &mut WatchPlan,
    register: &mut impl FnMut(&Path) -> Result<(), HostError>,
) -> Result<(), HostError> {
    if !root.policy.includes_file(alias)? && !root.policy.can_descend(alias)? {
        return Ok(());
    }
    let Ok(link) = fs::read_link(alias) else {
        return Ok(());
    };
    let target = alias.parent().unwrap_or(&root.path).join(link);
    // Resolve as much as exists, preserving missing target components for recreation.
    let Some(target) = resolve_missing_path(&target) else {
        return Ok(());
    };
    plan.aliases
        .files
        .entry(target.clone())
        .or_default()
        .insert(alias.to_path_buf());
    let mut parents = BTreeSet::new();
    add_existing_parent(&target, &mut parents);
    for parent in parents {
        if plan.directories.insert(parent.clone()) {
            register(&parent)?;
        }
    }
    Ok(())
}

fn resolve_missing_path(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = fs::canonicalize(ancestor) {
            return Some(canonical.join(path.strip_prefix(ancestor).ok()?));
        }
    }
    None
}

fn is_topology_event(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

#[derive(Default)]
struct NormalizedEvent {
    changes: Vec<WorkspaceChange>,
    refresh_registration: bool,
}

fn normalize_event(
    root: &RootSpec,
    root_is_file: bool,
    event: &Event,
) -> Result<NormalizedEvent, HostError> {
    let mut normalized = NormalizedEvent::default();
    if matches!(event.kind, EventKind::Access(_)) {
        return Ok(normalized);
    }
    if event.need_rescan() || matches!(event.kind, EventKind::Any | EventKind::Other) {
        root.policy.invalidate()?;
        normalized.changes.push(WorkspaceChange::Rescan);
        normalized.refresh_registration = true;
        return Ok(normalized);
    }
    let topology_event = is_topology_event(event.kind);
    let control_paths = root.policy.control_paths();
    for (index, path) in event.paths.iter().enumerate() {
        if let Some(change) = normalize_policy_change(root, path, topology_event, &control_paths)? {
            normalized.changes.push(change);
            normalized.refresh_registration = true;
            continue;
        }
        if path == &root.path && !root_is_file {
            normalized.changes.push(WorkspaceChange::Rescan);
            normalized.refresh_registration |= topology_event;
            continue;
        }
        let Some(relative) = relative_change_path(root, root_is_file, path) else {
            continue;
        };
        // Rename sources and deletions must clear formerly indexed entries even if
        // current rules reject the path, or it cannot be inspected any more.
        if is_removed_path(event.kind, index) {
            normalized.changes.push(
                if matches!(event.kind, EventKind::Remove(RemoveKind::File)) {
                    WorkspaceChange::Delete(relative)
                } else {
                    WorkspaceChange::DeletePrefix(relative)
                },
            );
            continue;
        }
        let metadata = fs::symlink_metadata(path).ok();
        let Some(metadata) = metadata else {
            normalized
                .changes
                .push(WorkspaceChange::DeletePrefix(relative));
            continue;
        };
        let metadata = if metadata.is_symlink() {
            normalized.refresh_registration |= topology_event;
            if !root.follow {
                continue;
            }
            let Ok(metadata) = fs::metadata(path) else {
                normalized
                    .changes
                    .push(WorkspaceChange::DeletePrefix(relative));
                continue;
            };
            metadata
        } else {
            metadata
        };
        let depth = relative.components().count();
        if metadata.is_dir() {
            normalized.refresh_registration |= topology_event;
            if root.recursive
                && root.max_depth.is_none_or(|maximum| depth < maximum)
                && root.policy.can_descend(path)?
            {
                normalized
                    .changes
                    .push(WorkspaceChange::RescanDirectory(relative));
            }
        } else if metadata.is_file()
            && (root.recursive || depth <= 1)
            && root.max_depth.is_none_or(|maximum| depth <= maximum)
            && root.policy.includes_file(path)?
        {
            // Do not suppress empty/oversized files: rescanning removes their
            // previous indexed contents when they stop satisfying size limits.
            normalized.changes.push(WorkspaceChange::Upsert(relative));
        }
    }
    Ok(normalized)
}

fn normalize_policy_change(
    root: &RootSpec,
    path: &Path,
    topology_event: bool,
    control_paths: &[PathBuf],
) -> Result<Option<WorkspaceChange>, HostError> {
    if topology_event && path.is_dir() {
        // A replacement directory must not inherit its predecessor's cached rules.
        root.policy.invalidate()?;
    }
    if topology_event
        && ((root.path.starts_with(path) && path != root.path)
            || control_paths
                .iter()
                .any(|control| control != path && control.starts_with(path)))
    {
        root.policy.invalidate()?;
        return Ok(Some(WorkspaceChange::Rescan));
    }
    // Rule changes precede ordinary hidden/ignored-file checks.
    Ok(root
        .policy
        .control_file_changed(path)?
        .map(WorkspaceChange::RescanDirectory))
}

fn is_removed_path(kind: EventKind, index: usize) -> bool {
    matches!(
        kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From))
    ) || (matches!(kind, EventKind::Modify(ModifyKind::Name(RenameMode::Both))) && index == 0)
}

fn relative_change_path(root: &RootSpec, root_is_file: bool, path: &Path) -> Option<PathBuf> {
    if root_is_file && path == root.path {
        return root.path.file_name().map(PathBuf::from);
    }
    path.strip_prefix(&root.path)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf)
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
    use super::*;
    use crate::PathPolicy;
    use notify::event::CreateKind;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    struct TestPolicy {
        excluded: PathBuf,
        control_file: PathBuf,
        control_changes: AtomicUsize,
        invalidations: AtomicUsize,
    }

    impl PathPolicy for TestPolicy {
        fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
            Ok(!path.starts_with(&self.excluded) && path != self.control_file)
        }

        fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
            Ok(!path.starts_with(&self.excluded))
        }

        fn control_file_changed(&self, path: &Path) -> Result<Option<PathBuf>, HostError> {
            if path != self.control_file {
                return Ok(None);
            }
            self.control_changes.fetch_add(1, Ordering::Relaxed);
            Ok(Some(PathBuf::new()))
        }

        fn control_paths(&self) -> Vec<PathBuf> {
            vec![self.control_file.clone()]
        }

        fn invalidate(&self) -> Result<(), HostError> {
            self.invalidations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn test_root(path: &Path, control_file: PathBuf) -> RootSpec {
        RootSpec::new(
            path.to_path_buf(),
            Arc::new(TestPolicy {
                excluded: path.join("blocked"),
                control_file,
                control_changes: AtomicUsize::new(0),
                invalidations: AtomicUsize::new(0),
            }),
        )
    }

    #[test]
    fn watch_registration_prunes_excluded_trees_and_preserves_external_controls() {
        let temporary = tempfile::tempdir().expect("fixture");
        let root = temporary.path().join("workspace");
        fs::create_dir_all(root.join("selected/deeper")).expect("selected directories");
        fs::create_dir_all(root.join("blocked/deep/deeper")).expect("excluded directories");
        fs::create_dir_all(temporary.path().join("config")).expect("external config directory");
        let spec = test_root(&root, temporary.path().join("config/missing/.ignore"));
        let mut registered = Vec::new();
        let plan = watch_directories(&spec, false, &mut |path| {
            registered.push(path.to_path_buf());
            Ok(())
        })
        .expect("watch paths");
        let paths = plan.directories;
        assert!(paths.contains(&root));
        assert!(paths.contains(&root.join("selected/deeper")));
        assert!(paths.contains(&temporary.path().join("config")));
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(root.join("blocked")))
        );
        let parent = registered
            .iter()
            .position(|path| path == &root.join("selected"))
            .expect("parent watch");
        let child = registered
            .iter()
            .position(|path| path == &root.join("selected/deeper"))
            .expect("child watch");
        assert!(parent < child, "register before visiting children");
    }

    #[test]
    fn removed_or_renamed_sources_are_not_lost_to_current_selection_rules() {
        let temporary = tempfile::tempdir().expect("fixture");
        let spec = test_root(temporary.path(), temporary.path().join(".rules"));
        let old = temporary.path().join("blocked/old.rs");
        fs::create_dir_all(old.parent().expect("old parent")).expect("blocked directory");
        // A rename source can already have been recreated before delivery.
        fs::write(&old, "replacement").expect("replacement file");
        let destination = temporary.path().join("new.rs");
        fs::write(&destination, "renamed source").expect("destination");
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(old.clone())
            .add_path(destination);
        assert_eq!(
            normalize_event(&spec, false, &event)
                .expect("rename")
                .changes,
            vec![
                WorkspaceChange::DeletePrefix(PathBuf::from("blocked/old.rs")),
                WorkspaceChange::Upsert(PathBuf::from("new.rs")),
            ]
        );
        let event = Event::new(EventKind::Remove(RemoveKind::File)).add_path(old);
        assert_eq!(
            normalize_event(&spec, false, &event)
                .expect("deletion")
                .changes,
            vec![WorkspaceChange::Delete(PathBuf::from("blocked/old.rs"))]
        );
    }

    #[test]
    fn external_control_changes_invalidate_policy_before_normal_filtering() {
        let temporary = tempfile::tempdir().expect("fixture");
        let root = temporary.path().join("workspace");
        let control = temporary.path().join("external.ignore");
        let spec = test_root(&root, control.clone());
        let event = Event::new(EventKind::Remove(RemoveKind::File)).add_path(control);
        let result = normalize_event(&spec, false, &event).expect("control change");
        assert!(result.refresh_registration);
        assert_eq!(
            result.changes,
            vec![WorkspaceChange::RescanDirectory(PathBuf::new())]
        );
    }

    #[test]
    fn ordinary_file_creation_does_not_reenumerate_all_watch_directories() {
        let temporary = tempfile::tempdir().expect("fixture");
        let path = temporary.path().join("new.rs");
        fs::write(&path, "fn new() {}").expect("new file");
        let spec = test_root(temporary.path(), temporary.path().join(".rules"));
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(path);
        let result = normalize_event(&spec, false, &event).expect("creation");
        assert!(!result.refresh_registration);
        assert_eq!(
            result.changes,
            vec![WorkspaceChange::Upsert(PathBuf::from("new.rs"))]
        );
    }
    #[test]
    fn unknown_events_discard_cached_rules_before_reconciliation() {
        let temporary = tempfile::tempdir().expect("fixture");
        let policy = Arc::new(TestPolicy {
            excluded: temporary.path().join("blocked"),
            control_file: temporary.path().join(".rules"),
            control_changes: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
        });
        let spec = RootSpec::new(temporary.path().to_path_buf(), policy.clone());
        let result = normalize_event(&spec, false, &Event::new(EventKind::Any)).expect("reconcile");
        assert_eq!(policy.invalidations.load(Ordering::Relaxed), 1);
        assert!(result.refresh_registration);
        assert_eq!(result.changes, vec![WorkspaceChange::Rescan]);
    }
    #[cfg(unix)]
    #[test]
    fn canonical_directory_events_and_rename_sources_use_the_discovered_alias() {
        let temporary = tempfile::tempdir().expect("fixture");
        let root = temporary.path().canonicalize().expect("root");
        let real = root.join("z-real");
        fs::create_dir(&real).expect("target directory");
        std::os::unix::fs::symlink(&real, root.join("a-alias")).expect("directory alias");
        let mut spec = RootSpec::new(root.clone(), Arc::new(crate::AllowAllPaths));
        spec.follow = true;
        let plan = watch_directories(&spec, false, &mut |_| Ok(())).expect("watch plan");
        assert!(plan.directories.contains(&root.join("a-alias")));
        assert!(!plan.directories.contains(&real));
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(real.join("old.rs"))
            .add_path(real.join("new.rs"));
        let translated = plan.aliases.translated_events(&event);
        assert_eq!(translated.len(), 2);
        assert_eq!(
            translated[0].kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::From))
        );
        assert_eq!(translated[0].paths, vec![root.join("a-alias/old.rs")]);
        assert_eq!(
            translated[1].kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::To))
        );
        assert_eq!(translated[1].paths, vec![root.join("a-alias/new.rs")]);
    }

    #[test]
    fn replacement_directories_discard_cached_ignore_rules_before_matching() {
        let temporary = tempfile::tempdir().expect("fixture");
        let replacement = temporary.path().join("replacement");
        fs::create_dir(&replacement).expect("replacement directory");
        let policy = Arc::new(TestPolicy {
            excluded: temporary.path().join("blocked"),
            control_file: temporary.path().join(".rules"),
            control_changes: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
        });
        let spec = RootSpec::new(temporary.path().to_path_buf(), policy.clone());
        let event = Event::new(EventKind::Create(CreateKind::Folder)).add_path(replacement);
        let result = normalize_event(&spec, false, &event).expect("new directory");
        assert_eq!(policy.invalidations.load(Ordering::Relaxed), 1);
        assert_eq!(
            result.changes,
            vec![WorkspaceChange::RescanDirectory("replacement".into())]
        );
    }
    #[derive(Debug)]
    struct MarkerPolicy {
        directory: PathBuf,
        marker: PathBuf,
        controls: StdMutex<Vec<PathBuf>>,
    }

    impl PathPolicy for MarkerPolicy {
        fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
            self.can_descend(path)
        }

        fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
            if path.starts_with(&self.directory) && self.marker.exists() {
                *self.controls.lock().expect("controls") = vec![self.marker.clone()];
                return Ok(false);
            }
            Ok(!path.starts_with(&self.marker))
        }

        fn control_paths(&self) -> Vec<PathBuf> {
            self.controls.lock().expect("controls").clone()
        }
    }

    #[test]
    fn a_dynamically_discovered_marker_keeps_only_the_rejected_directory_watch() {
        let temporary = tempfile::tempdir().expect("fixture");
        let directory = temporary.path().join("nested");
        let marker = directory.join(".git");
        fs::create_dir_all(marker.join("objects/deep")).expect("marker subtree");
        fs::create_dir_all(directory.join("src/deep")).expect("source subtree");
        let policy = Arc::new(MarkerPolicy {
            directory: directory.clone(),
            marker: marker.clone(),
            controls: StdMutex::new(Vec::new()),
        });
        let spec = RootSpec::new(temporary.path().to_path_buf(), policy);
        let (sender, _receiver) = mpsc::channel(32);
        let mut watcher = create_watcher(
            &spec,
            false,
            &sender,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(Notify::new()),
            Some(Duration::from_secs(60)),
        )
        .expect("watcher");
        assert!(
            watcher.registrations.contains_key(&directory),
            "retain marker observation"
        );
        assert!(!watcher.registrations.contains_key(&marker));
        assert!(!watcher.registrations.contains_key(&directory.join("src")));
        fs::remove_dir_all(&marker).expect("remove directory marker");
        watcher.refresh(&spec, false).expect("reopen subtree");
        assert!(
            watcher
                .registrations
                .contains_key(&directory.join("src/deep"))
        );
        fs::write(&marker, "marker file").expect("create file marker");
        watcher.refresh(&spec, false).expect("close subtree again");
        assert!(watcher.registrations.contains_key(&directory));
        assert!(!watcher.registrations.contains_key(&directory.join("src")));
        assert!(
            !watcher
                .registrations
                .contains_key(&directory.join("src/deep"))
        );
    }
}
