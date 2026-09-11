use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(not(windows))]
use std::process::{Child as DaemonChild, Command, Stdio};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, Signal, System};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use uuid::Uuid;
// `std::process::Command` broadly inherits inheritable handles on Windows.
// An explicit handle list prevents the resident daemon from retaining a
// bootstrap client's stdout/stderr pipes and suppressing EOF indefinitely.
#[cfg(windows)]
use windows_spawn::{Child as DaemonChild, Command, Stdio};

use crate::{DaemonError, ListenAddress, ServerConfig};

const INSTANCE_FILE: &str = "instance.lock";
const STARTUP_FILE: &str = "startup.lock";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const SIGNAL_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInstanceRecord {
    pub pid: u32,
    pub hostname: String,
    pub instance_token: Uuid,
    pub started_at: u64,
    pub updated_at: u64,
    pub server_url: String,
    #[serde(default)]
    pub listen: String,
    pub ready: bool,
    #[serde(default = "legacy_full_toolset")]
    pub mcp_toolset: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DaemonStatus {
    pub running: bool,
    pub ready: bool,
    pub pid: Option<u32>,
    pub server_url: Option<String>,
    pub mcp_toolset: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupRecord {
    pid: u32,
    hostname: String,
    token: Uuid,
}

struct StartupLock {
    path: PathBuf,
    record: StartupRecord,
}

impl StartupLock {
    async fn acquire(home: &Path) -> Result<Self, DaemonError> {
        let daemon_dir = daemon_dir(home);
        create_private_dir(&daemon_dir)?;
        let path = daemon_dir.join(STARTUP_FILE);
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            let record = StartupRecord {
                pid: std::process::id(),
                hostname: hostname(),
                token: Uuid::new_v4(),
            };
            if try_create_startup_record(&path, &record)? {
                return Ok(Self { path, record });
            }
            let existing = match read_startup_record(&path).await {
                Ok(existing) => existing,
                Err(DaemonError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if existing.hostname == hostname() && process_is_alive(existing.pid) {
                if tokio::time::Instant::now() >= deadline {
                    return Err(DaemonError::Timeout { action: "start" });
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            } else {
                remove_startup_record_if_same(&path, &existing).await?;
            }
        }
    }

    async fn release(self) -> Result<(), DaemonError> {
        remove_startup_record_if_same(&self.path, &self.record).await?;
        Ok(())
    }
}

fn try_create_startup_record(path: &Path, record: &StartupRecord) -> Result<bool, DaemonError> {
    // Publish a fully-written record with an atomic hard-link operation. A
    // contender can therefore never observe the empty/partial contents that a
    // direct create_new + write sequence would expose.
    let candidate = path.with_file_name(format!(".startup-{}.claim", record.token));
    let write_result = (|| -> Result<(), DaemonError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)?;
        set_private_file(&file)?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&candidate);
        return Err(error);
    }
    let linked = match std::fs::hard_link(&candidate, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&candidate);
            return Err(error.into());
        }
    };
    let _ = std::fs::remove_file(candidate);
    Ok(linked)
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        let Ok(current) = serde_json::from_slice::<StartupRecord>(&bytes) else {
            return;
        };
        if current == self.record {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) struct InstanceLock {
    path: PathBuf,
    record: DaemonInstanceRecord,
}

impl InstanceLock {
    pub(crate) async fn acquire(config: &ServerConfig) -> Result<Self, DaemonError> {
        let daemon_dir = daemon_dir(&config.home);
        create_private_dir(&daemon_dir)?;
        let path = daemon_dir.join(INSTANCE_FILE);
        for _ in 0..3 {
            let now = epoch_millis();
            let record = DaemonInstanceRecord {
                pid: std::process::id(),
                hostname: hostname(),
                instance_token: Uuid::new_v4(),
                started_at: now,
                updated_at: now,
                server_url: config.listen.server_url(),
                listen: config.listen.to_string(),
                ready: false,
                mcp_toolset: config.mcp_toolset.to_string(),
            };
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    set_private_file(&file)?;
                    serde_json::to_writer(&mut file, &record)?;
                    file.write_all(b"\n")?;
                    file.sync_all()?;
                    return Ok(Self { path, record });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(existing) = read_instance_record_path(&path).await?
                        && existing.hostname == hostname()
                        && process_is_alive(existing.pid)
                    {
                        return Err(DaemonError::AlreadyRunning { pid: existing.pid });
                    }
                    remove_file_if_exists(&path).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(DaemonError::InvalidRecord(path))
    }

    pub(crate) async fn mark_ready(&mut self) -> Result<(), DaemonError> {
        self.record.ready = true;
        self.record.updated_at = epoch_millis();
        let current = read_instance_record_path(&self.path).await?;
        if !same_instance(current.as_ref(), &self.record) {
            return Err(DaemonError::InstanceChanged {
                pid: self.record.pid,
            });
        }
        let bytes = serde_json::to_vec(&self.record)?;
        tokio::fs::write(&self.path, [bytes.as_slice(), b"\n"].concat()).await?;
        set_private_path(&self.path)?;
        Ok(())
    }

    pub(crate) async fn release(self) -> Result<(), DaemonError> {
        let current = read_instance_record_path(&self.path).await?;
        if same_instance(current.as_ref(), &self.record) {
            remove_file_if_exists(&self.path).await?;
        }
        Ok(())
    }
}

/// Starts the configured daemon process or returns the existing ready process.
///
/// # Errors
///
/// Returns process, state-record, listen-address, or startup timeout failures.
pub async fn start_server(
    executable: &Path,
    config: &ServerConfig,
) -> Result<DaemonStatus, DaemonError> {
    if let Some(current) = existing_server(config).await? {
        return Ok(current);
    }

    // The status check above is intentionally only a fast path. This
    // cross-process claim closes the check/spawn race for concurrent stdio
    // bootstrap processes. The winner alone may spawn; waiters re-check state
    // after the claim is released and reuse the ready daemon.
    let startup = StartupLock::acquire(&config.home).await?;
    let result = start_server_with_lock(executable, config).await;
    let release = startup.release().await;
    match (result, release) {
        (Ok(status), Ok(())) => Ok(status),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn existing_server(config: &ServerConfig) -> Result<Option<DaemonStatus>, DaemonError> {
    let current = server_status(&config.home).await?;
    if current.running {
        let requested_toolset = config.mcp_toolset.to_string();
        if current.mcp_toolset.as_deref() != Some(requested_toolset.as_str()) {
            return Err(DaemonError::ToolsetMismatch {
                active: current.mcp_toolset.unwrap_or_else(|| "unknown".to_owned()),
            });
        }
        if current.ready {
            return Ok(Some(current));
        }
        return wait_for_status(&config.home, true, START_TIMEOUT, None)
            .await
            .map(Some);
    }
    Ok(None)
}

async fn start_server_with_lock(
    executable: &Path,
    config: &ServerConfig,
) -> Result<DaemonStatus, DaemonError> {
    if let Some(current) = existing_server(config).await? {
        return Ok(current);
    }

    crate::resolve_token(config.token_file.as_deref())?;
    assert_address_available(&config.listen).await?;
    let daemon_dir = daemon_dir(&config.home);
    create_private_dir(&daemon_dir)?;
    let log_path = daemon_dir.join("server.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    set_private_file(&stdout)?;
    let stderr = stdout.try_clone()?;
    let mut command = Command::new(executable);
    command
        .arg("server")
        .arg("run")
        .arg("--mcp-toolset")
        .arg(config.mcp_toolset.to_string())
        .arg("--listen")
        .arg(config.listen.to_string())
        .arg("--home")
        .arg(&config.home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(file) = &config.token_file {
        command.arg("--token-file").arg(file);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    wait_for_status(&config.home, true, START_TIMEOUT, Some((child, log_path))).await
}

/// Requests graceful shutdown and falls back to verified process termination.
///
/// # Errors
///
/// Returns state-record, HTTP control, PID identity, signal, or timeout failures.
pub async fn stop_server(home: &Path, timeout: Duration) -> Result<DaemonStatus, DaemonError> {
    stop_server_with_token(home, timeout, None).await
}

/// Stops the daemon using an explicitly selected token file.
/// # Errors
/// Returns credential, lifecycle, or process verification errors.
pub async fn stop_server_with_token(
    home: &Path,
    timeout: Duration,
    token_file: Option<&Path>,
) -> Result<DaemonStatus, DaemonError> {
    let token = crate::resolve_token(token_file)?;
    let Some(record) = read_instance_record(home).await? else {
        return Ok(DaemonStatus::default());
    };
    if record.hostname != hostname() || !process_is_alive(record.pid) {
        remove_record_if_same(home, &record).await?;
        return Ok(DaemonStatus::default());
    }
    if record.pid == std::process::id() {
        return Err(DaemonError::RefuseCurrentProcess);
    }

    let address = record_address(&record, home)?;
    let accepted = http_request(
        &address,
        "POST",
        "/control/shutdown",
        HTTP_TIMEOUT,
        token.as_deref(),
    )
    .await
    .is_ok_and(|status| status == 202);
    if accepted && wait_for_exit(record.pid, timeout).await {
        remove_record_if_same(home, &record).await?;
        return Ok(DaemonStatus::default());
    }

    assert_same_instance(home, &record, accepted).await?;
    signal_process(record.pid, Signal::Term);
    if wait_for_exit(record.pid, SIGNAL_GRACE.min(timeout)).await {
        remove_record_if_same(home, &record).await?;
        return Ok(DaemonStatus::default());
    }
    assert_same_instance(home, &record, true).await?;
    force_kill_process(record.pid);
    if !wait_for_exit(record.pid, SIGNAL_GRACE).await {
        return Err(DaemonError::Timeout { action: "stop" });
    }
    remove_record_if_same(home, &record).await?;
    Ok(DaemonStatus::default())
}

pub async fn server_status(home: &Path) -> Result<DaemonStatus, DaemonError> {
    let Some(record) = read_instance_record(home).await? else {
        return Ok(DaemonStatus::default());
    };
    if record.hostname != hostname() || !process_is_alive(record.pid) {
        return Ok(DaemonStatus::default());
    }
    let address = record_address(&record, home)?;
    let healthy = http_request(&address, "GET", "/healthz", Duration::from_secs(1), None)
        .await
        .is_ok_and(|status| status == 200);
    Ok(DaemonStatus {
        running: true,
        ready: healthy && record.ready,
        pid: Some(record.pid),
        server_url: Some(record.server_url),
        mcp_toolset: Some(record.mcp_toolset),
    })
}

pub(crate) async fn read_instance_record(
    home: &Path,
) -> Result<Option<DaemonInstanceRecord>, DaemonError> {
    read_instance_record_path(&instance_path(home)).await
}

async fn read_instance_record_path(
    path: &Path,
) -> Result<Option<DaemonInstanceRecord>, DaemonError> {
    for attempt in 0..3 {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if let Ok(record) = serde_json::from_slice(&bytes) {
            return Ok(Some(record));
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Err(DaemonError::InvalidRecord(path.to_owned()))
}

async fn read_startup_record(path: &Path) -> Result<StartupRecord, DaemonError> {
    for attempt in 0..3 {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && attempt < 2 => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if let Ok(record) = serde_json::from_slice(&bytes) {
            return Ok(record);
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    Err(DaemonError::InvalidRecord(path.to_owned()))
}

async fn remove_startup_record_if_same(
    path: &Path,
    expected: &StartupRecord,
) -> Result<(), DaemonError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if serde_json::from_slice::<StartupRecord>(&bytes)
        .ok()
        .as_ref()
        == Some(expected)
    {
        remove_file_if_exists(path).await?;
    }
    Ok(())
}

async fn wait_for_status(
    home: &Path,
    running: bool,
    timeout: Duration,
    mut child: Option<(DaemonChild, PathBuf)>,
) -> Result<DaemonStatus, DaemonError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status = server_status(home).await?;
        if running && status.running && status.ready {
            return Ok(status);
        }
        if !running && !status.running {
            return Ok(status);
        }
        if let Some((process, log_path)) = &mut child
            && process.try_wait()?.is_some()
        {
            return Err(DaemonError::ChildExited {
                log_path: log_path.clone(),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonError::Timeout {
                action: if running { "start" } else { "stop" },
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_address_available(address: &ListenAddress) -> Result<(), DaemonError> {
    match tokio::net::TcpListener::bind(address.socket_addr()).await {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            Err(DaemonError::AddressInUse(address.to_string()))
        }
        Err(error) => Err(error.into()),
    }
}

async fn http_request(
    address: &ListenAddress,
    method: &str,
    path: &str,
    timeout: Duration,
    token: Option<&str>,
) -> Result<u16, DaemonError> {
    tokio::time::timeout(timeout, async {
        let mut stream = TcpStream::connect(address.socket_addr()).await?;
        let authorization = token.map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{authorization}Content-Length: 0\r\n\r\n",
            address.socket_addr()
        );
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        let status_line = response
            .split(|byte| *byte == b'\n')
            .next()
            .and_then(|line| std::str::from_utf8(line).ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing HTTP status"))?;
        status_line
            .split_ascii_whitespace()
            .nth(1)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing HTTP status code"))?
            .parse::<u16>()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
    .await
    .map_err(|_| DaemonError::Timeout { action: "contact" })?
    .map_err(Into::into)
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn with_process<T>(pid: u32, action: impl FnOnce(&sysinfo::Process) -> T) -> Option<T> {
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(action)
}

fn process_is_alive(pid: u32) -> bool {
    with_process(pid, |_| ()).is_some()
}

fn signal_process(pid: u32, signal: Signal) {
    let _ = with_process(pid, |process| {
        process.kill_with(signal).unwrap_or_else(|| process.kill())
    });
}

fn force_kill_process(pid: u32) {
    let _ = with_process(pid, sysinfo::Process::kill);
}

async fn assert_same_instance(
    home: &Path,
    expected: &DaemonInstanceRecord,
    allow_missing: bool,
) -> Result<(), DaemonError> {
    let current = read_instance_record(home).await?;
    if current.is_none() && allow_missing {
        return Ok(());
    }
    if !same_instance(current.as_ref(), expected) {
        return Err(DaemonError::InstanceChanged { pid: expected.pid });
    }
    Ok(())
}

async fn remove_record_if_same(
    home: &Path,
    expected: &DaemonInstanceRecord,
) -> Result<(), DaemonError> {
    let path = instance_path(home);
    if same_instance(read_instance_record_path(&path).await?.as_ref(), expected) {
        remove_file_if_exists(&path).await?;
    }
    Ok(())
}

fn same_instance(current: Option<&DaemonInstanceRecord>, expected: &DaemonInstanceRecord) -> bool {
    current.is_some_and(|current| {
        current.pid == expected.pid && current.instance_token == expected.instance_token
    })
}

fn record_address(
    record: &DaemonInstanceRecord,
    home: &Path,
) -> Result<ListenAddress, DaemonError> {
    let value = if record.listen.is_empty() {
        record
            .server_url
            .strip_prefix("http://")
            .and_then(|value| value.strip_suffix("/mcp"))
            .unwrap_or_default()
    } else {
        &record.listen
    };
    value
        .parse()
        .map_err(|_| DaemonError::InvalidRecord(instance_path(home)))
}

fn legacy_full_toolset() -> String {
    "full".to_owned()
}

fn daemon_dir(home: &Path) -> PathBuf {
    home.join("daemon")
}

fn instance_path(home: &Path) -> PathBuf {
    daemon_dir(home).join(INSTANCE_FILE)
}

fn hostname() -> String {
    System::host_name().unwrap_or_else(|| "unknown-host".to_owned())
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn create_private_dir(path: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(file: &std::fs::File) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_private_path(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

async fn remove_file_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{DaemonInstanceRecord, read_instance_record};

    #[tokio::test]
    async fn missing_instance_record_is_stopped() {
        let home = TempDir::new().expect("temp home");
        let record: Option<DaemonInstanceRecord> = read_instance_record(home.path())
            .await
            .expect("record read should succeed");
        assert!(record.is_none());
    }
}
