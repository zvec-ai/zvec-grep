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
    #[allow(clippy::struct_excessive_bools)]
    pub struct IndexOptions {
        /// Workspace whose index is being updated. `None` uses the working directory.
        pub root: Option<PathBuf>,
        pub roots: Vec<RootPath>,
        pub rebuild: bool,
        pub reset_paths: bool,
        /// Normalized watcher changes for a narrow incremental index operation.
        /// An empty list means normal discovery rather than "no work".
        pub changes: Vec<WorkspaceChange>,
        pub discovery: DiscoveryOptions,
        pub embedding: Option<EmbeddingModelSpec>,
        /// Maximum embedding batch tasks for this index operation.
        /// The model default is used when omitted.
        pub embedding_concurrency: Option<usize>,
        /// Allows remote embedding for this operation without persisting a grant.
        #[serde(default)]
        pub allow_remote: bool,
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

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct DiscoveryOptions {
        pub include_paths: Vec<String>,
        pub exclude_paths: Vec<String>,
        pub globs: Vec<String>,
        pub insensitive_globs: Vec<String>,
        pub file_types: Vec<String>,
        pub excluded_file_types: Vec<String>,
        pub hidden: bool,
        pub no_ignore: bool,
        pub ignore_files: Vec<PathBuf>,
        pub max_depth: Option<usize>,
        pub max_file_size_bytes: Option<u64>,
        pub follow: bool,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RootPath {
        pub path: PathBuf,
        pub recursive: bool,
        pub discovery: DiscoveryOptions,
    }

    /// A normalized filesystem change relative to its [`RootPath`].
    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind", content = "path")]
    pub enum WorkspaceChange {
        Upsert(PathBuf),
        Delete(PathBuf),
        RescanDirectory(PathBuf),
        DeletePrefix(PathBuf),
        Rescan,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Device {
        Auto,
        Cpu,
        Metal,
        Vulkan,
        Cuda,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct EmbeddingModelSpec {
        pub reference: String,
        pub revision: Option<String>,
        pub cache_dir: Option<PathBuf>,
        pub endpoint: Option<String>,
        pub device: Device,
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
        pub generation: u64,
        pub files_scanned: usize,
        pub files_added: usize,
        pub files_modified: usize,
        pub files_pending: usize,
        pub files_deleted: usize,
        pub files_unchanged: usize,
        pub files_failed: usize,
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
}
