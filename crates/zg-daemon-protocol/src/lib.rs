//! Versioned wire envelopes shared by the resident daemon and thin clients.
//!
//! These transport-only DTOs are intentionally separate from the in-process
//! `ZvecGrep` API.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zg_engine::ErrorReport;
use zg_engine::api::{
    context::{ContextOptions, ContextResult},
    index::{IndexOptions, IndexResult, progress::IndexProgress},
    info::{InfoOptions, InfoResult},
};

pub const CURRENT_DAEMON_PROTOCOL_VERSION: u32 = 7;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonRequest {
    pub message_id: u64,
    pub kind: DaemonRequestKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum DaemonRequestKind {
    Hello(HelloRequest),
    Execute(Box<ExecuteRequest>),
    Cancel(CancelRequest),
    Shutdown(ShutdownRequest),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HelloRequest {
    pub protocol_version: u32,
    pub client_version: String,
}

impl HelloRequest {
    #[must_use]
    pub fn current(client_version: impl Into<String>) -> Self {
        Self {
            protocol_version: CURRENT_DAEMON_PROTOCOL_VERSION,
            client_version: client_version.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RequestId(Uuid);

impl RequestId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecuteRequest {
    pub request_id: RequestId,
    pub command: DaemonCommand,
    /// Remaining duration at the client, avoiding cross-process clock skew.
    pub timeout_millis: Option<u64>,
    pub principal: Principal,
    pub trace: TraceContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "request")]
pub enum DaemonCommand {
    Context(ContextOptions),
    Index(IndexOptions),
    IndexAuthorization(IndexOptions),
    QueryAuthorization(ContextOptions),
    GrantIndexAuthorization(zg_engine::authorization::IndexAuthorization),
    DropIndex(InfoOptions),
    Info(InfoOptions),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Principal {
    pub subject: String,
}

impl Principal {
    #[must_use]
    pub fn local() -> Self {
        Self {
            subject: "local-user".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceContext {
    pub trace_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CancelRequest {
    pub request_id: RequestId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShutdownRequest {
    pub grace_period_millis: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DaemonResponse {
    pub message_id: u64,
    pub kind: DaemonResponseKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum DaemonResponseKind {
    Hello(HelloReply),
    Event(RequestEvent),
    Result(ExecuteResult),
    Acknowledged(DaemonAction),
    Error(ErrorReply),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HelloReply {
    pub protocol_version: u32,
    pub daemon_version: String,
    pub daemon_instance_id: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonAction {
    Cancel,
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEvent {
    pub request_id: RequestId,
    pub sequence: u64,
    pub kind: RequestEventKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RequestEventKind {
    Started,
    Progress { completed: u64, total: Option<u64> },
    IndexProgress { progress: IndexProgress },
    Warning { code: String, message: String },
    Completed { result_count: usize },
    Failed { report: ErrorReport },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecuteResult {
    pub request_id: RequestId,
    pub result: ExecutionResult,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "value")]
pub enum ExecutionResult {
    Success(DaemonReply),
    Failure(ErrorReply),
}

/// Bounded, newline-delimited events returned by the streaming index endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum IndexStreamEvent {
    Progress(IndexProgress),
    Finished(ExecutionResult),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "reply")]
pub enum DaemonReply {
    Context(Box<ContextResult>),
    Index(Box<IndexResult>),
    IndexAuthorization(Option<zg_engine::authorization::IndexAuthorization>),
    QueryAuthorization(Option<zg_engine::authorization::QueryAuthorization>),
    GrantIndexAuthorization,
    DropIndex(bool),
    Info(Box<InfoResult>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorReply {
    #[serde(flatten)]
    pub report: ErrorReport,
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use zg_engine::EngineError;
    use zg_engine::api::{
        context::ContextOptions,
        index::progress::{
            IndexEmbeddingProgress, IndexEmbeddingStage, IndexProgress, IndexProgressPhase,
        },
    };

    use super::{
        CURRENT_DAEMON_PROTOCOL_VERSION, DaemonCommand, DaemonRequest, DaemonRequestKind,
        DaemonResponse, DaemonResponseKind, ErrorReply, ExecuteRequest, ExecutionResult,
        HelloRequest, Principal, RequestEvent, RequestEventKind, RequestId, TraceContext,
    };

    #[test]
    fn hello_uses_current_protocol_version() {
        let hello = HelloRequest::current("fixture-client");
        assert_eq!(hello.protocol_version, CURRENT_DAEMON_PROTOCOL_VERSION);
    }

    #[test]
    fn execute_request_round_trips() {
        let request = DaemonRequest {
            message_id: 7,
            kind: DaemonRequestKind::Execute(Box::new(ExecuteRequest {
                request_id: RequestId::new(),
                command: DaemonCommand::Context(ContextOptions {
                    root: Some("/workspace".into()),
                    ..ContextOptions::default()
                }),
                timeout_millis: Some(5_000),
                principal: Principal::local(),
                trace: TraceContext {
                    trace_id: Some("trace-1".to_owned()),
                },
            })),
        };
        let encoded = serde_json::to_string(&request).expect("daemon request should serialize");
        let decoded: DaemonRequest =
            serde_json::from_str(&encoded).expect("daemon request should deserialize");
        assert_eq!(decoded, request);
    }

    #[test]
    fn detailed_index_progress_round_trips() {
        let response = DaemonResponse {
            message_id: 8,
            kind: DaemonResponseKind::Event(RequestEvent {
                request_id: RequestId::new(),
                sequence: 2,
                kind: RequestEventKind::IndexProgress {
                    progress: IndexProgress {
                        phase: IndexProgressPhase::Indexing,
                        files_total: Some(10),
                        files_indexed: Some(3),
                        files_failed: Some(1),
                        detail: Some("downloading local/fixture".to_owned()),
                        embedding: Some(IndexEmbeddingProgress {
                            stage: Some(IndexEmbeddingStage::Downloading),
                            model: Some("local/fixture".to_owned()),
                            downloaded_bytes: Some(4),
                            total_bytes: Some(8),
                            ..IndexEmbeddingProgress::default()
                        }),
                    },
                },
            }),
        };

        let encoded = serde_json::to_string(&response).expect("progress should serialize");
        let decoded: DaemonResponse =
            serde_json::from_str(&encoded).expect("progress should deserialize");
        assert_eq!(decoded, response);
    }

    #[test]
    fn complete_error_report_round_trips() {
        let result = ExecutionResult::Failure(ErrorReply {
            report: EngineError::resource_busy("workspace index is locked")
                .with_help("Retry after the active indexing operation finishes.")
                .report(),
            retryable: true,
        });

        let value = serde_json::to_value(&result).expect("error reply should serialize");
        assert_eq!(value["value"]["code"], EngineError::RESOURCE_BUSY);
        assert!(value["value"].get("origin").is_some());
        assert!(value["value"].get("help").is_some());

        let decoded: ExecutionResult =
            serde_json::from_value(value).expect("error reply should deserialize");
        assert_eq!(decoded, result);
    }
}
