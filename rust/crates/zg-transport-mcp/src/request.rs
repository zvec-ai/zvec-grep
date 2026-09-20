//! Request-scoped progress forwarding and cooperative cancellation.

use std::future::Future;

use rmcp::{RoleServer, model::ProgressNotificationParam, service::RequestContext};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zg_engine::{
    EngineError,
    api::index::progress::{IndexProgress, IndexProgressReporter},
};

pub(crate) async fn run<T, F, Fut>(
    context: &RequestContext<RoleServer>,
    operation: F,
) -> Result<T, EngineError>
where
    F: FnOnce(Option<IndexProgressReporter>, CancellationToken) -> Fut,
    Fut: Future<Output = Result<T, EngineError>>,
{
    let signal = context.ct.child_token();
    let _guard = signal.clone().drop_guard();
    if signal.is_cancelled() {
        return Err(EngineError::cancelled("MCP request was cancelled"));
    }
    let token = context.meta.get_progress_token();
    // Coalesce snapshots instead of creating an unbounded queue or a task per event.
    let (sender, mut receiver) = watch::channel(None::<IndexProgress>);
    let reporter = token.as_ref().map(|_| {
        IndexProgressReporter::new(move |progress| {
            sender.send_replace(Some(progress));
        })
        .prioritize_model_progress()
    });
    let operation = operation(reporter, signal.clone());
    tokio::pin!(operation);
    let mut sequence = 0_u32;
    loop {
        tokio::select! {
            biased;
            () = signal.cancelled() => return Err(EngineError::cancelled("MCP request was cancelled")),
            result = &mut operation => return result,
            Ok(()) = receiver.changed(), if token.is_some() => {
                let progress = receiver.borrow_and_update().clone();
                if let (Some(token), Some(progress)) = (&token, progress) {
                    sequence = sequence.saturating_add(1);
                    // An event sequence stays monotonic across scanning, downloading and indexing.
                    let notification = ProgressNotificationParam::new(token.clone(), f64::from(sequence))
                        .with_message(serde_json::json!(progress).to_string());
                    tokio::select! {
                        biased;
                        () = signal.cancelled() => return Err(EngineError::cancelled("MCP request was cancelled")),
                        result = &mut operation => return result,
                        _ = context.peer.notify_progress(notification) => {},
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{
        ClientHandler, RoleClient, ServerHandler, ServiceExt,
        model::*,
        service::{NotificationContext, PeerRequestOptions},
    };
    use std::sync::Arc;
    use tokio::sync::{Notify, mpsc};

    struct TestServer {
        finished: Arc<Notify>,
    }

    impl ServerHandler for TestServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            mut context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            let no_progress = request.name == "no-progress";
            if no_progress {
                // The SDK client inserts a token automatically; also exercise tokenless peers.
                context.meta = RequestMetaObject::default();
            }
            let result = run(&context, |reporter, signal| async move {
                if no_progress {
                    assert!(reporter.is_none());
                    return Ok(());
                }
                let reporter = reporter.expect("progress requested");
                reporter.report(IndexProgress {
                    phase: zg_engine::api::index::progress::IndexProgressPhase::Scanning,
                    files_total: None,
                    files_indexed: None,
                    files_failed: None,
                    detail: None,
                    embedding: None,
                });
                signal.cancelled().await;
                Err(EngineError::cancelled("cancelled"))
            })
            .await;
            if !no_progress {
                assert!(result.is_err());
            }
            self.finished.notify_one();
            Ok(CallToolResult::success(vec![]).into())
        }
    }

    struct TestClient(mpsc::UnboundedSender<ProgressNotificationParam>);

    impl ClientHandler for TestClient {
        async fn on_progress(
            &self,
            params: ProgressNotificationParam,
            _context: NotificationContext<RoleClient>,
        ) {
            self.0.send(params).expect("progress receiver");
        }
    }

    #[tokio::test]
    async fn wire_progress_and_cancellation_are_request_scoped() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let finished = Arc::new(Notify::new());
            let server = TestServer {
                finished: finished.clone(),
            };
            let (server_io, client_io) = tokio::io::duplex(4096);
            let task = tokio::spawn(async move {
                server
                    .serve(server_io)
                    .await
                    .expect("server")
                    .waiting()
                    .await
            });
            let (sender, mut progress) = mpsc::unbounded_channel();
            let client = TestClient(sender).serve(client_io).await.expect("client");
            client
                .call_tool(CallToolRequestParams::new("no-progress"))
                .await
                .expect("no progress call");
            finished.notified().await;
            assert!(progress.try_recv().is_err());
            let handle = client
                .send_cancellable_request(
                    ClientRequest::CallToolRequest(Request::new(CallToolRequestParams::new(
                        "progress",
                    ))),
                    PeerRequestOptions::no_options(),
                )
                .await
                .expect("request");
            let event = progress.recv().await.expect("progress");
            assert_eq!(event.progress_token, handle.progress_token);
            assert!((event.progress - 1.0).abs() < f64::EPSILON);
            assert!(event.message.expect("message").contains("scanning"));
            handle
                .cancel(Some("test cancellation".into()))
                .await
                .expect("cancel");
            finished.notified().await;
            client.cancel().await.expect("close client");
            task.await.expect("server task").expect("server stop");
        })
        .await
        .expect("protocol test must terminate");
    }
}
