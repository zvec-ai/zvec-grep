use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    extract::State,
    http::{
        HeaderMap, StatusCode,
        header::{HOST, ORIGIN},
        uri::Authority,
    },
    routing::{get, post},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zg_daemon_protocol::{DaemonCommand, DaemonReply, ErrorReply, ExecutionResult};
use zg_engine::ZvecGrep;
use zg_transport_mcp::{
    IndexOperationProvider, McpToolset, ServerStatusProvider, ServerStatusSnapshot,
    ZvecGrepMcpServer,
};

use crate::{
    DaemonError, ServerConfig, controller::InstanceLock, job_scheduler::JobState,
    workspace_runtime::WorkspaceRuntimeManager,
};

#[derive(Clone)]
struct ControlState {
    shutdown: CancellationToken,
    listen_port: u16,
    engine: Arc<ZvecGrep>,
    runtimes: WorkspaceRuntimeManager,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopbackHost {
    Localhost,
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LoopbackOrigin {
    host: LoopbackHost,
    port: u16,
}

struct RuntimeStatusProvider {
    started: Instant,
    shutdown: CancellationToken,
    runtimes: WorkspaceRuntimeManager,
    engine: Arc<ZvecGrep>,
}

impl ServerStatusProvider for RuntimeStatusProvider {
    fn snapshot(&self) -> ServerStatusSnapshot {
        let runtime = self.runtimes.snapshot();
        let engine = self.engine.runtime_snapshot();
        ServerStatusSnapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            shutting_down: self.shutdown.is_cancelled(),
            active_runtimes: runtime.active_runtimes,
            queued_jobs: runtime.jobs.queued,
            running_jobs: runtime.jobs.running,
            loaded_models: engine.loaded_models,
            active_model_leases: engine.active_model_leases,
        }
    }
}

pub(crate) async fn run_server(
    config: ServerConfig,
    engine: Arc<ZvecGrep>,
) -> Result<(), DaemonError> {
    engine.enable_read_session_cache()?;
    let token = crate::resolve_token(config.token_file.as_deref())?;
    let mut instance = InstanceLock::acquire(&config).await?;
    let listener = match tokio::net::TcpListener::bind(config.listen.socket_addr()).await {
        Ok(listener) => listener,
        Err(error) => {
            instance.release().await?;
            return Err(error.into());
        }
    };
    let listen_port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            instance.release().await?;
            return Err(error.into());
        }
    };
    let shutdown = CancellationToken::new();
    let runtimes = WorkspaceRuntimeManager::native(Arc::clone(&engine));
    let status: Arc<dyn ServerStatusProvider> = Arc::new(RuntimeStatusProvider {
        started: Instant::now(),
        shutdown: shutdown.clone(),
        runtimes: runtimes.clone(),
        engine: Arc::clone(&engine),
    });
    let index_operations: Arc<dyn IndexOperationProvider> = Arc::new(runtimes.clone());
    let mcp_server = match config.mcp_toolset.unwrap_or_default() {
        McpToolset::Agent => {
            ZvecGrepMcpServer::agent_with_index_operations(Arc::clone(&engine), index_operations)
        }
        McpToolset::Full => ZvecGrepMcpServer::full_with_index_operations(
            Arc::clone(&engine),
            status,
            index_operations,
        ),
    };
    let mcp_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown.child_token())
        .with_allowed_hosts([
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
            config.listen.socket_addr().to_string(),
        ])
        .with_max_request_body_bytes(1024 * 1024);
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp_server.clone()),
        LocalSessionManager::default().into(),
        mcp_config,
    );
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/control/shutdown", post(request_shutdown))
        .route("/admin/execute", post(execute_command))
        .route("/admin/index", post(stream_index))
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn_with_state(
            token,
            crate::authentication::authenticate,
        ))
        .with_state(ControlState {
            shutdown: shutdown.clone(),
            listen_port,
            engine: Arc::clone(&engine),
            runtimes: runtimes.clone(),
        });

    if let Err(error) = instance.mark_ready().await {
        instance.release().await?;
        return Err(error);
    }
    info!(url = %config.listen.server_url(), "zvec-grep daemon ready");
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal_shutdown.cancel();
    });
    let runtime_shutdown = shutdown.clone();
    let runtime_manager = runtimes.clone();
    let runtime_shutdown_task = tokio::spawn(async move {
        runtime_shutdown.cancelled().await;
        if let Err(error) = runtime_manager.shutdown_all().await {
            warn!(%error, "daemon runtime shutdown was incomplete");
        }
    });
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;
    shutdown.cancel();
    let _ = runtime_shutdown_task.await;
    engine.close();
    let release_result = instance.release().await;
    serve_result?;
    release_result
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn request_shutdown(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    shutdown_response(&state.shutdown, state.listen_port, &headers)
}

fn shutdown_response(
    shutdown: &CancellationToken,
    listen_port: u16,
    headers: &HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !valid_shutdown_origin(headers, listen_port) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "forbidden_origin" })),
        );
    }
    shutdown.cancel();
    (StatusCode::ACCEPTED, Json(json!({ "status": "stopping" })))
}

async fn stream_index(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Json(mut options): Json<zg_engine::api::index::IndexOptions>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use zg_daemon_protocol::IndexStreamEvent;
    if !has_loopback_host(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let progress_sender = sender.clone();
    options.on_progress = Some(zg_engine::api::index::progress::IndexProgressReporter::new(
        move |progress| {
            // Slow clients may skip intermediate snapshots; memory stays bounded.
            let _ = progress_sender.try_send(IndexStreamEvent::Progress(progress));
        },
    ));
    tokio::spawn(async move {
        let initial = zg_engine::api::index::progress::IndexProgress {
            phase: zg_engine::api::index::progress::IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: Some("Waiting for index job".into()),
            embedding: None,
        };
        let _ = sender.send(IndexStreamEvent::Progress(initial)).await;
        let (_, Json(result)) =
            execute_command(State(state), headers, Json(DaemonCommand::Index(options))).await;
        let _ = sender.send(IndexStreamEvent::Finished(result)).await;
    });
    let stream = futures::stream::unfold(receiver, |mut receiver| async {
        let event = receiver.recv().await?;
        let bytes = serde_json::to_vec(&event)
            .map(|mut bytes| {
                bytes.push(b'\n');
                bytes::Bytes::from(bytes)
            })
            .map_err(std::io::Error::other);
        Some((bytes, receiver))
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn execute_command(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Json(command): Json<DaemonCommand>,
) -> (StatusCode, Json<ExecutionResult>) {
    if !has_loopback_host(&headers) {
        let error =
            zg_engine::EngineError::permission_denied("request is not authorized for this daemon");
        return (
            StatusCode::UNAUTHORIZED,
            Json(ExecutionResult::Failure(ErrorReply {
                report: error.report(),
                retryable: false,
            })),
        );
    }
    let result = match command {
        DaemonCommand::QueryAuthorization(request) => engine_execution(
            zg_engine::authorization::query_authorizations(&request)
                .map(DaemonReply::QueryAuthorization),
        ),
        DaemonCommand::IndexAuthorization(request) => engine_execution(
            zg_engine::authorization::index_authorizations(&request)
                .map(DaemonReply::IndexAuthorization),
        ),
        DaemonCommand::GrantIndexAuthorization(target) => engine_execution(
            zg_engine::authorization::grant_index(&target)
                .map(|()| DaemonReply::GrantIndexAuthorization),
        ),
        DaemonCommand::Context(mut request) => {
            // HTTP bodies cannot carry in-process cancellation tokens. Tie this wait
            // to request disposal and daemon shutdown, as the MCP transport does.
            let signal = state.shutdown.child_token();
            let _guard = signal.clone().drop_guard();
            request.signal = Some(signal);
            engine_execution(
                state
                    .runtimes
                    .search(&state.engine, request)
                    .await
                    .map(|reply| DaemonReply::Context(Box::new(reply))),
            )
        }
        DaemonCommand::Index(request) => match state.runtimes.submit_index(request, true).await {
            Ok(submitted) if submitted.job.state == JobState::Succeeded => {
                submitted.result.map_or_else(
                    || internal_failure("successful daemon index job had no result"),
                    |reply| ExecutionResult::Success(DaemonReply::Index(Box::new(reply))),
                )
            }
            Ok(submitted) => ExecutionResult::Failure(submitted.job.error.map_or_else(
                || {
                    error_reply(zg_engine::EngineError::internal(
                        "daemon index job ended without an error",
                    ))
                },
                |error| ErrorReply {
                    report: error.report,
                    retryable: error.retryable,
                },
            )),
            Err(error) => engine_execution::<DaemonReply>(Err(error.into_engine_error())),
        },
        DaemonCommand::DropIndex(request) => engine_execution(
            state
                .runtimes
                .drop_index(request)
                .await
                .map(DaemonReply::DropIndex)
                .map_err(crate::workspace_runtime::WorkspaceRuntimeError::into_engine_error),
        ),
        DaemonCommand::Info(request) => engine_execution(
            state
                .runtimes
                .info(&state.engine, request)
                .await
                .map(|reply| DaemonReply::Info(Box::new(reply))),
        ),
    };
    (StatusCode::OK, Json(result))
}

fn engine_execution<T>(result: Result<T, zg_engine::EngineError>) -> ExecutionResult
where
    T: Into<DaemonReply>,
{
    result.map_or_else(
        |error| {
            ExecutionResult::Failure(ErrorReply {
                retryable: error.is_retryable(),
                report: error.report(),
            })
        },
        |reply| ExecutionResult::Success(reply.into()),
    )
}

fn internal_failure(message: &str) -> ExecutionResult {
    ExecutionResult::Failure(error_reply(zg_engine::EngineError::internal(message)))
}

fn error_reply(error: zg_engine::EngineError) -> ErrorReply {
    ErrorReply {
        retryable: error.is_retryable(),
        report: error.into_report(),
    }
}

fn has_loopback_host(headers: &HeaderMap) -> bool {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .is_some_and(|authority| matches!(authority.host(), "localhost" | "127.0.0.1" | "::1"))
}

fn valid_shutdown_origin(headers: &HeaderMap, listen_port: u16) -> bool {
    let Some(host) = single_header(headers, HOST) else {
        return false;
    };
    let Some(target) = parse_loopback_origin(&format!("http://{host}")) else {
        return false;
    };
    if target.port != listen_port {
        return false;
    }

    let mut origins = headers.get_all(ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    origin
        .to_str()
        .ok()
        .and_then(parse_loopback_origin)
        .is_some_and(|origin| origin == target)
}

fn single_header(headers: &HeaderMap, name: axum::http::header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

fn parse_loopback_origin(value: &str) -> Option<LoopbackOrigin> {
    let (scheme, authority_text) = value.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http")
        || authority_text.is_empty()
        || authority_text
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@' | b'\\'))
    {
        return None;
    }
    let authority = authority_text.parse::<Authority>().ok()?;
    let authority_host = authority.host();
    let host_text = authority_host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(authority_host);
    let host = if host_text.eq_ignore_ascii_case("localhost") {
        LoopbackHost::Localhost
    } else if host_text == "127.0.0.1" {
        LoopbackHost::Ipv4
    } else if host_text == "::1" {
        LoopbackHost::Ipv6
    } else {
        return None;
    };
    let has_explicit_port = authority_text
        .rfind(':')
        .is_some_and(|colon| !authority_text[colon..].starts_with("::"));
    let port = match authority.port() {
        Some(port) => port.as_str().parse::<u16>().ok()?,
        None if has_explicit_port => return None,
        None => 80,
    };
    Some(LoopbackOrigin { host, port })
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let terminate = signal(SignalKind::terminate());
    if let Ok(mut terminate) = terminate {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header::ORIGIN};

    use super::*;

    #[test]
    fn shutdown_rejects_hostile_origin_without_cancelling() {
        let shutdown = CancellationToken::new();
        let headers = headers("127.0.0.1:7999", &["https://untrusted.example"]);

        let (status, _) = shutdown_response(&shutdown, 7999, &headers);

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!shutdown.is_cancelled());
    }

    #[test]
    fn shutdown_accepts_native_and_exact_origin_requests() {
        for (host, origins, port) in [
            ("127.0.0.1:7999", Vec::new(), 7999),
            ("127.0.0.1:7999", vec!["http://127.0.0.1:7999"], 7999),
            ("[::1]:7999", vec!["http://[::1]:7999"], 7999),
            ("LOCALHOST:7999", vec!["HTTP://LOCALHOST:7999"], 7999),
            ("localhost:80", Vec::new(), 80),
            ("localhost:80", vec!["http://localhost:80"], 80),
            ("localhost", Vec::new(), 80),
            ("localhost", vec!["http://localhost"], 80),
        ] {
            let shutdown = CancellationToken::new();
            let headers = headers(host, &origins);

            let (status, _) = shutdown_response(&shutdown, port, &headers);

            assert_eq!(status, StatusCode::ACCEPTED, "host={host}");
            assert!(shutdown.is_cancelled(), "host={host}");
        }
    }

    #[test]
    fn shutdown_rejects_mismatched_and_malformed_authorities() {
        for (host, origin, port) in [
            ("localhost:8000", None, 7999),
            ("localhost", None, 7999),
            ("untrusted.example:7999", None, 7999),
            ("localhost:7999", Some("http://127.0.0.1:7999"), 7999),
            ("localhost:7999", Some("http://localhost:8000"), 7999),
            ("localhost:7999", Some("https://localhost:7999"), 7999),
            ("localhost:7999", Some("null"), 7999),
            ("localhost:7999", Some(""), 7999),
            ("localhost:7999", Some("not-an-origin"), 7999),
            ("localhost:7999", Some("http://localhost:65536"), 7999),
            ("localhost:7999", Some("http://localhost:7999/"), 7999),
            ("localhost:7999", Some("http://localhost:7999?"), 7999),
            ("localhost:7999", Some("http://localhost:7999#"), 7999),
            ("localhost:7999", Some("http://user@localhost:7999"), 7999),
            // Invalid explicit port in Host must be rejected even when daemon listens on 80.
            ("localhost:bogus", None, 80),
            ("localhost:99999", None, 80),
            ("127.0.0.1:bogus", None, 80),
            // Invalid explicit port in Origin must be rejected.
            ("localhost:80", Some("http://localhost:bogus"), 80),
        ] {
            let origins = origin.into_iter().collect::<Vec<_>>();
            assert!(
                !valid_shutdown_origin(&headers(host, &origins), port),
                "host={host}, origin={origin:?}"
            );
        }
    }

    #[test]
    fn shutdown_rejects_duplicate_host_and_origin_headers() {
        let mut duplicate_host = headers("localhost:7999", &[]);
        duplicate_host.append(HOST, HeaderValue::from_static("localhost:7999"));
        assert!(!valid_shutdown_origin(&duplicate_host, 7999));

        let duplicate_origin = headers(
            "localhost:7999",
            &["http://localhost:7999", "http://localhost:7999"],
        );
        assert!(!valid_shutdown_origin(&duplicate_origin, 7999));
    }

    fn headers(host: &str, origins: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HOST,
            HeaderValue::from_str(host).expect("valid Host header"),
        );
        for origin in origins {
            headers.append(
                ORIGIN,
                HeaderValue::from_str(origin).expect("valid Origin header"),
            );
        }
        headers
    }
}
