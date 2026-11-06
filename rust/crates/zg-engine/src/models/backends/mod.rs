//! Concrete embedding backends and catalog-backed construction.

mod factory;
mod llama_cpp;
mod model2vec;
mod qwen;
mod static_int8;
mod transformers;

pub(super) use factory::create_embedding_model;

#[cfg(test)]
pub(super) use transformers::TransformersEmbeddingModel;
