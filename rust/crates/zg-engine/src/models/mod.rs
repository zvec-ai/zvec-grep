//! Private embedding and layout model contracts, runtimes, and implementations.

// Model definitions and selection.
mod catalog;
mod error;
mod spi;

// Runtime, artifacts, and concrete backends.
mod artifacts;
mod backends;
mod runtime;

// Model catalog and reference resolution.
pub(crate) use artifacts::model_is_cached;
pub(crate) use catalog::list_embedding_models;
pub(crate) use catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};
pub(crate) use catalog::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};

// Runtime lifecycle.
pub(crate) use error::ModelError;
pub(crate) use runtime::{ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest};
pub(crate) use runtime::{batch_concurrency, resolve_index_concurrency};
pub(crate) use spi::{
    EmbeddingConcurrencyDefaults, EmbeddingOptions, EmbeddingPrepareOptions, ModelProgressReporter,
};
pub(crate) use spi::{LayoutImage, LayoutKind, LayoutModelConfig, LayoutOptions};

#[cfg(test)]
mod tests;
