//! High-level Rust API for zvec-grep.
//!
//! Create one [`ZvecGrep`] for a process and call its typed methods. Command
//! dispatch, transport envelopes and adapter composition are not part of this
//! public API.

pub mod api;
pub mod authorization;
pub mod config;
mod domain;
mod error;
mod extraction;
mod file_selection;
mod lexical;
mod models;
mod pipelines;
mod service;
mod storage;
mod utils;
mod workspace;

use api::{
    context::{ContextOptions, ContextResult},
    index::{IndexOptions, IndexResult},
    info::{InfoOptions, InfoResult},
};

pub use error::{EngineError, EngineResult, ErrorReport, ErrorSite};

/// Process-level resource counts used by resident daemon diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EngineRuntimeSnapshot {
    pub loaded_models: usize,
    pub active_model_leases: usize,
}

/// Reusable zvec-grep engine. One instance may serve multiple workspaces.
#[derive(Clone, Debug)]
pub struct ZvecGrep {
    service: service::EngineService,
}

#[allow(clippy::missing_errors_doc)]
impl ZvecGrep {
    #[must_use]
    pub fn new() -> Self {
        Self {
            service: service::EngineService::new(),
        }
    }

    pub async fn context(&self, options: ContextOptions) -> EngineResult<ContextResult> {
        self.service
            .context(options)
            .await
            .map_err(|error| error.report_here())
    }

    /// Queries an index after a resident coordinator has completed its refresh.
    /// The caller must drain delivered watcher events and wait for indexing
    /// before each call. OS notifications still in flight are outside that
    /// guarantee. No disk freshness scan is repeated here; the request's
    /// refresh policy still controls whether an active writer may be borrowed.
    pub async fn context_after_refresh(
        &self,
        options: ContextOptions,
    ) -> EngineResult<ContextResult> {
        self.service
            .context_after_refresh(options)
            .await
            .map_err(|error| error.report_here())
    }

    pub async fn index(&self, options: IndexOptions) -> EngineResult<IndexResult> {
        self.service
            .index(options)
            .await
            .map_err(|error| error.report_here())
    }

    pub async fn info(&self, options: InfoOptions) -> EngineResult<InfoResult> {
        self.service
            .info(options)
            .await
            .map_err(|error| error.report_here())
    }

    /// Watches a workspace using its saved, resolved file selection.
    /// The caller owns the returned session and schedules incremental indexing.
    pub async fn watch_workspace(
        &self,
        root: &std::path::Path,
        index_dir: Option<&std::path::Path>,
        control: &zg_host_native::TaskControl,
    ) -> EngineResult<std::sync::Arc<dyn zg_host_native::WorkspaceWatchSessionPort>> {
        use zg_host_native::WorkspaceWatcherFactoryPort;

        let info = self
            .info(InfoOptions {
                root: Some(root.to_path_buf()),
                index_dir: index_dir.map(std::path::Path::to_path_buf),
                include_status: false,
            })
            .await?;
        info.compatibility.ensure_compatible()?;
        if index_dir.is_some() && workspace::layout::canonical_path(root)? != info.root {
            return Err(EngineError::resource_busy(
                "workspace source directory changed; reopen the workspace before starting its watcher",
            ));
        }
        if !info.indexed {
            return Err(EngineError::invalid_argument(
                "workspace must have an index before it can be watched",
            ));
        }
        let workspace = info
            .workspace_index
            .ok_or_else(|| EngineError::invalid_argument("workspace configuration is missing"))?;
        let binding = bind_watch_source(&info.home, &workspace.root)?;
        let root = file_selection::ScanPolicy::root_spec(
            &workspace.root,
            &workspace.scan,
            Some(&info.home),
        )?;
        let session = zg_host_native::NativeWatcherFactory::default()
            .watch(&zg_host_native::WatchRequest { root }, control)
            .await
            .map_err(pipelines::indexing::pipeline::map_host_error)?;
        Ok(std::sync::Arc::new(BoundWatchSession {
            session,
            binding: std::sync::Mutex::new(Some(binding)),
        }))
    }

    pub async fn drop_index(&self, options: InfoOptions) -> EngineResult<bool> {
        self.service
            .drop_index(options)
            .await
            .map_err(|error| error.report_here())
    }

    pub fn close(&self) {
        self.service.close();
    }

    /// Reuses index read handles between requests, closing them after 60 idle seconds.
    /// Workspace writes retire cached handles before opening writable storage.
    /// Repeated calls are idempotent; ordinary engine instances remain uncached.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine is closed or cache maintenance cannot start.
    pub fn enable_read_session_cache(&self) -> EngineResult<()> {
        self.service
            .enable_read_session_cache()
            .map_err(|error| error.report_here())
    }

    #[must_use]
    pub fn runtime_snapshot(&self) -> EngineRuntimeSnapshot {
        self.service.runtime_snapshot()
    }
}

fn bind_watch_source(
    home: &std::path::Path,
    root: &std::path::Path,
) -> EngineResult<workspace::lock::FileLock> {
    let binding = workspace::lock::bind_source(home, root)?;
    // Inspection releases its lease; recheck after acquiring the watcher's lease.
    if workspace::manifest::read_source_root(home)?.as_deref() != Some(root) {
        return Err(EngineError::resource_busy(
            "workspace source directory changed; reopen the workspace before starting its watcher",
        ));
    }
    Ok(binding)
}

struct BoundWatchSession {
    session: std::sync::Arc<dyn zg_host_native::WorkspaceWatchSessionPort>,
    binding: std::sync::Mutex<Option<workspace::lock::FileLock>>,
}

#[async_trait::async_trait]
impl zg_host_native::WorkspaceWatchSessionPort for BoundWatchSession {
    async fn flush(&self) -> Result<(), zg_host_native::HostError> {
        self.session.flush().await
    }

    async fn flush_pending(&self) -> Result<(), zg_host_native::HostError> {
        self.session.flush_pending().await
    }

    async fn next_changes(
        &self,
        control: &zg_host_native::TaskControl,
    ) -> Result<zg_host_native::WorkspaceChangeBatch, zg_host_native::HostError> {
        self.session.next_changes(control).await
    }

    async fn close(&self) -> Result<(), zg_host_native::HostError> {
        self.session.close().await?;
        self.binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(())
    }
}

impl Default for ZvecGrep {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{EngineRuntimeSnapshot, ZvecGrep};

    #[tokio::test]
    async fn watcher_rejects_source_rebound_after_inspection() {
        use crate::api::{
            index::{
                IndexOptions,
                options::{Device, EmbeddingModelSpec},
            },
            info::InfoOptions,
        };

        let directory = tempfile::tempdir().expect("workspace package");
        let parent = directory.path().canonicalize().expect("parent");
        let root = parent.join("materials");
        let replacement = parent.join("replacement");
        let home = parent.join("index");
        std::fs::create_dir(&root).expect("source");
        std::fs::create_dir(&replacement).expect("replacement source");
        let engine = ZvecGrep::new();
        engine
            .index(IndexOptions {
                root: Some(root.clone()),
                index_dir: Some(home.clone()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: Device::Cpu,
                }),
                endpoint: Some("http://127.0.0.1:1/empty-index".into()),
                api_key: Some("fixture-key".into()),
                allow_remote: true,
                ..IndexOptions::default()
            })
            .await
            .expect("empty index without model inference");
        let inspected = engine
            .info(InfoOptions {
                root: Some(root),
                index_dir: Some(home.clone()),
                include_status: false,
            })
            .await
            .expect("initial watcher inspection");
        engine
            .index(IndexOptions {
                root: Some(replacement.clone()),
                index_dir: Some(home.clone()),
                allow_remote: true,
                ..IndexOptions::default()
            })
            .await
            .expect("explicit rebind before watcher acquires its lease");
        let error = super::bind_watch_source(&home, &inspected.root)
            .expect_err("stale watcher must not start");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_BUSY);
        let binding = super::bind_watch_source(&home, &replacement)
            .expect("failed stale watcher releases its binding");
        drop(binding);
        engine.close();
    }

    #[test]
    fn fresh_engine_reports_an_empty_process_runtime() {
        assert_eq!(
            ZvecGrep::new().runtime_snapshot(),
            EngineRuntimeSnapshot {
                loaded_models: 0,
                active_model_leases: 0,
            }
        );
    }
}
