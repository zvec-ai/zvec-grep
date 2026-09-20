use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp::{
    RoleClient, RoleServer,
    service::{RxJsonRpcMessage, ServiceRole, TxJsonRpcMessage},
    transport::Transport,
};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{relay, same_daemon};
use crate::{DaemonError, DaemonStatus};

struct SendAttempt {
    message: Value,
    complete: oneshot::Sender<io::Result<()>>,
}

struct TestTransport<R: ServiceRole> {
    incoming: mpsc::UnboundedReceiver<RxJsonRpcMessage<R>>,
    outgoing: mpsc::UnboundedSender<SendAttempt>,
    closed: Arc<AtomicBool>,
    delay_first_send: bool,
}

struct Peer<R: ServiceRole> {
    incoming: mpsc::UnboundedSender<RxJsonRpcMessage<R>>,
    outgoing: mpsc::UnboundedReceiver<SendAttempt>,
    closed: Arc<AtomicBool>,
}

impl<R: ServiceRole> Peer<R> {
    fn send(&self, message: Value) {
        self.incoming
            .send(serde_json::from_value(message).expect("valid MCP message"))
            .expect("relay is receiving");
    }

    async fn receive(&mut self) -> SendAttempt {
        tokio::time::timeout(Duration::from_secs(5), self.outgoing.recv())
            .await
            .expect("relay should forward message")
            .expect("relay is sending")
    }
}

impl<R: ServiceRole> Transport<R> for TestTransport<R> {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<R>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let outgoing = self.outgoing.clone();
        let delay = std::mem::take(&mut self.delay_first_send);
        async move {
            // These tests use a current-thread runtime with preloaded input.
            // Yield the first send so independently spawned later sends can
            // overtake it, exposing the old relay's ordering bug.
            if delay {
                tokio::task::yield_now().await;
            }
            let (complete, completed) = oneshot::channel();
            outgoing
                .send(SendAttempt {
                    message: serde_json::to_value(item).expect("serialize MCP message"),
                    complete,
                })
                .map_err(|_| io::Error::other("peer closed"))?;
            completed
                .await
                .map_err(|_| io::Error::other("peer dropped send"))?
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<R>> {
        self.incoming.recv().await
    }

    fn close(&mut self) -> impl Future<Output = io::Result<()>> {
        self.closed.store(true, Ordering::SeqCst);
        std::future::ready(Ok(()))
    }
}

fn transport<R: ServiceRole>() -> (TestTransport<R>, Peer<R>) {
    let (incoming, receiver) = mpsc::unbounded_channel();
    let (sender, outgoing) = mpsc::unbounded_channel();
    let closed = Arc::new(AtomicBool::new(false));
    (
        TestTransport {
            incoming: receiver,
            outgoing: sender,
            closed: Arc::clone(&closed),
            delay_first_send: false,
        },
        Peer {
            incoming,
            outgoing,
            closed,
        },
    )
}

fn start_relay(
    downstream: TestTransport<RoleServer>,
    upstream: TestTransport<RoleClient>,
) -> JoinHandle<Result<(), DaemonError>> {
    tokio::spawn(async move {
        let home = tempfile::tempdir().expect("daemon home");
        let connected = status(10, "http://127.0.0.1:7999/mcp");
        relay(downstream, upstream, &connected, home.path()).await
    })
}

async fn finish_relay(task: JoinHandle<Result<(), DaemonError>>) -> Result<(), DaemonError> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("relay must terminate")
        .expect("relay task")
}

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
fn daemon_identity_requires_the_same_running_process_and_url() {
    let connected = status(10, "http://127.0.0.1:7999/mcp");
    assert!(same_daemon(&connected, &connected));
    let mut busy = connected.clone();
    busy.ready = false;
    assert!(same_daemon(&connected, &busy));
    assert!(!same_daemon(
        &connected,
        &status(10, "http://127.0.0.1:8000/mcp")
    ));
    assert!(!same_daemon(
        &connected,
        &status(11, "http://127.0.0.1:7999/mcp")
    ));

    let mut stopped = connected.clone();
    stopped.running = false;
    stopped.ready = false;
    assert!(!same_daemon(&connected, &stopped));
}

#[tokio::test]
async fn health_probe_timeout_does_not_interrupt_an_inflight_tool() {
    use tokio::{io::AsyncReadExt, net::TcpListener};

    let home = tempfile::tempdir().expect("daemon home");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("health listener");
    let config = crate::ServerConfig::new(
        listener
            .local_addr()
            .expect("health address")
            .to_string()
            .parse()
            .expect("listen address"),
        home.path().to_owned(),
    );
    let mut instance = crate::controller::InstanceLock::acquire(&config)
        .await
        .expect("instance record");
    instance.mark_ready().await.expect("ready instance");
    let connected = status(std::process::id(), &config.listen.server_url());
    let (downstream, mut client) = transport();
    let (upstream, mut daemon) = transport();
    let task =
        tokio::spawn(async move { relay(downstream, upstream, &connected, &config.home).await });
    client.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "index", "arguments": {}}
    }));
    daemon
        .receive()
        .await
        .complete
        .send(Ok(()))
        .expect("request sent");

    // Keep MCP open while the independent health connection gets no response.
    // Wait for its client-side timeout to close the socket, not a guessed delay.
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut stream, _) = listener.accept().await.expect("health probe");
        let mut request = Vec::new();
        stream
            .read_to_end(&mut request)
            .await
            .expect("probe timed out");
        assert!(request.starts_with(b"GET /healthz "));
    })
    .await
    .expect("health probe must time out");
    assert!(
        !client.closed.load(Ordering::SeqCst),
        "a slow health endpoint must not close stdio"
    );
    daemon.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": []}}));
    let response = client.receive().await;
    assert_eq!(response.message["id"], 1);
    response.complete.send(Ok(())).expect("tool response sent");
    drop(client.incoming);
    finish_relay(task).await.expect("stdio closes normally");
    instance.release().await.expect("release instance");
}

#[tokio::test]
async fn relay_preserves_initialized_and_response_order() {
    let (mut downstream, mut client) = transport();
    let (mut upstream, mut daemon) = transport();
    downstream.delay_first_send = true;
    upstream.delay_first_send = true;
    client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    client.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "search", "arguments": {}}
    }));
    daemon.send(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    daemon.send(json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    let task = start_relay(downstream, upstream);

    let initialized = daemon.receive().await;
    assert_eq!(initialized.message["method"], "notifications/initialized");
    initialized.complete.send(Ok(())).expect("complete send");
    let request = daemon.receive().await;
    assert_eq!(request.message["method"], "tools/call");
    request.complete.send(Ok(())).expect("complete send");

    let first_response = client.receive().await;
    assert_eq!(first_response.message["id"], 1);
    first_response.complete.send(Ok(())).expect("complete send");
    let second_response = client.receive().await;
    assert_eq!(second_response.message["id"], 2);
    second_response
        .complete
        .send(Ok(()))
        .expect("complete send");
    drop(client.incoming);
    finish_relay(task).await.expect("stdio closes normally");
}

#[tokio::test]
async fn relay_forwards_elicitation_while_request_send_is_pending() {
    let (downstream, mut client) = transport();
    let (upstream, mut daemon) = transport();
    let task = start_relay(downstream, upstream);
    client.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "search", "arguments": {}}
    }));
    let request = daemon.receive().await;

    daemon.send(json!({
        "jsonrpc": "2.0", "id": 10, "method": "elicitation/create",
        "params": {"message": "Allow remote embedding?", "requestedSchema": {
            "type": "object", "properties": {}
        }}
    }));
    let consent = client.receive().await;
    assert_eq!(consent.message["method"], "elicitation/create");
    // The HTTP send completes once the SSE stream is established, while the
    // tool itself still awaits this consent response.
    request.complete.send(Ok(())).expect("request sent");
    client.send(json!({"jsonrpc": "2.0", "id": 10, "result": {"action": "accept"}}));
    let consent_reply = daemon.receive().await;
    assert_eq!(consent_reply.message["id"], 10);
    assert_eq!(consent_reply.message["result"]["action"], "accept");
    consent_reply.complete.send(Ok(())).expect("consent sent");
    consent.complete.send(Ok(())).expect("elicitation sent");

    daemon.send(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": []}}));
    let response = client.receive().await;
    assert_eq!(response.message["id"], 1);
    response.complete.send(Ok(())).expect("tool response sent");
    drop(client.incoming);
    finish_relay(task).await.expect("stdio closes normally");
}

#[tokio::test]
async fn stdio_eof_cancels_pending_sends_and_closes_both_transports() {
    let (downstream, client) = transport();
    let (upstream, mut daemon) = transport();
    let task = start_relay(downstream, upstream);
    client.send(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
    let pending = daemon.receive().await;
    drop(client.incoming);

    finish_relay(task).await.expect("stdio closes normally");
    assert!(pending.complete.is_closed());
    assert!(client.closed.load(Ordering::SeqCst));
    assert!(daemon.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn daemon_eof_is_an_error_and_closes_both_transports() {
    let (downstream, client) = transport();
    let (upstream, daemon) = transport();
    let task = start_relay(downstream, upstream);
    drop(daemon.incoming);

    let error = finish_relay(task).await.expect_err("daemon disconnected");
    assert!(error.to_string().contains("daemon MCP transport closed"));
    assert!(client.closed.load(Ordering::SeqCst));
    assert!(daemon.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn send_failure_is_reported_and_closes_both_transports() {
    let (downstream, client) = transport();
    let (upstream, mut daemon) = transport();
    let task = start_relay(downstream, upstream);
    client.send(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
    daemon
        .receive()
        .await
        .complete
        .send(Err(io::Error::other("fixture send failed")))
        .expect("fail send");

    let error = finish_relay(task).await.expect_err("send failed");
    assert!(error.to_string().contains("sending MCP request to daemon"));
    assert!(error.to_string().contains("fixture send failed"));
    assert!(client.closed.load(Ordering::SeqCst));
    assert!(daemon.closed.load(Ordering::SeqCst));
}
