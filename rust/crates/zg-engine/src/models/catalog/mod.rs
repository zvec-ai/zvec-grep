//! Supported embedding model definitions and reference resolution.

mod entries;
mod resolution;
mod types;

pub(crate) use entries::get_embedding_model_catalog_entry;
#[cfg(test)]
pub(crate) use entries::list_embedding_models;
pub(crate) use resolution::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};
pub(crate) use types::{
    EmbeddingCatalogEntry, LlamaCppConfig, Model2VecConfig, QwenConfig, TransformersConfig,
};
