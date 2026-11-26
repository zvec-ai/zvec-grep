//! Workspace discovery, extraction, embedding, and storage orchestration.

pub(crate) mod embedding_policy;
mod graph_assembly;
pub(crate) mod graph_resolver;
mod input_budget;
mod model_progress;
pub(crate) mod pipeline;
mod preprocessor;
mod routing;
pub(crate) mod service;
pub(crate) use routing::embedding_requests;
pub(crate) mod setup;
pub(crate) mod storage;

#[cfg(test)]
mod resolution_tests;
