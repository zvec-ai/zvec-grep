use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::HOST, uri::Authority},
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
    engine: Arc<ZvecGrep>,
    runtimes: WorkspaceRuntimeManager,
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
    let mut instance = InstanceLock::acquire(&config).await?;
    let listener = match tokio::net::TcpListener::bind(config.listen.socket_addr()).await {
        Ok(listener) => listener,
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
    let mcp_server = match config.mcp_toolset {
        McpToolset::Agent => ZvecGrepMcpServer::agent(Arc::clone(&engine)),
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
        .nest_service("/mcp", mcp_service)
        .with_state(ControlState {
            shutdown: shutdown.clone(),
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
    if !has_loopback_host(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        );
    }
    state.shutdown.cancel();
    (StatusCode::ACCEPTED, Json(json!({ "status": "stopping" })))
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
        DaemonCommand::IndexAuthorization(request) => engine_execution(
            zg_engine::authorization::index_authorization(&request)
                .map(DaemonReply::IndexAuthorization),
        ),
        DaemonCommand::GrantIndexAuthorization(target) => engine_execution(
            zg_engine::authorization::grant_index(&target)
                .map(|()| DaemonReply::GrantIndexAuthorization),
        ),
        DaemonCommand::Context(request) => engine_execution(
            state
                .runtimes
                .search(&state.engine, request)
                .await
                .map(|reply| DaemonReply::Context(Box::new(reply))),
        ),
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
                .engine
                .info(request)
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
