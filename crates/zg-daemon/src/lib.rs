//! Daemon lifecycle and loopback HTTP host.
//!
//! The daemon owns one reusable `ZvecGrep` instance. It exposes the selected agent or
//! full MCP toolset at `/mcp`, a health probe, a local shutdown endpoint, and a
//! concurrency-safe stdio bootstrap proxy.

mod authentication;
mod controller;
pub use authentication::resolve_token;
mod http_client;
mod job_scheduler;
mod runtime;
mod stdio;
mod workspace_runtime;

use std::{fmt, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

pub use controller::{
    DaemonInstanceRecord, DaemonStatus, start_server, stop_server, stop_server_with_token,
};
pub use stdio::run_stdio_bridge;
use thiserror::Error;
use zg_engine::ErrorReport;
use zg_engine::ZvecGrep;
pub use zg_transport_mcp::McpToolset;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7999";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenAddress {
    value: String,
    socket: SocketAddr,
}

impl ListenAddress {
    #[must_use]
    pub fn socket_addr(&self) -> SocketAddr {
        self.socket
    }

    #[must_use]
    pub fn server_url(&self) -> String {
        format!("http://{}/mcp", self.socket)
    }
}

impl Default for ListenAddress {
    fn default() -> Self {
        Self::from_str(DEFAULT_LISTEN).expect("the default daemon address must be valid")
    }
}

impl fmt::Display for ListenAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

impl FromStr for ListenAddress {
    type Err = DaemonError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let normalized = value
            .strip_prefix("localhost:")
            .map_or_else(|| value.to_owned(), |port| format!("127.0.0.1:{port}"));
        let socket = normalized
            .parse::<SocketAddr>()
            .map_err(|_| DaemonError::InvalidListen(value.to_owned()))?;
        if !socket.ip().is_loopback() || socket.port() == 0 {
            return Err(DaemonError::InvalidListen(value.to_owned()));
        }
        Ok(Self {
            value: socket.to_string(),
            socket,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub listen: ListenAddress,
    pub home: PathBuf,
    pub mcp_toolset: McpToolset,
    pub token_file: Option<PathBuf>,
}

impl ServerConfig {
    #[must_use]
    pub const fn new(listen: ListenAddress, home: PathBuf) -> Self {
        Self {
            listen,
            home,
            mcp_toolset: McpToolset::Agent,
            token_file: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("invalid daemon listen address {0:?}; expected a loopback host and non-zero port")]
    InvalidListen(String),
    #[error("cannot determine zvec-grep home; pass --home or set ZVEC_GREP_HOME")]
    MissingHome,
    #[error("zvec-grep server is already running with PID {pid}")]
    AlreadyRunning { pid: u32 },
    #[error(
        "zvec-grep server is already running with MCP toolset {active:?}; run `zg server off` before changing toolsets"
    )]
    ToolsetMismatch { active: String },
    #[error("server address {0} is already in use")]
    AddressInUse(String),
    #[error("timed out waiting for zvec-grep server to {action}")]
    Timeout { action: &'static str },
    #[error("daemon process exited before becoming ready; see {log_path}")]
    ChildExited { log_path: PathBuf },
    #[error("refusing to stop the current process as a daemon")]
    RefuseCurrentProcess,
    #[error("zvec-grep server instance changed while stopping; refusing to signal PID {pid}")]
    InstanceChanged { pid: u32 },
    #[error("invalid daemon instance record at {0}")]
    InvalidRecord(PathBuf),
    #[error("daemon I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon state serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daemon HTTP server failed: {0}")]
    Http(#[from] axum::Error),
    #[error("daemon MCP stdio bridge failed: {0}")]
    McpBridge(String),
    #[error("resident daemon is not ready")]
    NotReady,
    #[error("resident daemon request failed:\n{report}\nretryable: {retryable}")]
    Remote {
        report: Box<ErrorReport>,
        retryable: bool,
    },
}

/// Resolves the daemon home without changing process-global state.
///
/// # Errors
///
/// Returns [`DaemonError::MissingHome`] when neither an explicit value nor a
/// platform home environment variable is available.
pub fn resolve_home(explicit: Option<PathBuf>) -> Result<PathBuf, DaemonError> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("ZVEC_GREP_HOME") {
        return Ok(PathBuf::from(path));
    }
    let user_home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    user_home
        .map(|path| PathBuf::from(path).join(".zvec-grep"))
        .ok_or(DaemonError::MissingHome)
}

/// Returns current daemon status for a home directory.
///
/// # Errors
///
/// Returns an I/O error when the instance record or health endpoint cannot be
/// inspected safely.
pub async fn server_status(home: &std::path::Path) -> Result<DaemonStatus, DaemonError> {
    controller::server_status(home).await
}

/// Executes one typed engine command through the resident daemon's local
/// administration endpoint. The command and reply are the same DTOs used by
/// direct mode, which keeps CLI behavior independent of the MCP tool profile.
///
/// # Errors
///
/// Returns [`DaemonError`] when the daemon is unavailable, the loopback HTTP
/// exchange fails, the reply is malformed, or the remote engine call fails.
pub async fn execute_command(
    home: &std::path::Path,
    command: zg_daemon_protocol::DaemonCommand,
) -> Result<zg_daemon_protocol::DaemonReply, DaemonError> {
    let status = server_status(home).await?;
    if !status.ready {
        return Err(DaemonError::NotReady);
    }
    let mut url = status.server_url.ok_or(DaemonError::NotReady)?;
    if let Some(prefix) = url.strip_suffix("/mcp") {
        url = format!("{prefix}/admin/execute");
    } else {
        return Err(DaemonError::McpBridge(
            "daemon published an invalid server URL".to_owned(),
        ));
    }
    let body = serde_json::to_vec(&command)?;
    let reply = http_client::post_json(&url, body).await?;
    match serde_json::from_slice::<zg_daemon_protocol::ExecutionResult>(&reply)? {
        zg_daemon_protocol::ExecutionResult::Success(reply) => Ok(reply),
        zg_daemon_protocol::ExecutionResult::Failure(error) => Err(DaemonError::Remote {
            report: Box::new(error.report),
            retryable: error.retryable,
        }),
    }
}

/// Runs the resident HTTP daemon until local shutdown or an OS termination
/// signal, then releases its instance record.
///
/// # Errors
///
/// Returns lifecycle, bind, state-file, or HTTP server errors.
pub async fn run_server(config: ServerConfig, engine: Arc<ZvecGrep>) -> Result<(), DaemonError> {
    runtime::run_server(config, engine).await
}

#[must_use]
pub const fn default_stop_timeout() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::ListenAddress;

    #[test]
    fn listen_address_accepts_only_loopback() {
        assert!(ListenAddress::from_str("127.0.0.1:7999").is_ok());
        assert!(ListenAddress::from_str("localhost:7999").is_ok());
        assert!(ListenAddress::from_str("[::1]:7999").is_ok());
        assert!(ListenAddress::from_str("0.0.0.0:7999").is_err());
        assert!(ListenAddress::from_str("127.0.0.1:0").is_err());
    }
}
