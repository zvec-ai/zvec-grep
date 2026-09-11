use std::{path::Path, time::Duration};

use rmcp::{
    RoleClient, RoleServer,
    transport::{
        Transport,
        async_rw::AsyncRwTransport,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use tokio::task::JoinSet;
use tracing::warn;

use crate::{
    DaemonError, DaemonStatus, ServerConfig, http_client::LoopbackHttpClient, server_status,
    start_server,
};

const DAEMON_MONITOR_INTERVAL: Duration = Duration::from_secs(2);

/// Starts or reuses the resident daemon, then transparently relays MCP between
/// stdin/stdout and the daemon's Streamable HTTP endpoint.
///
/// The daemon remains resident when stdin closes. Process startup is serialized
/// by [`start_server`], so many MCP clients may bootstrap concurrently without
/// spawning competing daemon processes.
///
/// # Errors
///
/// Returns daemon startup, health-monitoring, HTTP transport, or stdio relay
/// failures.
pub async fn run_stdio_bridge(executable: &Path, config: &ServerConfig) -> Result<(), DaemonError> {
    let connected = start_server(executable, config).await?;
    let server_url = connected.server_url.clone().ok_or_else(|| {
        DaemonError::McpBridge("ready daemon did not publish an MCP URL".to_owned())
    })?;

    let downstream =
        AsyncRwTransport::<RoleServer, _, _>::new_server(tokio::io::stdin(), tokio::io::stdout());
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(server_url);
    if let Some(token) = crate::resolve_token(config.token_file.as_deref())? {
        transport_config = transport_config.auth_header(token);
    }
    let upstream = StreamableHttpClientTransport::with_client(LoopbackHttpClient, transport_config);
    relay(downstream, upstream, &connected, &config.home).await
}

async fn relay<Downstream, Upstream>(
    mut downstream: Downstream,
    mut upstream: Upstream,
    connected: &DaemonStatus,
    home: &Path,
) -> Result<(), DaemonError>
where
    Downstream: Transport<RoleServer>,
    Upstream: Transport<RoleClient>,
{
    let mut sends = JoinSet::new();
    let mut monitor = tokio::time::interval_at(
        tokio::time::Instant::now() + DAEMON_MONITOR_INTERVAL,
        DAEMON_MONITOR_INTERVAL,
    );

    let relay_result = loop {
        tokio::select! {
            message = downstream.receive() => {
                let Some(message) = message else {
                    break Ok(());
                };
                let send = upstream.send(message);
                sends.spawn(async move {
                    send.await.map_err(|error| format!("sending MCP request to daemon: {error}"))
                });
            }
            message = upstream.receive() => {
                let Some(message) = message else {
                    break Err(DaemonError::McpBridge(
                        "daemon MCP transport closed while stdio was connected".to_owned(),
                    ));
                };
                let send = downstream.send(message);
                sends.spawn(async move {
                    send.await.map_err(|error| format!("sending MCP response to stdout: {error}"))
                });
            }
            completed = sends.join_next(), if !sends.is_empty() => {
                match completed {
                    Some(Ok(Err(error))) => break Err(DaemonError::McpBridge(error)),
                    Some(Err(error)) => {
                        break Err(DaemonError::McpBridge(format!(
                            "MCP relay send task failed: {error}"
                        )));
                    }
                    Some(Ok(Ok(()))) | None => {}
                }
            }
            _ = monitor.tick() => {
                let current = server_status(home).await?;
                if !same_daemon(connected, &current) {
                    break Err(DaemonError::McpBridge(
                        "daemon stopped or changed while stdio was connected".to_owned(),
                    ));
                }
            }
        }
    };

    sends.abort_all();
    while sends.join_next().await.is_some() {}
    let (downstream_close, upstream_close) = tokio::join!(downstream.close(), upstream.close());
    if let Err(error) = downstream_close {
        warn!(%error, "failed to close MCP stdio transport");
    }
    if let Err(error) = upstream_close {
        warn!(%error, "failed to close daemon MCP transport");
    }
    relay_result
}

fn same_daemon(connected: &DaemonStatus, current: &DaemonStatus) -> bool {
    current.running
        && current.ready
        && current.pid == connected.pid
        && current.server_url == connected.server_url
}

#[cfg(test)]
mod tests {
    use super::same_daemon;
    use crate::DaemonStatus;

    fn status(pid: u32, url: &str) -> DaemonStatus {
        DaemonStatus {
            running: true,
            ready: true,
            pid: Some(pid),
            server_url: Some(url.to_owned()),
            mcp_toolset: Some("agent".to_owned()),
        }
    }

    #[test]
    fn daemon_identity_requires_the_same_ready_process_and_url() {
        let connected = status(10, "http://127.0.0.1:7999/mcp");
        assert!(same_daemon(&connected, &connected));
        assert!(!same_daemon(
            &connected,
            &status(11, "http://127.0.0.1:7999/mcp")
        ));

        let mut stopped = connected.clone();
        stopped.running = false;
        stopped.ready = false;
        assert!(!same_daemon(&connected, &stopped));
    }
}
