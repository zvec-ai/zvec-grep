//! Process-level model ownership, caching, and compute resources.

mod compute;
mod manager;

pub(super) use compute::ModelComputeRuntime;
pub(crate) use manager::{ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest};
