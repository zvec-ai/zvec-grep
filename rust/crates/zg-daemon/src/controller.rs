use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
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
const DAEMON_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Eq, PartialEq)]
enum StopMode {
    Graceful,
    Terminate,
}

struct DaemonProcess {
    pid: u32,
    started_at: u64,
}

impl DaemonProcess {
    fn from_record(record: &DaemonInstanceRecord) -> Result<Option<Self>, DaemonError> {
        let Some(started_at) = with_process(record.pid, sysinfo::Process::start_time) else {
            return Ok(None);
        };
        // The record timestamps lock acquisition, after process creation. A
        // newer OS start time proves PID reuse; zero means identity is unknown.
        // Second-resolution times alone cannot prove ownership, so automatic
        // migration never falls back to process signals, even after this check.
        if started_at == 0 || started_at > record.started_at / 1_000 {
            return Err(DaemonError::ProcessIdentityMismatch { pid: record.pid });
        }
        Ok(Some(Self {
            pid: record.pid,
            started_at,
        }))
    }

    fn is_alive(&self) -> bool {
        with_process(self.pid, sysinfo::Process::start_time) == Some(self.started_at)
    }

    fn signal(&self, signal: Signal) -> Result<(), DaemonError> {
        with_process(self.pid, |process| {
            if process.start_time() != self.started_at {
                return Err(DaemonError::ProcessIdentityMismatch { pid: self.pid });
            }
            process.kill_with(signal).unwrap_or_else(|| process.kill());
            Ok(())
        })
        .unwrap_or(Ok(()))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DaemonInstanceRecord {
    pid: u32,
    hostname: String,
    instance_token: Uuid,
    started_at: u64,
    updated_at: u64,
    server_url: String,
    #[serde(default)]
    listen: String,
    ready: bool,
    #[serde(default = "legacy_full_toolset")]
    mcp_toolset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daemon_protocol_version: Option<u32>,
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
    _held: HeldToken,
}

impl StartupLock {
    async fn acquire(home: &Path) -> Result<Self, DaemonError> {
        let daemon_dir = daemon_dir(home);
        create_private_dir(&daemon_dir)?;
        let path = daemon_dir.join(STARTUP_FILE);
        // A competing bootstrap may be draining a legacy daemon before spawning.
        let deadline = tokio::time::Instant::now()
            + crate::default_stop_timeout()
            + SIGNAL_GRACE * 2
            + START_TIMEOUT;
        loop {
            let record = StartupRecord {
                pid: std::process::id(),
                hostname: hostname(),
                token: Uuid::new_v4(),
            };
            // Register before publishing so a contender in this process never
            // sees our record without its token.
            let held = HeldToken::new(record.token);
            if try_create_startup_record(&path, &record)? {
                return Ok(Self {
                    path,
                    record,
                    _held: held,
                });
            }
            let existing = match read_startup_record(&path).await {
                Ok(existing) => existing,
                Err(DaemonError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if record_owner_alive(&existing.hostname, existing.pid, existing.token) {
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
    _held: HeldToken,
}

impl InstanceLock {
    pub(crate) async fn acquire(config: &ServerConfig) -> Result<Self, DaemonError> {
        let daemon_dir = daemon_dir(&config.home);
        create_private_dir(&daemon_dir)?;
        let path = daemon_dir.join(INSTANCE_FILE);
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
            mcp_toolset: config.mcp_toolset.unwrap_or_default().to_string(),
            daemon_protocol_version: Some(DAEMON_PROTOCOL_VERSION),
        };
        // Register before publishing so a contender in this process never
        // sees our record without its token; dropping the guard on failure
        // or cancellation unregisters it.
        let held = HeldToken::new(record.instance_token);
        let candidate =
            path.with_file_name(format!("{INSTANCE_FILE}.{}.tmp", record.instance_token));
        write_instance_record_file(&candidate, &record)?;
        let result = acquire_instance_record(&path, &candidate).await;
        let _ = remove_file_if_exists(&candidate).await;
        result.map(|()| Self {
            path,
            record,
            _held: held,
        })
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
        replace_instance_record(&self.path, &self.record).await?;
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

async fn acquire_instance_record(path: &Path, candidate: &Path) -> Result<(), DaemonError> {
    for _ in 0..3 {
        match std::fs::hard_link(candidate, path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                match read_instance_record_path(path).await {
                    Ok(Some(existing))
                        if record_owner_alive(
                            &existing.hostname,
                            existing.pid,
                            existing.instance_token,
                        ) =>
                    {
                        return Err(DaemonError::AlreadyRunning { pid: existing.pid });
                    }
                    Ok(_) | Err(DaemonError::InvalidRecord(_)) => {
                        remove_file_if_exists(path).await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(DaemonError::InvalidRecord(path.to_owned()))
}

fn write_instance_record_file(
    path: &Path,
    record: &DaemonInstanceRecord,
) -> Result<(), DaemonError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    set_private_file(&file)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

async fn replace_instance_record(
    path: &Path,
    record: &DaemonInstanceRecord,
) -> Result<(), DaemonError> {
    const RETRY_DELAYS: [Duration; 7] = [
        Duration::from_millis(5),
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(40),
        Duration::from_millis(80),
        Duration::from_millis(160),
        Duration::from_millis(320),
    ];

    let candidate = path.with_file_name(format!("{INSTANCE_FILE}.{}.tmp", Uuid::new_v4()));
    write_instance_record_file(&candidate, record)?;
    for delay in RETRY_DELAYS {
        match std::fs::rename(&candidate, path) {
            Ok(()) => return Ok(()),
            Err(error) if retryable_replace_error(&error) => {
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                let _ = std::fs::remove_file(candidate);
                return Err(error.into());
            }
        }
    }
    let result = std::fs::rename(&candidate, path).map_err(DaemonError::from);
    if result.is_err() {
        let _ = std::fs::remove_file(candidate);
    }
    result
}

fn retryable_replace_error(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AlreadyExists
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(16) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(32 | 33)) {
        return true;
    }
    false
}

/// Starts the configured daemon process or returns the existing ready process.
/// A legacy daemon without a protocol version is stopped before replacement.
///
/// # Errors
///
/// Returns process, state-record, listen-address, or startup timeout failures.
pub async fn start_server(
    executable: &Path,
    config: &ServerConfig,
) -> Result<DaemonStatus, DaemonError> {
    match existing_server(config).await {
        Ok(Some(current)) => return Ok(current),
        Ok(None) | Err(DaemonError::LegacyDaemon { .. }) => {}
        Err(error) => return Err(error),
    }

    // The status check above is intentionally only a fast path. This
    // cross-process claim closes the check/spawn race for concurrent stdio
    // bootstrap processes. The winner alone may spawn; waiters re-check state
    // after the claim is released and reuse the ready daemon. Legacy shutdown
    // also runs under this claim so only its winner may replace the old process.
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
        if let Some(requested_toolset) = config.mcp_toolset
            && current.mcp_toolset.as_deref() != Some(requested_toolset.to_string().as_str())
        {
            return Err(DaemonError::ToolsetMismatch {
                active: current.mcp_toolset.unwrap_or_else(|| "unknown".to_owned()),
            });
        }
        if current.ready {
            return Ok(Some(current));
        }
        return wait_until_ready(&config.home, None).await.map(Some);
    }
    Ok(None)
}

async fn start_server_with_lock(
    executable: &Path,
    config: &ServerConfig,
) -> Result<DaemonStatus, DaemonError> {
    let token = crate::resolve_token(config.token_file.as_deref())?;
    if let Some(record) = read_running_instance(&config.home).await?
        && record.daemon_protocol_version.is_none()
    {
        // A different target can be checked while the legacy service is still
        // available. Its own listening address must be released before binding.
        if record_address(&record, &config.home).await? != config.listen {
            assert_address_available(&config.listen).await?;
        }
        stop_recorded_server(
            &config.home,
            &record,
            crate::default_stop_timeout(),
            token.as_deref(),
            StopMode::Graceful,
        )
        .await?;
    }
    if let Some(current) = existing_server(config).await? {
        return Ok(current);
    }

    assert_address_available(&config.listen).await?;
    let daemon_dir = daemon_dir(&config.home);
    create_private_dir(&daemon_dir)?;
    let log_path = daemon_dir.join("bootstrap.log");
    let stdout = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;
    set_private_file(&stdout)?;
    let stderr = stdout.try_clone()?;
    let mut command = Command::new(executable);
    command
        .arg("--server")
        .arg("run")
        .arg("--mcp-toolset")
        .arg(config.mcp_toolset.unwrap_or_default().to_string())
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
    wait_until_ready(&config.home, Some((child, log_path))).await
}

/// Requests graceful shutdown using an explicitly selected token file and falls
/// back to verified process termination. Automatic migration uses graceful
/// shutdown only and never sends process signals.
///
/// # Errors
///
/// Returns credential, state-record, HTTP control, PID identity, signal, or timeout failures.
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
    stop_recorded_server(
        home,
        &record,
        timeout,
        token.as_deref(),
        StopMode::Terminate,
    )
    .await
}

async fn stop_recorded_server(
    home: &Path,
    record: &DaemonInstanceRecord,
    timeout: Duration,
    token: Option<&str>,
    mode: StopMode,
) -> Result<DaemonStatus, DaemonError> {
    if record.pid == std::process::id() {
        return Err(DaemonError::RefuseCurrentProcess);
    }
    let Some(process) = DaemonProcess::from_record(record)? else {
        remove_record_if_same(home, record).await?;
        return Ok(DaemonStatus::default());
    };

    assert_same_instance(home, record, false).await?;
    let address = record_address(record, home).await?;
    let accepted =
        match http_request(&address, "POST", "/control/shutdown", HTTP_TIMEOUT, token).await {
            Ok(202) => true,
            // An HTTP rejection, including failed authentication, must not be
            // bypassed by falling back to process signals.
            Ok(status) => return Err(DaemonError::ShutdownRejected { status }),
            Err(error) if mode == StopMode::Graceful => return Err(error),
            Err(_) => false,
        };
    if accepted && wait_for_exit(&process, timeout).await {
        remove_record_if_same(home, record).await?;
        return Ok(DaemonStatus::default());
    }
    if mode == StopMode::Graceful {
        return Err(DaemonError::Timeout { action: "stop" });
    }

    assert_same_instance(home, record, accepted).await?;
    process.signal(Signal::Term)?;
    if wait_for_exit(&process, SIGNAL_GRACE.min(timeout)).await {
        remove_record_if_same(home, record).await?;
        return Ok(DaemonStatus::default());
    }
    assert_same_instance(home, record, true).await?;
    process.signal(Signal::Kill)?;
    if !wait_for_exit(&process, SIGNAL_GRACE).await {
        return Err(DaemonError::Timeout { action: "stop" });
    }
    remove_record_if_same(home, record).await?;
    Ok(DaemonStatus::default())
}

/// Returns current daemon status for a home directory.
///
/// # Errors
///
/// Returns an I/O error when the instance record or health endpoint cannot be
/// inspected safely, or when a running daemon uses an incompatible protocol.
pub async fn server_status(home: &Path) -> Result<DaemonStatus, DaemonError> {
    let Some(record) = read_running_instance(home).await? else {
        return Ok(DaemonStatus::default());
    };
    match record.daemon_protocol_version {
        Some(DAEMON_PROTOCOL_VERSION) => {}
        Some(actual) => {
            return Err(DaemonError::ProtocolMismatch {
                pid: record.pid,
                actual,
                expected: DAEMON_PROTOCOL_VERSION,
            });
        }
        None => return Err(DaemonError::LegacyDaemon { pid: record.pid }),
    }
    let address = record_address(&record, home).await?;
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

async fn read_running_instance(home: &Path) -> Result<Option<DaemonInstanceRecord>, DaemonError> {
    Ok(read_instance_record(home)
        .await?
        .filter(|record| record.hostname == hostname() && process_is_alive(record.pid)))
}

async fn read_instance_record(home: &Path) -> Result<Option<DaemonInstanceRecord>, DaemonError> {
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

async fn wait_until_ready(
    home: &Path,
    mut child: Option<(DaemonChild, PathBuf)>,
) -> Result<DaemonStatus, DaemonError> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        let status = server_status(home).await?;
        if status.ready {
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
            return Err(DaemonError::Timeout { action: "start" });
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

async fn wait_for_exit(process: &DaemonProcess, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process.is_alive() {
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

pub(crate) fn process_is_alive(pid: u32) -> bool {
    with_process(pid, |_| ()).is_some()
}

/// Tokens of the startup and instance locks this process currently holds.
static HELD_TOKENS: Mutex<BTreeSet<Uuid>> = Mutex::new(BTreeSet::new());

fn held_tokens() -> MutexGuard<'static, BTreeSet<Uuid>> {
    HELD_TOKENS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps a lock token registered in [`HELD_TOKENS`] until dropped.
struct HeldToken(Uuid);

impl HeldToken {
    fn new(token: Uuid) -> Self {
        held_tokens().insert(token);
        Self(token)
    }
}

impl Drop for HeldToken {
    fn drop(&mut self) {
        held_tokens().remove(&self.0);
    }
}

/// Reports whether a lock record still belongs to a live owner. A record
/// naming the current PID is only live if this process holds that token:
/// containers restart the daemon as PID 1 with the same hostname, so a
/// crashed daemon's record can name the new process without being its lock.
fn record_owner_alive(host: &str, pid: u32, token: Uuid) -> bool {
    if host != hostname() {
        return false;
    }
    if pid == std::process::id() {
        return held_tokens().contains(&token);
    }
    process_is_alive(pid)
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

async fn record_address(
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
    let address = value
        .parse::<ListenAddress>()
        .map_err(|_| DaemonError::InvalidRecord(instance_path(home)))?;
    if !value
        .split_once(':')
        .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost"))
    {
        return Ok(address);
    }

    // Node.js records the requested hostname, which may resolve to IPv6 even
    // though Rust's listen configuration defaults localhost to IPv4. Probe both
    // loopback families without credentials or side effects. Never send shutdown
    // to an arbitrary first responder when both endpoints appear healthy.
    let ipv6 = format!("[::1]:{}", address.socket_addr().port()).parse::<ListenAddress>()?;
    let healthy = async |candidate: &ListenAddress| {
        http_request(candidate, "GET", "/healthz", HTTP_TIMEOUT, None)
            .await
            .is_ok_and(|status| status == 200)
    };
    let (ipv4_healthy, ipv6_healthy) = tokio::join!(healthy(&address), healthy(&ipv6));
    match (ipv4_healthy, ipv6_healthy) {
        (true, false) => Ok(address),
        (false, true) => Ok(ipv6),
        _ => Err(DaemonError::UnverifiedEndpoint {
            address: value.to_owned(),
        }),
    }
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

async fn remove_file_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        DaemonInstanceRecord, InstanceLock, STARTUP_FILE, StartupLock, StartupRecord, daemon_dir,
        hostname, instance_path, read_instance_record,
    };
    use crate::{DaemonError, ServerConfig};

    #[tokio::test]
    async fn instance_lock_replaces_an_incomplete_record_with_complete_states() {
        let home = TempDir::new().expect("temp home");
        let config = ServerConfig::new(
            "127.0.0.1:7999".parse().expect("listen address"),
            home.path().to_owned(),
        );
        let path = instance_path(home.path());
        std::fs::create_dir_all(path.parent().expect("daemon directory"))
            .expect("daemon directory");
        std::fs::write(&path, b"{").expect("incomplete record");

        let mut lock = InstanceLock::acquire(&config).await.expect("instance lock");
        let starting = read_instance_record(home.path())
            .await
            .expect("starting record")
            .expect("starting record exists");
        assert!(!starting.ready);
        assert_eq!(
            starting.daemon_protocol_version,
            Some(super::DAEMON_PROTOCOL_VERSION)
        );

        lock.mark_ready().await.expect("ready record");
        let ready = read_instance_record(home.path())
            .await
            .expect("ready record")
            .expect("ready record exists");
        assert!(ready.ready);
        assert_eq!(
            ready.daemon_protocol_version,
            starting.daemon_protocol_version
        );
        assert_eq!(ready.instance_token, starting.instance_token);
        assert!(
            std::fs::read_dir(path.parent().expect("daemon directory"))
                .expect("daemon files")
                .all(|entry| !entry
                    .expect("daemon file")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );

        lock.release().await.expect("release instance lock");
        assert!(
            read_instance_record(home.path())
                .await
                .expect("released record")
                .is_none()
        );
    }

    #[tokio::test]
    async fn instance_lock_replaces_a_record_left_under_the_current_pid() {
        // A container restart runs the new daemon with the crashed one's PID
        // and hostname, so the leftover record names this very process.
        let home = TempDir::new().expect("temp home");
        let config = ServerConfig::new(
            "127.0.0.1:7999".parse().expect("listen address"),
            home.path().to_owned(),
        );
        let path = instance_path(home.path());
        std::fs::create_dir_all(path.parent().expect("daemon directory"))
            .expect("daemon directory");
        let stale = DaemonInstanceRecord {
            pid: std::process::id(),
            hostname: hostname(),
            instance_token: uuid::Uuid::new_v4(),
            started_at: 1,
            updated_at: 1,
            server_url: "http://127.0.0.1:7999".to_owned(),
            listen: "127.0.0.1:7999".to_owned(),
            ready: true,
            mcp_toolset: "full".to_owned(),
            daemon_protocol_version: None,
        };
        std::fs::write(&path, serde_json::to_vec(&stale).expect("stale record"))
            .expect("write stale record");

        let lock = InstanceLock::acquire(&config).await.expect("instance lock");
        let current = read_instance_record(home.path())
            .await
            .expect("current record")
            .expect("current record exists");
        assert_ne!(current.instance_token, stale.instance_token);

        lock.release().await.expect("release instance lock");
    }

    #[tokio::test]
    async fn startup_lock_replaces_a_record_left_under_the_current_pid() {
        let home = TempDir::new().expect("temp home");
        let path = daemon_dir(home.path()).join(STARTUP_FILE);
        std::fs::create_dir_all(path.parent().expect("daemon directory"))
            .expect("daemon directory");
        let stale = StartupRecord {
            pid: std::process::id(),
            hostname: hostname(),
            token: uuid::Uuid::new_v4(),
        };
        std::fs::write(&path, serde_json::to_vec(&stale).expect("stale record"))
            .expect("write stale record");

        let lock = StartupLock::acquire(home.path())
            .await
            .expect("startup lock");
        assert_ne!(lock.record.token, stale.token);

        lock.release().await.expect("release startup lock");
    }

    #[tokio::test]
    async fn instance_lock_held_by_this_process_is_not_replaced() {
        let home = TempDir::new().expect("temp home");
        let config = ServerConfig::new(
            "127.0.0.1:7999".parse().expect("listen address"),
            home.path().to_owned(),
        );
        let lock = InstanceLock::acquire(&config).await.expect("instance lock");

        let second = InstanceLock::acquire(&config).await;
        assert!(
            matches!(second, Err(DaemonError::AlreadyRunning { pid }) if pid == std::process::id())
        );
        let current = read_instance_record(home.path())
            .await
            .expect("current record")
            .expect("current record exists");
        assert_eq!(current.instance_token, lock.record.instance_token);

        lock.release().await.expect("release instance lock");
    }

    #[tokio::test]
    async fn interleaved_instance_lock_acquisitions_admit_one_owner() {
        let home = TempDir::new().expect("temp home");
        let config = ServerConfig::new(
            "127.0.0.1:7999".parse().expect("listen address"),
            home.path().to_owned(),
        );

        // Poll the first acquisition once so it publishes the lock file and
        // parks on its temporary-file cleanup, then let the second one run.
        let mut first = Box::pin(InstanceLock::acquire(&config));
        let first_ready =
            std::future::poll_fn(|cx| std::task::Poll::Ready(first.as_mut().poll(cx).is_ready()))
                .await;
        assert!(
            !first_ready,
            "first acquisition should park after publishing"
        );
        assert!(instance_path(home.path()).exists());

        let second = InstanceLock::acquire(&config).await;
        assert!(
            matches!(second, Err(DaemonError::AlreadyRunning { pid }) if pid == std::process::id())
        );
        let mut lock = first.await.expect("first owner keeps the lock");
        lock.mark_ready().await.expect("owner keeps its lock");

        lock.release().await.expect("release instance lock");
    }

    #[tokio::test]
    async fn cancelled_instance_lock_acquisition_releases_its_token() {
        let home = TempDir::new().expect("temp home");
        let config = ServerConfig::new(
            "127.0.0.1:7999".parse().expect("listen address"),
            home.path().to_owned(),
        );

        // Cancel an acquisition after it publishes the lock file. Its record
        // stays behind under this PID, but the token must not stay held.
        let mut cancelled = Box::pin(InstanceLock::acquire(&config));
        let cancelled_ready = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(cancelled.as_mut().poll(cx).is_ready())
        })
        .await;
        assert!(!cancelled_ready, "acquisition should park after publishing");
        drop(cancelled);
        let leftover = read_instance_record(home.path())
            .await
            .expect("leftover record")
            .expect("leftover record exists");

        let lock = InstanceLock::acquire(&config).await.expect("instance lock");
        assert_ne!(lock.record.instance_token, leftover.instance_token);

        lock.release().await.expect("release instance lock");
    }

    #[tokio::test]
    async fn interleaved_startup_lock_acquisitions_admit_one_owner() {
        let home = TempDir::new().expect("temp home");
        let wait = Duration::from_millis(200);

        let (first, second) = tokio::join!(
            tokio::time::timeout(wait, StartupLock::acquire(home.path())),
            tokio::time::timeout(wait, StartupLock::acquire(home.path())),
        );
        let lock = match (first, second) {
            (Ok(Ok(lock)), Err(_)) | (Err(_), Ok(Ok(lock))) => lock,
            (first, second) => panic!(
                "expected exactly one owner, got {:?} and {:?}",
                first.map(|result| result.map(|_| ())),
                second.map(|result| result.map(|_| ()))
            ),
        };
        let path = daemon_dir(home.path()).join(STARTUP_FILE);
        let current: StartupRecord =
            serde_json::from_slice(&std::fs::read(&path).expect("startup record"))
                .expect("startup record json");
        assert_eq!(current, lock.record);

        lock.release().await.expect("release startup lock");
    }
}
