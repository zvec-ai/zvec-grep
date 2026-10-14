//! Bounded legacy HTTP sessions with request-aware idle expiry.

use futures::{Stream, StreamExt};
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::streamable_http_server::session::{
        ServerSseMessage, SessionId, SessionManager, local::LocalSessionManager,
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("legacy MCP session capacity reached")]
    Capacity,
    #[error("MCP session not found")]
    Missing,
    #[error(transparent)]
    Local(
        #[from] rmcp::transport::streamable_http_server::session::local::LocalSessionManagerError,
    ),
}

struct Activity {
    touched: Instant,
    active: usize,
}
type Activities = Arc<Mutex<HashMap<SessionId, Activity>>>;

pub(crate) struct BoundedSessionManager {
    inner: LocalSessionManager,
    activities: Activities,
    admission: tokio::sync::Mutex<()>,
    capacity: usize,
    idle: Duration,
}

impl Default for BoundedSessionManager {
    fn default() -> Self {
        Self::new(256, Duration::from_mins(30))
    }
}

impl BoundedSessionManager {
    fn new(capacity: usize, idle: Duration) -> Self {
        let mut inner = LocalSessionManager::default();
        // This manager owns request-aware expiry; rmcp's independent timer also
        // expires sessions with an active request waiting for a response.
        inner.session_config.keep_alive = None;
        Self {
            inner,
            activities: Arc::default(),
            admission: tokio::sync::Mutex::new(()),
            capacity,
            idle,
        }
    }

    async fn expire(&self) -> Result<(), SessionError> {
        let expired = {
            let mut entries = self.activities.lock().expect("session activity lock");
            let ids: Vec<_> = entries
                .iter()
                .filter(|(_, activity)| {
                    activity.active == 0 && activity.touched.elapsed() >= self.idle
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                entries.remove(id);
            }
            ids
        };
        for id in expired {
            self.inner.close_session(&id).await?;
        }
        Ok(())
    }

    fn enter(&self, id: &SessionId) -> Result<ActivityGuard, SessionError> {
        let mut entries = self.activities.lock().expect("session activity lock");
        let activity = entries.get_mut(id).ok_or(SessionError::Missing)?;
        if activity.active == 0 && activity.touched.elapsed() >= self.idle {
            return Err(SessionError::Missing);
        }
        activity.active += 1;
        activity.touched = Instant::now();
        Ok(ActivityGuard {
            id: id.clone(),
            entries: Arc::clone(&self.activities),
        })
    }
}

struct ActivityGuard {
    id: SessionId,
    entries: Activities,
}
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if let Some(activity) = self
            .entries
            .lock()
            .expect("session activity lock")
            .get_mut(&self.id)
        {
            activity.active -= 1;
            activity.touched = Instant::now();
        }
    }
}

fn guarded<S: Stream<Item = ServerSseMessage> + Send + Sync + 'static>(
    stream: S,
    guard: Option<ActivityGuard>,
) -> impl Stream<Item = ServerSseMessage> + Send + Sync + 'static {
    futures::stream::unfold(
        (Box::pin(stream), guard),
        |(mut stream, guard)| async move {
            stream
                .next()
                .await
                .map(|message| (message, (stream, guard)))
        },
    )
}

impl SessionManager for BoundedSessionManager {
    type Error = SessionError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        // Serialize admission across the async creation to enforce the hard cap.
        let _admission = self.admission.lock().await;
        self.expire().await?;
        if self.activities.lock().expect("session activity lock").len() >= self.capacity {
            return Err(SessionError::Capacity);
        }
        let (id, transport) = self.inner.create_session().await?;
        self.activities
            .lock()
            .expect("session activity lock")
            .insert(
                id.clone(),
                Activity {
                    touched: Instant::now(),
                    active: 0,
                },
            );
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        let _guard = self.enter(id)?;
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.expire().await?;
        Ok(self
            .activities
            .lock()
            .expect("session activity lock")
            .contains_key(id))
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.activities
            .lock()
            .expect("session activity lock")
            .remove(id);
        Ok(self.inner.close_session(id).await?)
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        let guard = self.enter(id)?;
        Ok(guarded(
            self.inner.create_stream(id, message).await?,
            Some(guard),
        ))
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        let _guard = self.enter(id)?;
        Ok(self.inner.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        // An idle subscription alone must not keep an unused session alive.
        let _guard = self.enter(id)?;
        Ok(self.inner.create_standalone_stream(id).await?)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        let guard = self.enter(id)?;
        // Local event IDs include a request suffix only for request-bound streams.
        let request_stream = last_event_id.contains('/');
        let stream = self.inner.resume(id, last_event_id).await?;
        Ok(guarded(stream, request_stream.then_some(guard)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::{Transport, streamable_http_server::session::local::SessionTransport};

    #[tokio::test(start_paused = true)]
    async fn bounds_sessions_and_expires_only_idle_work() {
        let manager = BoundedSessionManager::new(2, Duration::from_secs(60));
        let (first, _transport1) = manager.create_session().await.expect("first");
        let (second, _transport2) = manager.create_session().await.expect("second");
        assert!(matches!(
            manager.create_session().await,
            Err(SessionError::Capacity)
        ));
        let active = manager.enter(&first).expect("active request");
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(manager.has_session(&first).await.expect("active survives"));
        assert!(!manager.has_session(&second).await.expect("idle expires"));
        let (_third, _transport3) = manager.create_session().await.expect("capacity recovered");
        drop(active);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(manager.has_session(&first).await.expect("fresh idle clock"));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            !manager
                .has_session(&first)
                .await
                .expect("expires after completion")
        );
    }

    async fn initialized_session(manager: &BoundedSessionManager) -> (SessionId, SessionTransport) {
        let (id, mut transport) = manager.create_session().await.expect("first session");
        let initialize = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        }))
        .expect("initialize request");
        let (initialized, ()) = tokio::join!(manager.initialize_session(&id, initialize), async {
            transport
                .receive()
                .await
                .expect("worker receives initialization");
            let response = serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "test", "version": "1.0"}
                }
            }))
            .expect("initialize response");
            transport
                .send(response)
                .await
                .expect("worker sends initialization");
        });
        initialized.expect("session initialized");
        (id, transport)
    }

    fn tools_list_request() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .expect("tools/list request")
    }

    async fn complete_tools_list(
        transport: &mut SessionTransport,
        stream: impl Stream<Item = ServerSseMessage>,
    ) {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "result": {"tools": []}
        });
        transport
            .send(serde_json::from_value(response.clone()).expect("tools/list response"))
            .await
            .expect("session worker sends response");
        let messages: Vec<_> = stream
            .filter_map(|event| async move { event.message })
            .collect()
            .await;
        assert_eq!(messages.len(), 1);
        assert_eq!(
            serde_json::to_value(messages[0].as_ref()).expect("response JSON"),
            response
        );
    }

    #[tokio::test(start_paused = true)]
    async fn default_session_serves_requests_after_rmcp_idle_timeout() {
        let manager = BoundedSessionManager::default();
        let (id, mut transport) = initialized_session(&manager).await;
        let _subscription = manager
            .create_standalone_stream(&id)
            .await
            .expect("notification subscription");

        tokio::time::sleep(Duration::from_mins(29)).await;
        assert!(manager.has_session(&id).await.expect("session still live"));
        let stream = manager
            .create_stream(&id, tools_list_request())
            .await
            .expect("same session accepts a request after five minutes");
        assert_eq!(
            serde_json::to_value(transport.receive().await.expect("worker receives request"))
                .expect("received request JSON"),
            serde_json::to_value(tools_list_request()).expect("expected request JSON")
        );
        complete_tools_list(&mut transport, stream).await;
        manager.close_session(&id).await.expect("close session");
    }

    #[tokio::test(start_paused = true)]
    async fn default_idle_expiry_closes_inner_session_with_notification_subscription() {
        let manager = BoundedSessionManager::default();
        let (id, mut transport) = initialized_session(&manager).await;
        let subscription = manager
            .create_standalone_stream(&id)
            .await
            .expect("notification subscription");

        tokio::time::sleep(Duration::from_mins(30)).await;
        assert!(!manager.has_session(&id).await.expect("idle expiry"));
        assert!(!manager.inner.has_session(&id).await.expect("inner removed"));
        assert!(transport.receive().await.is_none());
        assert_eq!(subscription.count().await, 0);
        assert!(matches!(
            manager.create_stream(&id, tools_list_request()).await,
            Err(SessionError::Missing)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn active_request_survives_idle_timeout_and_gets_fresh_idle_period() {
        let manager = BoundedSessionManager::default();
        let (id, mut transport) = initialized_session(&manager).await;
        let stream = manager
            .create_stream(&id, tools_list_request())
            .await
            .expect("active request");
        transport.receive().await.expect("worker receives request");

        tokio::time::sleep(Duration::from_mins(31)).await;
        assert!(manager.has_session(&id).await.expect("active survives"));
        complete_tools_list(&mut transport, stream).await;

        tokio::time::sleep(Duration::from_mins(29)).await;
        assert!(manager.has_session(&id).await.expect("fresh idle period"));
        tokio::time::sleep(Duration::from_mins(1)).await;
        assert!(!manager.has_session(&id).await.expect("idle expires"));
        assert!(transport.receive().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn resumed_notification_stream_does_not_block_idle_expiry() {
        let manager = BoundedSessionManager::new(1, Duration::from_secs(60));
        let (id, _transport) = initialized_session(&manager).await;

        let _stream = manager
            .resume(&id, "0".to_owned())
            .await
            .expect("resume notification stream");
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(
            !manager
                .has_session(&id)
                .await
                .expect("idle session expires")
        );
        manager.create_session().await.expect("capacity recovered");
    }
}
