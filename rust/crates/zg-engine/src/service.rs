use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::{IndexOptions, IndexResult},
        info::{InfoOptions, InfoResult},
    },
    models::ModelRuntimeManager,
    pipelines::{
        direct_search::DirectSearchService, indexed_search,
        indexing::service::WorkspaceIndexService,
    },
};

#[derive(Clone, Debug)]
pub(crate) struct EngineService {
    direct_search: DirectSearchService,
    indexing: WorkspaceIndexService,
    models: ModelRuntimeManager,
    closed: Arc<AtomicBool>,
}

impl EngineService {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            direct_search: DirectSearchService::new(),
            indexing: WorkspaceIndexService::new(),
            models: ModelRuntimeManager::new(),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Retrieves context from an index or, when `rg` is enabled, embedded ripgrep.
    ///
    /// # Errors
    ///
    /// Returns an engine error when the request is invalid or its selected
    /// retrieval mode is unavailable.
    pub(crate) async fn context(
        &self,
        options: ContextOptions,
    ) -> Result<ContextResult, EngineError> {
        self.ensure_open()?;
        options.validate_file_selection()?;
        if options.rg {
            self.direct_search.context(options).await
        } else {
            indexed_search::service::context(&self.indexing, &self.models, &options).await
        }
    }

    /// Creates or refreshes the workspace index.
    ///
    /// # Errors
    ///
    /// Returns an engine error when indexing fails or no storage backend is configured.
    pub(crate) async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
        self.ensure_open()?;
        self.indexing.index(&self.models, options).await
    }

    /// Returns workspace index metadata and status.
    ///
    /// # Errors
    ///
    /// Returns an engine error when workspace metadata or status cannot be read.
    pub(crate) async fn info(&self, options: InfoOptions) -> Result<InfoResult, EngineError> {
        self.ensure_open()?;
        self.indexing.info(options).await
    }

    /// Drops the persisted workspace index.
    ///
    /// # Errors
    ///
    /// Returns an engine error when index removal fails or no storage backend is configured.
    pub(crate) async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        self.ensure_open()?;
        std::future::ready(self.indexing.drop_index(&options)).await
    }

    /// Closes this service and rejects subsequent requests.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.models.close();
    }

    pub(crate) fn runtime_snapshot(&self) -> crate::EngineRuntimeSnapshot {
        let models = self.models.snapshot();
        crate::EngineRuntimeSnapshot {
            loaded_models: models.cached_runtimes,
            active_model_leases: models.active_leases,
        }
    }

    fn ensure_open(&self) -> Result<(), EngineError> {
        if self.closed.load(Ordering::Acquire) {
            Err(
                EngineError::resource_closed("engine instance has been closed")
                    .with_help("Create a new ZvecGrep instance before sending another request."),
            )
        } else {
            Ok(())
        }
    }
}
