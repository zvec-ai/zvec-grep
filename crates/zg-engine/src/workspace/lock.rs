use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use uuid::Uuid;

use crate::{EngineError, utils::create_directories};

const LOCK_INFO_FILE: &str = "lock.json";
const DEFAULT_STALE_LOCK: Duration = Duration::from_hours(6);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    Read,
    Write,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileLockInfo {
    pub token: String,
    pub pid: u32,
    pub hostname: String,
    pub started_at: u64,
    pub operation: String,
}

#[derive(Debug)]
pub(crate) struct FileLock {
    path: PathBuf,
    info: FileLockInfo,
    released: bool,
}

impl FileLock {
    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        release_file_lock(&self.path, &self.info);
        self.released = true;
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release_inner();
    }
}

pub(crate) fn acquire_home_lock(
    home: &Path,
    mode: LockMode,
    operation: &str,
) -> Result<FileLock, EngineError> {
    acquire_read_write_lock(
        &home.join("locks/home"),
        mode,
        operation,
        DEFAULT_STALE_LOCK,
    )
}

pub(crate) fn acquire_read_write_lock(
    lock_path: &Path,
    mode: LockMode,
    operation: &str,
    stale_after: Duration,
) -> Result<FileLock, EngineError> {
    match mode {
        LockMode::Read => acquire_read_lock(lock_path, operation, stale_after),
        LockMode::Write => acquire_write_lock(lock_path, operation, stale_after),
    }
}

fn acquire_read_lock(
    lock_path: &Path,
    operation: &str,
    stale_after: Duration,
) -> Result<FileLock, EngineError> {
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    create_directories(parent).map_err(|error| lock_io("create lock parent", parent, &error))?;
    let write_path = write_lock_path(lock_path);

    for _ in 0..2 {
        if write_path.exists() {
            if cleanup_stale_lock(&write_path, stale_after)? {
                continue;
            }
            return Err(lock_busy(&write_path, operation));
        }

        let info = current_lock_info(operation);
        let reader_path = readers_lock_path(lock_path).join(format!("{}-{}", info.pid, info.token));
        create_directories(reader_path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| lock_io("create readers directory", &reader_path, &error))?;
        match fs::create_dir(&reader_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(lock_io("create read lock", &reader_path, &error)),
        }
        if let Err(error) = write_lock_info(&reader_path, &info) {
            let _ = fs::remove_dir_all(&reader_path);
            return Err(error);
        }

        if write_path.exists() {
            release_file_lock(&reader_path, &info);
            if cleanup_stale_lock(&write_path, stale_after)? {
                continue;
            }
            return Err(lock_busy(&write_path, operation));
        }

        return Ok(FileLock {
            path: reader_path,
            info,
            released: false,
        });
    }
    Err(lock_busy(&write_path, operation))
}

fn acquire_write_lock(
    lock_path: &Path,
    operation: &str,
    stale_after: Duration,
) -> Result<FileLock, EngineError> {
    let lock =
        acquire_exclusive_directory_lock(&write_lock_path(lock_path), operation, stale_after)?;
    if has_active_readers(lock_path, stale_after)? {
        drop(lock);
        return Err(read_lock_busy(lock_path, operation));
    }
    Ok(lock)
}

fn acquire_exclusive_directory_lock(
    lock_path: &Path,
    operation: &str,
    stale_after: Duration,
) -> Result<FileLock, EngineError> {
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    create_directories(parent).map_err(|error| lock_io("create lock parent", parent, &error))?;
    for _ in 0..2 {
        match fs::create_dir(lock_path) {
            Ok(()) => {
                let info = current_lock_info(operation);
                if let Err(error) = write_lock_info(lock_path, &info) {
                    let _ = fs::remove_dir_all(lock_path);
                    return Err(error);
                }
                return Ok(FileLock {
                    path: lock_path.to_path_buf(),
                    info,
                    released: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if cleanup_stale_lock(lock_path, stale_after)? {
                    continue;
                }
                return Err(lock_busy(lock_path, operation));
            }
            Err(error) => return Err(lock_io("create write lock", lock_path, &error)),
        }
    }
    Err(lock_busy(lock_path, operation))
}

fn write_lock_info(lock_path: &Path, info: &FileLockInfo) -> Result<(), EngineError> {
    let path = lock_path.join(LOCK_INFO_FILE);
    let mut bytes = serde_json::to_vec_pretty(info)
        .map_err(|error| EngineError::internal(format!("failed to encode lock info: {error}")))?;
    bytes.push(b'\n');
    fs::write(&path, bytes).map_err(|error| lock_io("write lock info", &path, &error))
}

fn read_lock_info(lock_path: &Path) -> Option<FileLockInfo> {
    fs::read(lock_path.join(LOCK_INFO_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn release_file_lock(lock_path: &Path, owner: &FileLockInfo) {
    if read_lock_info(lock_path).as_ref().map(|info| &info.token) != Some(&owner.token) {
        return;
    }
    let _ = fs::remove_dir_all(lock_path);
}

fn cleanup_stale_lock(lock_path: &Path, stale_after: Duration) -> Result<bool, EngineError> {
    if !lock_path.exists() {
        return Ok(false);
    }
    let info = read_lock_info(lock_path);
    if !is_stale_lock(lock_path, info.as_ref(), stale_after) {
        return Ok(false);
    }
    fs::remove_dir_all(lock_path)
        .map_err(|error| lock_io("remove stale lock", lock_path, &error))?;
    Ok(true)
}

fn has_active_readers(lock_path: &Path, stale_after: Duration) -> Result<bool, EngineError> {
    let readers_path = readers_lock_path(lock_path);
    let entries = match fs::read_dir(&readers_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Ok(false),
    };
    let mut active = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if cleanup_stale_lock(&path, stale_after)? {
            continue;
        }
        active = true;
    }
    Ok(active)
}

fn is_stale_lock(lock_path: &Path, info: Option<&FileLockInfo>, stale_after: Duration) -> bool {
    let started_at = info.map_or_else(|| lock_directory_mtime(lock_path), |info| info.started_at);
    let expired = epoch_millis().saturating_sub(started_at) > duration_millis(stale_after);
    if let Some(info) = info
        && info.hostname == hostname()
        && !process_is_alive(info.pid)
    {
        return true;
    }
    expired
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

fn current_lock_info(operation: &str) -> FileLockInfo {
    FileLockInfo {
        token: Uuid::new_v4().to_string(),
        pid: std::process::id(),
        hostname: hostname(),
        started_at: epoch_millis(),
        operation: operation.to_owned(),
    }
}

fn lock_directory_mtime(lock_path: &Path) -> u64 {
    fs::metadata(lock_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or_else(epoch_millis, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn hostname() -> String {
    System::host_name().unwrap_or_else(|| "unknown-host".to_owned())
}

fn write_lock_path(lock_path: &Path) -> PathBuf {
    let mut path = lock_path.as_os_str().to_os_string();
    path.push(".write");
    PathBuf::from(path)
}

fn readers_lock_path(lock_path: &Path) -> PathBuf {
    let mut path = lock_path.as_os_str().to_os_string();
    path.push(".readers");
    PathBuf::from(path)
}

#[track_caller]
fn lock_busy(lock_path: &Path, requested_operation: &str) -> EngineError {
    let owner = read_lock_info(lock_path);
    EngineError::resource_busy(format!(
        "index unavailable: lock={} operation={} owner_operation={} owner_pid={} owner_host={}",
        lock_path.display(),
        requested_operation,
        owner
            .as_ref()
            .map_or("unknown", |info| info.operation.as_str()),
        owner.as_ref().map_or(0, |info| info.pid),
        owner
            .as_ref()
            .map_or("unknown", |info| info.hostname.as_str())
    ))
}

#[track_caller]
fn read_lock_busy(lock_path: &Path, requested_operation: &str) -> EngineError {
    let readers_path = readers_lock_path(lock_path);
    let owner = fs::read_dir(&readers_path).ok().and_then(|entries| {
        entries
            .flatten()
            .find_map(|entry| read_lock_info(&entry.path()))
    });
    EngineError::resource_busy(format!(
        "index unavailable: lock={} operation={} owner_operation={} owner_pid={} owner_host={}",
        readers_path.display(),
        requested_operation,
        owner
            .as_ref()
            .map_or("unknown", |info| info.operation.as_str()),
        owner.as_ref().map_or(0, |info| info.pid),
        owner
            .as_ref()
            .map_or("unknown", |info| info.hostname.as_str())
    ))
}

#[track_caller]
fn lock_io(operation: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!("{operation} workspace lock {}", path.display()),
        error,
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn readers_share_the_lock_and_block_a_writer() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("locks/home");
        let first = acquire_read_write_lock(&path, LockMode::Read, "context", DEFAULT_STALE_LOCK)
            .expect("first reader");
        let second = acquire_read_write_lock(&path, LockMode::Read, "info", DEFAULT_STALE_LOCK)
            .expect("second reader");

        assert!(
            acquire_read_write_lock(&path, LockMode::Write, "index", DEFAULT_STALE_LOCK).is_err()
        );
        drop(first);
        drop(second);
        assert!(
            acquire_read_write_lock(&path, LockMode::Write, "index", DEFAULT_STALE_LOCK).is_ok()
        );
    }

    #[test]
    fn writer_blocks_readers_until_released() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("locks/home");
        let writer = acquire_read_write_lock(&path, LockMode::Write, "index", DEFAULT_STALE_LOCK)
            .expect("writer");

        assert!(
            acquire_read_write_lock(&path, LockMode::Read, "context", DEFAULT_STALE_LOCK).is_err()
        );
        drop(writer);
        assert!(
            acquire_read_write_lock(&path, LockMode::Read, "context", DEFAULT_STALE_LOCK).is_ok()
        );
    }

    #[test]
    fn release_only_removes_a_lock_owned_by_the_same_token() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("locks/home");
        let lock = acquire_read_write_lock(&path, LockMode::Write, "index", DEFAULT_STALE_LOCK)
            .expect("writer");
        let different_owner = FileLockInfo {
            token: Uuid::new_v4().to_string(),
            ..lock.info.clone()
        };

        release_file_lock(&lock.path, &different_owner);
        assert!(lock.path.exists());
        drop(lock);
        assert!(!write_lock_path(&path).exists());
    }

    #[test]
    fn dead_same_host_owner_is_reclaimed() {
        let directory = tempdir().expect("temporary directory");
        let path = write_lock_path(&directory.path().join("locks/home"));
        fs::create_dir_all(&path).expect("stale lock directory");
        write_lock_info(
            &path,
            &FileLockInfo {
                token: Uuid::new_v4().to_string(),
                pid: u32::MAX,
                hostname: hostname(),
                started_at: epoch_millis(),
                operation: "dead".to_owned(),
            },
        )
        .expect("stale lock info");

        assert!(cleanup_stale_lock(&path, DEFAULT_STALE_LOCK).expect("cleanup"));
        assert!(!path.exists());
    }
}
