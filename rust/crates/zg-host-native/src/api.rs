use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::HostError;

/// Selection decisions supplied by the engine, shared by scanning and watching.
pub trait PathPolicy: Send + Sync + fmt::Debug {
    /// Whether a candidate file belongs in discovery.
    ///
    /// # Errors
    /// Returns an error when selection rules cannot be evaluated.
    fn includes_file(&self, absolute_path: &Path) -> Result<bool, HostError>;

    /// Whether traversal or watch registration may enter a directory.
    ///
    /// # Errors
    /// Returns an error when selection rules cannot be evaluated.
    fn can_descend(&self, absolute_path: &Path) -> Result<bool, HostError>;

    /// Invalidate changed selection rules and return a root-relative rescan scope.
    ///
    /// # Errors
    /// Returns an error when changed rules cannot be reloaded.
    fn control_file_changed(&self, _absolute_path: &Path) -> Result<Option<PathBuf>, HostError> {
        Ok(None)
    }

    /// Rule files that must remain observable, including files outside the root.
    fn control_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Discard cached rules when event loss or reconciliation requires a fresh view.
    ///
    /// # Errors
    /// Returns an error when the policy cannot invalidate its cached state.
    fn invalidate(&self) -> Result<(), HostError> {
        Ok(())
    }
}

/// A policy for callers that do not need path filtering.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllPaths;

impl PathPolicy for AllowAllPaths {
    fn includes_file(&self, _absolute_path: &Path) -> Result<bool, HostError> {
        Ok(true)
    }

    fn can_descend(&self, _absolute_path: &Path) -> Result<bool, HostError> {
        Ok(true)
    }
}

#[derive(Clone, Debug)]
pub struct RootSpec {
    pub path: PathBuf,
    pub recursive: bool,
    pub follow: bool,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,
    pub policy: Arc<dyn PathPolicy>,
}

impl RootSpec {
    #[must_use]
    pub fn new(path: PathBuf, policy: Arc<dyn PathPolicy>) -> Self {
        Self {
            path,
            recursive: true,
            follow: false,
            max_depth: None,
            max_file_size_bytes: None,
            policy,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub reason: SkippedFileReason,
    pub size_bytes: Option<u64>,
    pub limit_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkippedFileReason {
    Empty,
    TooLarge,
    Unsupported,
    Binary,
}

/// Cancellation and deadline state for scanner and watcher operations.
#[derive(Clone)]
pub struct TaskControl {
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}

impl TaskControl {
    #[must_use]
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline: None,
        }
    }
}

impl Default for TaskControl {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

impl fmt::Debug for TaskControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskControl")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("deadline", &self.deadline)
            .finish()
    }
}

pub trait ClockPort: Send + Sync {
    fn now_epoch_ms(&self) -> u64;
}

#[derive(Clone, Debug, Default)]
pub struct ScanRequest {
    pub roots: Vec<RootSpec>,
    /// Absolute file or directory paths that bound discovery within `roots`.
    /// An empty list scans every configured root.
    pub scope_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredFile {
    pub root: PathBuf,
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkippedByReason {
    pub empty: usize,
    pub too_large: usize,
    pub unsupported: usize,
    pub binary: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanDiagnostics {
    pub skipped_files: usize,
    pub skipped_by_reason: SkippedByReason,
    /// Bounded diagnostic samples; production scanners retain at most 20.
    pub skipped_samples: Vec<SkippedFile>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanSnapshot {
    pub files: Vec<DiscoveredFile>,
    pub diagnostics: ScanDiagnostics,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadBatchRequest {
    pub files: Vec<DiscoveredFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    pub root: PathBuf,
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
    pub source_fingerprint: String,
}

/// Metadata-first workspace discovery and bounded source reads.
///
/// Discovery must not read complete file contents. Callers compare the source
/// fingerprints with indexed file state, then request bytes only for files
/// that need extraction.
#[async_trait]
pub trait WorkspaceScannerPort: Send + Sync {
    async fn discover(
        &self,
        request: &ScanRequest,
        control: &TaskControl,
    ) -> Result<ScanSnapshot, HostError>;

    async fn read_batch(
        &self,
        request: &ReadBatchRequest,
        control: &TaskControl,
    ) -> Result<Vec<SourceFile>, HostError>;
}

#[derive(Clone, Debug)]
pub struct WatchRequest {
    pub root: RootSpec,
}

/// Normalized changes relative to the watched root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceChange {
    Upsert(PathBuf),
    Delete(PathBuf),
    RescanDirectory(PathBuf),
    DeletePrefix(PathBuf),
    Rescan,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceChangeBatch {
    pub changes: Vec<WorkspaceChange>,
}

/// Creates one resident watch session for a workspace root.
#[async_trait]
pub trait WorkspaceWatcherFactoryPort: Send + Sync {
    async fn watch(
        &self,
        request: &WatchRequest,
        control: &TaskControl,
    ) -> Result<Arc<dyn WorkspaceWatchSessionPort>, HostError>;
}

/// A daemon-owned watch session.
///
/// Native file rename events are normalized into Delete plus Upsert. Directory
/// changes retain their scope through `RescanDirectory` or `DeletePrefix`.
/// Native queue overflow and watcher recovery are normalized into one Rescan.
#[async_trait]
pub trait WorkspaceWatchSessionPort: Send + Sync {
    /// Publishes pending changes before returning. Backends may request a full rescan.
    async fn flush(&self) -> Result<(), HostError> {
        Ok(())
    }

    async fn next_changes(&self, control: &TaskControl) -> Result<WorkspaceChangeBatch, HostError>;

    async fn close(&self) -> Result<(), HostError>;
}
