//! Native filesystem scanner and watcher adapters.

mod api;
mod change_set;
mod error;
mod scanner;
mod watcher;

pub use api::{
    AllowAllPaths, ClockPort, DiscoveredFile, PathPolicy, ReadBatchRequest, RootSpec,
    ScanDiagnostics, ScanRequest, ScanSnapshot, SkippedByReason, SkippedFile, SkippedFileReason,
    SourceFile, TaskControl, WatchRequest, WorkspaceChange, WorkspaceChangeBatch,
    WorkspaceScannerPort, WorkspaceWatchSessionPort, WorkspaceWatcherFactoryPort,
};
pub use error::{HostError, HostErrorSite};
pub use scanner::NativeScanner;
pub use watcher::{NativeWatcherConfig, NativeWatcherFactory};
