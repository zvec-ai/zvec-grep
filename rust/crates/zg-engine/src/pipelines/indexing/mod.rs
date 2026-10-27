//! Workspace discovery, extraction, embedding, and storage orchestration.

mod graph_assembly;
pub(crate) mod graph_resolver;
mod input_budget;
mod model_progress;
pub(crate) mod pipeline;
pub(crate) mod service;
pub(crate) mod setup;
pub(crate) mod storage;

#[cfg(test)]
mod resolution_tests;
