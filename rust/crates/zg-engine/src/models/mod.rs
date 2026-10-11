//! Private embedding model implementations matching the TypeScript engine.

// Model definitions and selection.
mod catalog;
mod error;
mod spi;

// Runtime, artifacts, and concrete backends.
mod artifacts;
mod backends;
mod runtime;

// Model catalog and reference resolution.
pub(crate) use catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};
pub(crate) use catalog::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};

// Runtime lifecycle.
pub(crate) use error::ModelError;
pub(crate) use runtime::{ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest};
#[cfg(test)]
pub(crate) use spi::EmbeddingModel;
pub(crate) use spi::{
    EmbeddingConcurrencyDefaults, EmbeddingOptions, EmbeddingPrepareOptions, ModelProgressReporter,
};

#[cfg(test)]
pub(crate) fn embeddinggemma2_test_model(cache_dir: std::path::PathBuf) -> Box<dyn EmbeddingModel> {
    use crate::domain::model::{Device, ModelConfig};

    Box::new(backends::LlamaCppEmbeddingModel::new(
        catalog::embeddinggemma2_test_config(),
        ModelConfig {
            cache_dir: Some(cache_dir),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        runtime::ModelComputeRuntime::shared(),
    ))
}

#[cfg(test)]
mod tests;
