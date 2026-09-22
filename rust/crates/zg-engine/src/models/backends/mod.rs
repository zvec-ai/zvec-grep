//! Concrete embedding backends and catalog-backed construction.

mod factory;
mod llama_cpp;
mod model2vec;
mod qwen;
mod transformers;

pub(super) use factory::create_embedding_model;
