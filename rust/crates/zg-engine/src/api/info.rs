//! Types used by `info` and `drop_index`.

pub use options::InfoOptions;
pub use result::InfoResult;

pub mod options {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct InfoOptions {
        /// Workspace to inspect or mutate. `None` uses the working directory.
        pub root: Option<PathBuf>,
        pub include_status: bool,
    }
}

pub mod result {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use crate::domain::ScanRules;

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct InfoResult {
        pub root: PathBuf,
        pub indexed: bool,
        pub index_policy: WorkspaceIndexPolicy,
        pub home: PathBuf,
        pub index_path: PathBuf,
        pub source: InfoSource,
        pub workspace_index: Option<WorkspaceIndexInfo>,
        pub status: Option<IndexStats>,
        pub suggestion: Option<String>,
    }

    impl InfoResult {
        /// Health observed by this inspection. Merely opening metadata cannot establish readiness.
        #[must_use]
        pub fn index_status(&self) -> IndexStatus {
            match self.index_policy {
                WorkspaceIndexPolicy::Disabled => return IndexStatus::Disabled,
                WorkspaceIndexPolicy::Uninitialized => return IndexStatus::Uninitialized,
                WorkspaceIndexPolicy::Enabled => {}
            }
            if !self.indexed {
                return IndexStatus::Missing;
            }
            let Some(stats) = &self.status else {
                return IndexStatus::Unknown;
            };
            if stats.files_failed > 0 {
                IndexStatus::Failed
            } else if stats.files_pending > 0
                || stats.files_added > 0
                || stats.files_modified > 0
                || stats.files_deleted > 0
            {
                IndexStatus::Stale
            } else {
                IndexStatus::Ready
            }
        }
    }

    /// Observed index health, independent of a build job's queued/running/failed state.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IndexStatus {
        Unknown,
        Uninitialized,
        Disabled,
        Missing,
        Ready,
        Stale,
        Failed,
    }

    impl IndexStatus {
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Unknown => "unknown",
                Self::Uninitialized => "uninitialized",
                Self::Disabled => "disabled",
                Self::Missing => "missing",
                Self::Ready => "ready",
                Self::Stale => "stale",
                Self::Failed => "failed",
            }
        }
    }

    /// Last completed inspection, kept only in memory by a resident workspace runtime.
    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexStatusSnapshot {
        pub status: IndexStatus,
        pub stats: Option<IndexStats>,
        pub checked_epoch_ms: u64,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum InfoSource {
        Index,
        Unindexed,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WorkspaceIndexPolicy {
        Enabled,
        Disabled,
        Uninitialized,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexInfo {
        pub name: String,
        pub path: PathBuf,
        /// The single base directory for every source file in this workspace.
        pub root: PathBuf,
        pub scan: ScanRules,
        pub policy: WorkspaceIndexPolicy,
        /// The workspace's text embedding model.
        pub embedding: Option<WorkspaceIndexEmbedding>,
        pub fts: Option<WorkspaceIndexFts>,
        pub index_version: Option<u32>,
        pub created_epoch_ms: u64,
        pub updated_epoch_ms: u64,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexEmbedding {
        pub provider: String,
        pub model: String,
        pub dimension: usize,
        pub metric: String,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexFts {
        pub tokenizer: String,
        pub filters: Vec<String>,
    }

    /// A file that could not be indexed, recorded independently of successful files.
    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct FailedFile {
        pub path: PathBuf,
        pub reason: String,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexStats {
        pub files_scanned: usize,
        pub files_stored: usize,
        pub files_indexed: usize,
        pub entities_indexed: u64,
        /// Total source snapshot bytes for successfully indexed files, excluding index storage.
        pub indexed_size_bytes: u64,
        pub files_pending: usize,
        pub files_failed: usize,
        #[serde(default)]
        pub failed_files: Vec<FailedFile>,
        pub files_added: usize,
        pub files_modified: usize,
        pub files_deleted: usize,
        pub files_unchanged: usize,
    }
}

impl From<&crate::domain::IndexState> for result::WorkspaceIndexPolicy {
    fn from(value: &crate::domain::IndexState) -> Self {
        match value {
            crate::domain::IndexState::Uninitialized => Self::Uninitialized,
            crate::domain::IndexState::Enabled(_) => Self::Enabled,
            crate::domain::IndexState::Disabled => Self::Disabled,
        }
    }
}

impl result::WorkspaceIndexInfo {
    pub(crate) fn from_workspace(
        workspace: &crate::domain::Workspace,
        home: &std::path::Path,
        index_version: Option<u32>,
    ) -> Self {
        Self {
            name: workspace.name.clone(),
            path: home.to_path_buf(),
            root: workspace.root.clone(),
            scan: workspace.scan.clone(),
            policy: (&workspace.index).into(),
            embedding: workspace
                .index
                .descriptor()
                .and_then(|index| index.embeddings.first().map(Into::into)),
            fts: workspace
                .index
                .descriptor()
                .map(|index| (&index.fts).into()),
            index_version,
            created_epoch_ms: workspace.created_epoch_ms,
            updated_epoch_ms: workspace.updated_epoch_ms,
        }
    }
}

impl From<&crate::domain::FtsConfig> for result::WorkspaceIndexFts {
    fn from(value: &crate::domain::FtsConfig) -> Self {
        Self {
            tokenizer: value.tokenizer.to_owned(),
            filters: value
                .filters
                .iter()
                .map(|filter| (*filter).to_owned())
                .collect(),
        }
    }
}

impl From<&crate::domain::EmbeddingModelInfo> for result::WorkspaceIndexEmbedding {
    fn from(value: &crate::domain::EmbeddingModelInfo) -> Self {
        Self {
            provider: value.model.provider.clone(),
            model: value.model.name.clone(),
            dimension: value.dimension,
            metric: match value.metric {
                crate::domain::Metric::Cosine => "cosine",
                crate::domain::Metric::DotProduct => "dot",
                crate::domain::Metric::Euclidean => "euclidean",
            }
            .to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InfoResult, result::*};

    fn indexed_info() -> InfoResult {
        InfoResult {
            root: "/workspace".into(),
            indexed: true,
            index_policy: WorkspaceIndexPolicy::Enabled,
            home: "/workspace/.zvec-grep".into(),
            index_path: "/workspace/.zvec-grep/storage".into(),
            source: InfoSource::Index,
            workspace_index: None,
            status: Some(IndexStats::default()),
            suggestion: None,
        }
    }

    #[test]
    fn readiness_requires_a_scan_without_pending_changes_or_failures() {
        let mut info = indexed_info();
        assert_eq!(info.index_status(), IndexStatus::Ready);
        for stats in [
            IndexStats {
                files_added: 1,
                ..IndexStats::default()
            },
            IndexStats {
                files_modified: 1,
                ..IndexStats::default()
            },
            IndexStats {
                files_deleted: 1,
                ..IndexStats::default()
            },
            IndexStats {
                files_pending: 1,
                ..IndexStats::default()
            },
        ] {
            info.status = Some(stats);
            assert_eq!(info.index_status(), IndexStatus::Stale);
        }
        info.status = Some(IndexStats {
            files_failed: 1,
            files_pending: 1,
            ..IndexStats::default()
        });
        assert_eq!(info.index_status(), IndexStatus::Failed);
        info.status = None;
        assert_eq!(info.index_status(), IndexStatus::Unknown);
    }

    #[test]
    fn saved_policy_and_missing_storage_have_distinct_states() {
        let mut info = indexed_info();
        info.indexed = false;
        assert_eq!(info.index_status(), IndexStatus::Missing);
        info.index_policy = WorkspaceIndexPolicy::Disabled;
        assert_eq!(info.index_status(), IndexStatus::Disabled);
        info.index_policy = WorkspaceIndexPolicy::Uninitialized;
        assert_eq!(info.index_status(), IndexStatus::Uninitialized);
    }
}
