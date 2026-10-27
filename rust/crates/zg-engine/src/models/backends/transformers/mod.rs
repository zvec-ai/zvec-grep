mod model;
#[cfg(any(feature = "cuda", test))]
mod native_runtime;

pub(crate) use model::TransformersEmbeddingModel;
