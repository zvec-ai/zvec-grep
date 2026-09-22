//! Cross-process model cache locking and stale-owner recovery.

use std::{
    fs::{self, FileTimes, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::error::{ArtifactDownloadError, FailureKind};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);
pub(super) const LOCK_STALE_AFTER: Duration = Duration::from_mins(10);
const LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

struct CleanupDirectory {
    path: PathBuf,
}

impl CleanupDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for CleanupDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) struct CacheLock {
    pub(super) lock_path: PathBuf,
    pub(super) owner_path: PathBuf,
    pub(super) last_heartbeat: std::sync::Mutex<SystemTime>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct LockOwner {
    pub(super) token: String,
    pub(super) pid: u32,
    pub(super) hostname: String,
}

impl CacheLock {
    pub(super) async fn acquire(
        lock_path: &Path,
        signal: Option<&CancellationToken>,
    ) -> Result<Self, ArtifactDownloadError> {
        loop {
            check_cancelled(signal)?;
            match Self::try_acquire(lock_path) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) => {}
                Err(error) => {
                    return Err(ArtifactDownloadError::new(
                        FailureKind::Filesystem,
                        format!("unable to acquire model cache lock: {error}"),
                    ));
                }
            }
            if remove_stale_lock(lock_path).map_err(|error| {
                ArtifactDownloadError::new(
                    FailureKind::Filesystem,
                    format!("unable to recover model cache lock: {error}"),
                )
            })? {
                continue;
            }
            wait_for_retry(signal).await?;
        }
    }

    pub(super) fn try_acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        if fs::symlink_metadata(lock_path).is_ok() {
            return Ok(None);
        }
        let token = Uuid::new_v4().to_string();
        let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
        let name = lock_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact.lock");
        let staging = parent.join(format!(".{name}.pending-{}-{token}", std::process::id()));
        fs::create_dir(&staging)?;
        let _staging_cleanup = CleanupDirectory::new(staging.clone());
        let owner_name = format!(".owner-{token}");
        let staging_owner = staging.join(&owner_name);
        let owner = LockOwner {
            token: token.clone(),
            pid: std::process::id(),
            hostname: System::host_name().unwrap_or_default(),
        };
        let mut owner_json = serde_json::to_vec(&owner).map_err(io::Error::other)?;
        owner_json.push(b'\n');
        fs::write(&staging_owner, owner_json)?;
        match fs::rename(&staging, lock_path) {
            Ok(()) => {}
            Err(error) if is_directory_conflict(&error, lock_path) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let lock = Self {
            lock_path: lock_path.to_path_buf(),
            owner_path: lock_path.join(owner_name),
            last_heartbeat: std::sync::Mutex::new(UNIX_EPOCH),
        };
        lock.finish_acquire()
    }

    pub(super) fn finish_acquire(self) -> io::Result<Option<Self>> {
        match self.refresh_owner() {
            Ok(()) => Ok(Some(self)),
            // A stale-lock contender can displace us after rename succeeds but
            // before the first heartbeat. Retry instead of returning a lock we
            // no longer own or failing the entire model load.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn touch(&self) -> Result<(), ArtifactDownloadError> {
        let mut last = self
            .last_heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SystemTime::now().duration_since(*last).unwrap_or_default() < LOCK_HEARTBEAT_INTERVAL {
            return Ok(());
        }
        touch_owner(&self.owner_path).map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("model cache lock ownership was lost: {error}"),
            )
        })?;
        *last = SystemTime::now();
        Ok(())
    }

    pub(super) fn assert_owned(&self) -> Result<(), ArtifactDownloadError> {
        self.refresh_owner().map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("model cache lock ownership was lost: {error}"),
            )
        })
    }

    fn refresh_owner(&self) -> io::Result<()> {
        touch_owner(&self.owner_path)?;
        *self
            .last_heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SystemTime::now();
        Ok(())
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        if self.owner_path.exists() {
            let _ = fs::remove_file(&self.owner_path);
            let _ = fs::remove_dir(&self.lock_path);
        }
    }
}

fn touch_owner(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_times(FileTimes::new().set_modified(SystemTime::now()))
}

fn remove_stale_lock(lock_path: &Path) -> io::Result<bool> {
    remove_stale_lock_at(lock_path, SystemTime::now(), LOCK_STALE_AFTER)
}

pub(super) fn remove_stale_lock_at(
    lock_path: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> io::Result<bool> {
    let Some(observed) = inspect_lock(lock_path)? else {
        return Ok(true);
    };
    if !lock_is_abandoned(&observed, now, stale_after) {
        return Ok(false);
    }
    let stale =
        lock_path.with_extension(format!("stale-{}-{}", std::process::id(), Uuid::new_v4()));
    match fs::rename(lock_path, &stale) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    }
    let Some(moved) = inspect_lock(&stale)? else {
        return Ok(true);
    };
    if !lock_is_abandoned(&moved, now, stale_after) {
        match fs::rename(&stale, lock_path) {
            Ok(()) => {}
            Err(error) if is_directory_conflict(&error, lock_path) => {
                // A successor owns lock_path. Leave the newly refreshed displaced
                // owner intact rather than deleting another process's lease.
            }
            Err(error) => return Err(error),
        }
        return Ok(false);
    }
    match fs::remove_dir_all(stale) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

pub(super) struct LockObservation {
    newest_heartbeat: SystemTime,
    dead_owner: bool,
}

fn lock_is_abandoned(
    observation: &LockObservation,
    now: SystemTime,
    stale_after: Duration,
) -> bool {
    observation.dead_owner
        || now
            .duration_since(observation.newest_heartbeat)
            .unwrap_or_default()
            >= stale_after
}

fn inspect_lock(path: &Path) -> io::Result<Option<LockObservation>> {
    let Some(metadata) = lock_component(fs::metadata(path))? else {
        return Ok(None);
    };
    inspect_existing_lock(path, &metadata)
}

pub(super) fn inspect_existing_lock(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<Option<LockObservation>> {
    let mut newest = metadata.modified().unwrap_or(UNIX_EPOCH);
    let mut owners = Vec::new();
    let Some(entries) = lock_component(fs::read_dir(path))? else {
        return Ok(None);
    };
    for entry in entries {
        let Some(entry) = lock_component(entry)? else {
            return Ok(None);
        };
        let Some(file_type) = lock_component(entry.file_type())? else {
            return Ok(None);
        };
        if file_type.is_file() && entry.file_name().to_string_lossy().starts_with(".owner-") {
            // Re-read metadata through the path instead of relying on
            // `DirEntry`'s platform-specific cache. In particular, Windows
            // may keep returning cached metadata after the owner file has
            // already been removed by the lock holder.
            let Some(metadata) = lock_entry_metadata(&entry)? else {
                return Ok(None);
            };
            newest = newest.max(metadata.modified().unwrap_or(UNIX_EPOCH));
            owners.push(entry.path());
        }
    }
    let dead_owner = owners.len() == 1 && is_known_dead_owner(&owners[0]);
    Ok(Some(LockObservation {
        newest_heartbeat: newest,
        dead_owner,
    }))
}

fn lock_component<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn is_directory_conflict(error: &io::Error, path: &Path) -> bool {
    match error.kind() {
        // These errors already prove that rename observed a competing target.
        // The target may be released before a follow-up metadata lookup.
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty => true,
        // Windows may report PermissionDenied when the target is an existing
        // directory. Preserve genuine permission failures when it is not.
        io::ErrorKind::PermissionDenied => {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        }
        _ => false,
    }
}

pub(super) fn lock_entry_metadata(entry: &fs::DirEntry) -> io::Result<Option<fs::Metadata>> {
    lock_component(fs::metadata(entry.path()))
}

fn is_known_dead_owner(owner_path: &Path) -> bool {
    let Ok(bytes) = fs::read(owner_path) else {
        return false;
    };
    let Ok(owner) = serde_json::from_slice::<LockOwner>(&bytes) else {
        return false;
    };
    let Some(owner_name) = owner_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hostname) = System::host_name() else {
        return false;
    };
    if owner.hostname.is_empty()
        || owner.hostname != hostname
        || owner.token.is_empty()
        || owner_name != format!(".owner-{}", owner.token)
        || owner.pid == 0
        || owner.pid > i32::MAX as u32
    {
        return false;
    }
    let mut system = System::new();
    let pid = Pid::from_u32(owner.pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_none()
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ArtifactDownloadError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(cancelled())
    } else {
        Ok(())
    }
}

async fn wait_for_retry(signal: Option<&CancellationToken>) -> Result<(), ArtifactDownloadError> {
    if let Some(signal) = signal {
        tokio::select! {
            () = signal.cancelled() => Err(cancelled()),
            () = tokio::time::sleep(LOCK_POLL_INTERVAL) => Ok(()),
        }
    } else {
        tokio::time::sleep(LOCK_POLL_INTERVAL).await;
        Ok(())
    }
}

fn cancelled() -> ArtifactDownloadError {
    ArtifactDownloadError::new(
        FailureKind::Cancelled,
        "model artifact download was cancelled",
    )
}
