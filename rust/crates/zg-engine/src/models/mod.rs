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
pub(crate) use spi::{EmbeddingConcurrencyDefaults, EmbeddingOptions, ModelProgressReporter};

#[cfg(test)]
mod tests;
