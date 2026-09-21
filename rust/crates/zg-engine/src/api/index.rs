//! Types used by [`crate::ZvecGrep::index`].

pub use options::IndexOptions;
pub use result::IndexResult;

/// Input types for [`crate::ZvecGrep::index`].
pub mod options {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};
    use tokio_util::sync::CancellationToken;

    use super::progress::IndexProgressReporter;
    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct IndexOptions {
        /// Unique workspace name. Defaults to the root directory name on creation;
        /// supplying a different name explicitly renames an existing workspace.
        #[serde(default)]
        pub name: Option<String>,
        /// Workspace whose index is being updated. `None` uses the working directory.
        pub root: Option<PathBuf>,
        /// Build from empty storage and replace the active index only after success.
        pub rebuild: bool,
        /// Resets saved scan rules before applying this request's updates.
        pub reset_paths: bool,
        /// Normalized watcher changes for a narrow incremental index operation.
        /// An empty list means normal discovery rather than "no work".
        pub changes: Vec<WorkspaceChange>,
        #[serde(default)]
        pub scan: ScanRulesUpdate,
        /// The single model used to embed text content in this workspace.
        pub embedding: Option<EmbeddingModelSpec>,
        /// Maximum embedding batch tasks for this index operation.
        /// The model default is used when omitted.
        pub embedding_concurrency: Option<usize>,
        /// Maximum time to wait for workspace admission, in milliseconds (default: 30 seconds).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub lock_timeout_ms: Option<u64>,
        /// Allows remote embedding for this operation without persisting a grant.
        #[serde(default)]
        pub allow_remote: bool,
        /// Exact destinations approved by interactive consent for this operation.
        /// Separate from the explicit blanket `allow_remote` option.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub authorized_remote: Vec<crate::authorization::IndexAuthorization>,
        #[serde(default)]
        pub api_key: Option<String>,
        #[serde(default)]
        pub endpoint: Option<String>,
        #[serde(default)]
        pub device: Option<crate::api::index::options::Device>,
        #[serde(default)]
        pub model_cache: Option<PathBuf>,
        /// Receives in-process indexing and model download progress.
        ///
        /// Reporters are runtime-only and are deliberately omitted from serialized
        /// daemon and transport requests.
        #[serde(skip)]
        pub on_progress: Option<IndexProgressReporter>,
        /// Cancels this in-process indexing operation.
        ///
        /// Like progress reporters, cancellation is runtime-only and is never
        /// serialized across the daemon protocol.
        #[serde(skip)]
        pub signal: Option<CancellationToken>,
    }

    pub use crate::domain::{ContentKind, GlobRule, ScanRules};

    /// Changes to filesystem scanning. `null` clears an optional limit.
    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct ScanRulesUpdate {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub globs: Option<Vec<GlobRule>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub hidden: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub no_ignore: Option<bool>,
        /// Whether indexing may descend into nested Git repositories and submodules.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub nested_git: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ignore_files: Option<Vec<PathBuf>>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_update"
        )]
        pub max_depth: Option<Option<usize>>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_update"
        )]
        pub max_file_size_bytes: Option<Option<u64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub follow_symlinks: Option<bool>,
    }

    impl ScanRulesUpdate {
        /// Applies only the fields supplied by this request.
        pub fn apply(&self, target: &mut ScanRules) {
            if let Some(value) = &self.globs {
                target.globs.clone_from(value);
            }
            if let Some(value) = self.hidden {
                target.hidden = value;
            }
            if let Some(value) = self.no_ignore {
                target.no_ignore = value;
            }
            if let Some(value) = self.nested_git {
                target.nested_git = value;
            }
            if let Some(value) = &self.ignore_files {
                target.ignore_files.clone_from(value);
            }
            if let Some(value) = self.max_depth {
                target.max_depth = value;
            }
            if let Some(value) = self.max_file_size_bytes {
                target.max_file_size_bytes = value;
            }
            if let Some(value) = self.follow_symlinks {
                target.follow_symlinks = value;
            }
        }
    }

    /// Distinguishes an omitted update from an explicit JSON null.
    ///
    /// # Errors
    ///
    /// Returns the deserializer error for a value of the wrong type.
    pub fn deserialize_optional_update<'de, D, T>(
        deserializer: D,
    ) -> Result<Option<Option<T>>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }

    /// A normalized filesystem change relative to the workspace root.
    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind", content = "path")]
    pub enum WorkspaceChange {
        Upsert(PathBuf),
        Delete(PathBuf),
        RescanDirectory(PathBuf),
        DeletePrefix(PathBuf),
        Rescan,
    }

    pub use crate::domain::model::Device;

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct EmbeddingModelSpec {
        pub reference: String,
        pub revision: Option<String>,
        pub cache_dir: Option<PathBuf>,
        pub endpoint: Option<String>,
        pub device: Device,
    }

    #[cfg(test)]
    mod selection_update_tests {
        use super::*;

        #[test]
        fn updates_preserve_omission_and_apply_false_empty_and_null() {
            let mut scan = ScanRules {
                globs: vec!["*.rs".into()],
                hidden: true,
                nested_git: true,
                max_depth: Some(3),
                ..Default::default()
            };
            let update: ScanRulesUpdate =
                serde_json::from_str(r#"{"hidden":false,"nested_git":false,"max_depth":null}"#)
                    .expect("valid test fixture");
            assert_eq!(update.max_depth, Some(None));
            assert_eq!(update.follow_symlinks, None);
            update.apply(&mut scan);
            assert!(!scan.hidden);
            assert!(!scan.nested_git);
            assert_eq!(scan.max_depth, None);
            ScanRulesUpdate::default().apply(&mut scan);
            assert!(!scan.nested_git, "omission preserves the configured value");
            assert_eq!(scan.globs, vec![GlobRule::from("*.rs")]);
            assert_eq!(
                serde_json::to_value(&update).expect("valid test fixture")["max_depth"],
                serde_json::Value::Null
            );
            assert!(
                serde_json::to_value(ScanRulesUpdate::default())
                    .expect("valid test fixture")
                    .get("max_depth")
                    .is_none()
            );
            let update: ScanRulesUpdate =
                serde_json::from_str(r#"{"globs":[]}"#).expect("valid test fixture");
            update.apply(&mut scan);
            assert!(scan.globs.is_empty());
        }

        #[test]
        fn query_constraints_are_not_accepted_as_scan_updates() {
            for field in [
                "formats",
                "excluded_formats",
                "categories",
                "excluded_categories",
                "modified_after_epoch_ms",
                "modified_before_epoch_ms",
            ] {
                let mut value = serde_json::json!({});
                value[field] = serde_json::json!([]);
                assert!(serde_json::from_value::<ScanRulesUpdate>(value).is_err());
            }
        }
    }
}

/// Progress values emitted while [`crate::ZvecGrep::index`] is running.
pub mod progress {
    use std::{
        fmt,
        sync::{Arc, Mutex},
    };

    use serde::{Deserialize, Serialize};

    /// Thread-safe callback used by the in-process indexing API.
    #[derive(Clone)]
    pub struct IndexProgressReporter(Arc<dyn Fn(IndexProgress) + Send + Sync + 'static>);

    impl IndexProgressReporter {
        #[must_use]
        pub fn new(reporter: impl Fn(IndexProgress) + Send + Sync + 'static) -> Self {
            Self(Arc::new(reporter))
        }

        /// Keeps model preparation visible until ready, then restores indexing progress.
        /// Apply before transport snapshot coalescing so a missed ready event cannot
        /// leave a remote display stuck in the downloading phase.
        #[must_use]
        pub fn prioritize_model_progress(self) -> Self {
            let state = Mutex::new((false, None::<IndexProgress>));
            Self::new(move |progress| {
                // Serialize delivery with state changes across concurrent producers.
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let model_stage = progress.embedding.as_ref().and_then(|value| value.stage);
                match model_stage {
                    Some(IndexEmbeddingStage::Preparing | IndexEmbeddingStage::Downloading) => {
                        state.0 = true;
                    }
                    Some(IndexEmbeddingStage::Ready) => {
                        state.0 = false;
                        self.report(state.1.take().unwrap_or(progress));
                        return;
                    }
                    _ if progress.phase == IndexProgressPhase::Done => {
                        state.0 = false;
                        state.1 = None;
                    }
                    None if progress.phase == IndexProgressPhase::Indexing => {
                        state.1 = Some(progress.clone());
                        if state.0 {
                            return;
                        }
                    }
                    _ => {}
                }
                self.report(progress);
            })
        }

        /// Publishes one progress snapshot to the configured observer.
        pub fn report(&self, progress: IndexProgress) {
            (self.0)(progress);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn model_progress_suppresses_interleaved_indexing_and_restores_latest_counts() {
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let reporter = IndexProgressReporter::new(move |event| {
                captured.lock().expect("progress events lock").push(event);
            })
            .prioritize_model_progress();
            let indexing = IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: Some(10),
                files_indexed: Some(0),
                files_failed: Some(0),
                detail: None,
                embedding: None,
            };
            let model = |stage| IndexProgress {
                embedding: Some(IndexEmbeddingProgress {
                    stage: Some(stage),
                    ..Default::default()
                }),
                ..indexing.clone()
            };
            let preparing = model(IndexEmbeddingStage::Preparing);
            let downloading = model(IndexEmbeddingStage::Downloading);
            let latest = IndexProgress {
                files_indexed: Some(3),
                ..indexing.clone()
            };
            reporter.report(indexing.clone());
            reporter.report(preparing.clone());
            reporter.report(downloading.clone());
            reporter.report(latest.clone());
            assert_eq!(
                *events.lock().expect("progress events lock"),
                [indexing.clone(), preparing.clone(), downloading.clone()]
            );
            reporter.report(model(IndexEmbeddingStage::Ready));
            let next = IndexProgress {
                files_indexed: Some(4),
                ..indexing.clone()
            };
            reporter.report(next.clone());
            assert_eq!(
                *events.lock().expect("progress events lock"),
                [indexing, preparing, downloading, latest, next]
            );
        }
    }

    impl fmt::Debug for IndexProgressReporter {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("IndexProgressReporter(..)")
        }
    }

    impl PartialEq for IndexProgressReporter {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.0, &other.0)
        }
    }

    impl Eq for IndexProgressReporter {}

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IndexProgressPhase {
        Scanning,
        Indexing,
        Done,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IndexEmbeddingStage {
        Preparing,
        Downloading,
        Ready,
        Warning,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexEmbeddingProgress {
        pub concurrency: Option<usize>,
        pub max_concurrency: Option<usize>,
        pub retryable_failures: Option<usize>,
        pub stage: Option<IndexEmbeddingStage>,
        pub model: Option<String>,
        pub downloaded_bytes: Option<u64>,
        pub total_bytes: Option<u64>,
        pub message: Option<String>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexProgress {
        pub phase: IndexProgressPhase,
        pub files_total: Option<usize>,
        pub files_indexed: Option<usize>,
        pub files_failed: Option<usize>,
        pub detail: Option<String>,
        pub embedding: Option<IndexEmbeddingProgress>,
    }
}

/// Result types returned by [`crate::ZvecGrep::index`].
pub mod result {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexResult {
        pub files_scanned: usize,
        pub files_added: usize,
        pub files_modified: usize,
        pub files_pending: usize,
        pub files_deleted: usize,
        pub files_unchanged: usize,
        pub files_failed: usize,
        /// Files that could not be indexed; successful files remain published.
        #[serde(default)]
        pub failed_files: Vec<crate::api::info::result::FailedFile>,
        pub entities_created: usize,
        pub duration_micros: u64,
        pub timings: Vec<TimingEntry>,
        pub skipped: Vec<SkippedFile>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct SkippedFile {
        pub path: PathBuf,
        pub reason: SkippedFileReason,
        pub size_bytes: Option<u64>,
        pub limit_bytes: Option<u64>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SkippedFileReason {
        Empty,
        TooLarge,
        Unsupported,
        Binary,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct TimingEntry {
        pub name: String,
        pub duration_micros: u64,
        pub count: Option<u64>,
    }
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::{IndexOptions, progress::IndexProgressReporter};

    #[test]
    fn index_progress_reporter_is_runtime_only() {
        let request = IndexOptions {
            root: Some("/workspace".into()),
            on_progress: Some(IndexProgressReporter::new(|_| {})),
            signal: Some(CancellationToken::new()),
            ..IndexOptions::default()
        };

        let encoded = serde_json::to_string(&request).expect("index request should serialize");
        assert!(!encoded.contains("on_progress"));
        assert!(!encoded.contains("signal"));
        let decoded: IndexOptions =
            serde_json::from_str(&encoded).expect("index request should deserialize");
        assert!(decoded.on_progress.is_none());
        assert!(decoded.signal.is_none());
        assert_eq!(decoded.root, request.root);
    }

    #[test]
    fn rejects_removed_content_routing_in_index_requests() {
        for routes in [
            serde_json::json!({}),
            serde_json::json!({"text": "local/model"}),
        ] {
            let mut request = serde_json::to_value(IndexOptions::default()).expect("index request");
            assert!(request.get("embedding_routes").is_none());
            request["embedding_routes"] = routes;
            let error =
                serde_json::from_value::<IndexOptions>(request).expect_err("removed routing");
            assert!(error.to_string().contains("embedding_routes"));
        }
    }

    #[test]
    fn rejects_requests_with_the_removed_roots_field() {
        let mut request = serde_json::to_value(IndexOptions::default()).expect("index request");
        request["roots"] = serde_json::json!([{"path": "src"}]);
        let error = serde_json::from_value::<IndexOptions>(request).expect_err("legacy root list");
        assert!(error.to_string().contains("roots"));
    }
}
