use std::{collections::VecDeque, path::Path, time::Duration};

use futures::{FutureExt, future::BoxFuture};
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
    // Transport::send may only enqueue a message when its future is polled.
    // Poll one send at a time in each direction to preserve wire order, while
    // continuing to receive and to send in the opposite direction.
    let mut upstream_sends = VecDeque::new();
    let mut downstream_sends = VecDeque::new();
    let mut monitor = tokio::time::interval_at(
        tokio::time::Instant::now() + DAEMON_MONITOR_INTERVAL,
        DAEMON_MONITOR_INTERVAL,
    );
    let mut stop_check = StdioBridgeStopCheck::default();

    let relay_result = loop {
        tokio::select! {
            message = downstream.receive() => {
                let Some(message) = message else {
                    break Ok(());
                };
                upstream_sends.push_back(upstream.send(message).boxed());
            }
            message = upstream.receive() => {
                let Some(message) = message else {
                    break Err(DaemonError::McpBridge(
                        "daemon MCP transport closed while stdio was connected".to_owned(),
                    ));
                };
                downstream_sends.push_back(downstream.send(message).boxed());
            }
            result = send_next(&mut upstream_sends) => {
                if let Err(error) = result {
                    break Err(DaemonError::McpBridge(format!(
                        "sending MCP request to daemon: {error}"
                    )));
                }
            }
            result = send_next(&mut downstream_sends) => {
                if let Err(error) = result {
                    break Err(DaemonError::McpBridge(format!(
                        "sending MCP response to stdout: {error}"
                    )));
                }
            }
            _ = monitor.tick() => {
                let current = match server_status(home).await {
                    Ok(current) => current,
                    Err(error) => {
                        warn!(%error, "daemon status temporarily unavailable during MCP relay");
                        DaemonStatus::default()
                    }
                };
                if current.pid.is_none()
                    && connected.pid.is_some_and(crate::controller::process_is_alive)
                {
                    continue;
                }
                if stop_check.should_stop(connected, &current) {
                    break Err(DaemonError::McpBridge(
                        "daemon stopped or changed while stdio was connected".to_owned(),
                    ));
                }
            }
        }
    };

    drop(upstream_sends);
    drop(downstream_sends);
    let (downstream_close, upstream_close) = tokio::join!(downstream.close(), upstream.close());
    if let Err(error) = downstream_close {
        warn!(%error, "failed to close MCP stdio transport");
    }
    if let Err(error) = upstream_close {
        warn!(%error, "failed to close daemon MCP transport");
    }
    relay_result
}

async fn send_next<E>(sends: &mut VecDeque<BoxFuture<'static, Result<(), E>>>) -> Result<(), E> {
    let Some(send) = sends.front_mut() else {
        return std::future::pending().await;
    };
    let result = send.await;
    sends.pop_front();
    result
}

#[derive(Default)]
struct StdioBridgeStopCheck {
    consecutive_missing: u8,
}

impl StdioBridgeStopCheck {
    fn should_stop(&mut self, connected: &DaemonStatus, current: &DaemonStatus) -> bool {
        if !current.running {
            self.consecutive_missing = self.consecutive_missing.saturating_add(1);
            return self.consecutive_missing >= 3;
        }
        self.consecutive_missing = 0;
        !same_daemon(connected, current)
    }
}

fn same_daemon(connected: &DaemonStatus, current: &DaemonStatus) -> bool {
    // Startup checks readiness before opening MCP. Once connected, a slow health
    // probe must not tear down in-flight tools while the same process is alive.
    current.running && current.pid == connected.pid && current.server_url == connected.server_url
}

#[cfg(test)]
mod tests;
