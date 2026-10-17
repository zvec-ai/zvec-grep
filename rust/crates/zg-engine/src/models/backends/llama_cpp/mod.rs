#[cfg(any(feature = "cuda", test))]
mod backend_loading;
mod model;

pub(crate) use model::LlamaCppEmbeddingModel;
