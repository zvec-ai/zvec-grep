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
mod indexing;
mod lexical;
mod models;
mod search;
mod service;
mod storage;
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

    pub async fn drop_index(&self, options: InfoOptions) -> EngineResult<bool> {
        self.service
            .drop_index(options)
            .await
            .map_err(|error| error.report_here())
    }

    pub fn close(&self) {
        self.service.close();
    }

    #[must_use]
    pub fn runtime_snapshot(&self) -> EngineRuntimeSnapshot {
        self.service.runtime_snapshot()
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
