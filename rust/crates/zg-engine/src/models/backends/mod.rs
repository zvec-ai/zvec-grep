//! Concrete embedding backends and catalog-backed construction.

mod factory;
mod llama_cpp;
mod model2vec;
mod pp_doclayout;
mod qwen;
mod transformers;

pub(super) use factory::create_embedding_model;
pub(super) use factory::create_layout_model;
