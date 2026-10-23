//! HTTP wire commands, replies, and index progress shared by the daemon and its clients.
//!
//! These transport-only DTOs are intentionally separate from the in-process
//! `ZvecGrep` API.

use serde::{Deserialize, Serialize};
use zg_engine::ErrorReport;
use zg_engine::api::{
    context::{ContextOptions, ContextResult},
    index::{IndexOptions, IndexResult, progress::IndexProgress},
    info::{InfoOptions, InfoResult},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "request")]
pub enum DaemonCommand {
    IndexSetup(zg_engine::api::index::setup::IndexSetupOptions),
    Context(ContextOptions),
    Index(IndexOptions),
    IndexAuthorization(IndexOptions),
    QueryAuthorization(ContextOptions),
    GrantIndexAuthorization(zg_engine::authorization::IndexAuthorization),
    DropIndex(InfoOptions),
    Info(InfoOptions),
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
    IndexSetup(zg_engine::api::index::setup::IndexSetup),
    Context(Box<ContextResult>),
    Index(Box<IndexResult>),
    IndexAuthorization(Vec<zg_engine::authorization::IndexAuthorization>),
    QueryAuthorization(Vec<zg_engine::authorization::QueryAuthorization>),
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

    use super::{DaemonCommand, ErrorReply, ExecutionResult, IndexStreamEvent};

    #[test]
    fn index_round_trips_by_root_without_a_name() {
        use zg_engine::api::index::IndexOptions;
        let command = DaemonCommand::Index(IndexOptions {
            root: Some("/workspace".into()),
            ..IndexOptions::default()
        });
        let mut encoded = serde_json::to_value(&command).expect("serialize command");
        assert!(encoded["request"].get("name").is_none());
        assert_eq!(encoded["request"]["root"], "/workspace");
        let decoded: DaemonCommand =
            serde_json::from_value(encoded.clone()).expect("deserialize command");
        assert_eq!(decoded, command);
        encoded["request"]["name"] = serde_json::json!("obsolete");
        assert!(serde_json::from_value::<DaemonCommand>(encoded).is_err());
    }

    #[test]
    fn context_command_round_trips() {
        let command = DaemonCommand::Context(ContextOptions {
            root: Some("/workspace".into()),
            ..ContextOptions::default()
        });
        let encoded = serde_json::to_value(&command).expect("daemon command should serialize");
        assert_eq!(encoded["kind"], "context");
        assert_eq!(encoded["request"]["root"], "/workspace");
        let decoded: DaemonCommand =
            serde_json::from_value(encoded).expect("daemon command should deserialize");
        assert_eq!(decoded, command);
    }

    #[test]
    fn indexed_source_bytes_and_entity_counts_round_trip_without_truncation_fields() {
        use zg_engine::api::index::options::ScanRules;
        use zg_engine::api::info::{
            InfoResult,
            result::{
                IndexCompatibility, IndexStats, InfoSource, WorkspaceIndexInfo,
                WorkspaceIndexPolicy,
            },
        };

        let count = u64::from(u32::MAX) + 1;
        let reply = super::DaemonReply::Info(Box::new(InfoResult {
            root: "/workspace".into(),
            indexed: true,
            compatibility: IndexCompatibility::Compatible { version: 2 },
            index_policy: WorkspaceIndexPolicy::Enabled,
            home: "/workspace/.zvec-grep".into(),
            index_path: "/workspace/.zvec-grep/storage".into(),
            source: InfoSource::Index,
            workspace_index: Some(WorkspaceIndexInfo {
                path: "/workspace/.zvec-grep".into(),
                root: "/workspace".into(),
                scan: ScanRules::default(),
                policy: WorkspaceIndexPolicy::Enabled,
                embedding: None,
                fts: Some(zg_engine::api::info::result::WorkspaceIndexFts {
                    tokenizer: "jieba".into(),
                    filters: vec!["lowercase".into()],
                }),
                index_version: Some(2),

                created_epoch_ms: 1,
                updated_epoch_ms: 2,
            }),
            status: Some(IndexStats {
                entities_indexed: count,
                indexed_size_bytes: count + 3,
                ..IndexStats::default()
            }),
            suggestion: None,
        }));

        let encoded = serde_json::to_value(&reply).expect("info reply should serialize");
        assert_eq!(
            encoded["reply"]["compatibility"],
            serde_json::json!({"status": "compatible", "version": 2})
        );
        assert!(encoded["reply"]["workspace_index"].get("name").is_none());
        assert_eq!(encoded["reply"]["workspace_index"]["root"], "/workspace");
        assert!(encoded["reply"]["workspace_index"].get("id").is_none());
        assert_eq!(
            encoded["reply"]["workspace_index"]["fts"]["tokenizer"],
            "jieba"
        );
        assert_eq!(
            encoded["reply"]["workspace_index"]["fts"]["filters"],
            serde_json::json!(["lowercase"])
        );
        let status = &encoded["reply"]["status"];
        assert_eq!(status["entities_indexed"], count);
        assert_eq!(status["indexed_size_bytes"], count + 3);
        assert!(status.get("fragments_truncated").is_none());
        let decoded: super::DaemonReply =
            serde_json::from_value(encoded).expect("info reply should deserialize");
        assert_eq!(decoded, reply);
    }

    #[test]
    fn detailed_index_progress_round_trips() {
        let event = IndexStreamEvent::Progress(IndexProgress {
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
        });

        let encoded = serde_json::to_value(&event).expect("progress should serialize");
        assert_eq!(encoded["kind"], "progress");
        assert_eq!(encoded["value"]["files_indexed"], 3);
        let decoded: IndexStreamEvent =
            serde_json::from_value(encoded).expect("progress should deserialize");
        assert_eq!(decoded, event);
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
