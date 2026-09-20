//! Private embedding model implementations matching the TypeScript engine.

// Model definitions and selection.
mod catalog;
mod error;
mod factory;
mod resolution;
mod spi;

// Runtime and artifact management.
mod artifacts;
mod compute;
mod download_progress;
mod runtime;

// Embedding backends.
mod llama_cpp;
mod model2vec;
mod qwen;
mod transformers;

// Model catalog and reference resolution.
pub(crate) use catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};
pub(crate) use resolution::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};

// Runtime lifecycle.
pub(crate) use runtime::ModelRuntimeManager;
pub(crate) type ModelRuntimeLease = runtime::ModelRuntimeLease;
pub(crate) type ModelRuntimeRequest = runtime::ModelRuntimeRequest;
pub(crate) type ModelRuntimeSnapshot = runtime::ModelRuntimeSnapshot;

// Shared values live in domain; execution controls remain in models.
use crate::domain::{
    Content,
    model::{EmbeddingModelInfo, EmbeddingResult, ModelConfig},
};
pub(crate) use error::ModelError;
pub(crate) use spi::{EmbeddingConcurrencyDefaults, EmbeddingOptions, ModelProgressReporter};

impl runtime::ModelRuntimeManager {
    pub(crate) fn new() -> Self {
        Self::new_impl()
    }

    /// Returns a counted lease, reusing an existing runtime with the same key.
    pub(crate) fn acquire(
        &self,
        request: ModelRuntimeRequest,
    ) -> Result<ModelRuntimeLease, ModelError> {
        self.acquire_impl(request)
    }

    /// Stops new acquisitions and retires runtimes without active leases.
    pub(crate) fn close(&self) {
        self.close_impl();
    }

    pub(crate) fn snapshot(&self) -> ModelRuntimeSnapshot {
        self.snapshot_impl()
    }
}

impl runtime::ModelRuntimeRequest {
    pub(crate) fn new(
        reference: impl Into<String>,
        options: ModelConfig,
        embedding_concurrency: Option<usize>,
    ) -> Self {
        Self::new_impl(reference, options, embedding_concurrency)
    }
}

impl runtime::ModelRuntimeLease {
    pub(crate) fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        self.concurrency_defaults_impl()
    }

    pub(crate) fn info(&self) -> &EmbeddingModelInfo {
        self.info_impl()
    }

    pub(crate) async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
        progress: Option<ModelProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        self.embed_impl(inputs, options, progress).await
    }
}

#[cfg(test)]
mod tests;
