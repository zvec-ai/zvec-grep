//! Process-level model ownership, caching, and compute resources.

mod compute;
mod concurrency;
pub(crate) mod llama_config;
mod manager;
pub(crate) mod onnx;
pub(crate) mod ort_config;

pub(super) use compute::ModelComputeRuntime;
pub(crate) use concurrency::{batch_concurrency, resolve_index_concurrency};
pub(crate) use manager::{ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest};
