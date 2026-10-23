//! Workspace discovery, extraction, embedding, and storage orchestration.

pub(crate) mod graph_assembly;
pub(crate) mod graph_sink;
mod input_budget;
mod model_progress;
pub(crate) mod pipeline;
pub(crate) mod service;
pub(crate) mod setup;
pub(crate) mod storage;
