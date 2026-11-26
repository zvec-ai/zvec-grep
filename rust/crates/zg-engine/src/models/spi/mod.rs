//! Backend-neutral model contracts shared by runtimes and implementations.

mod embedding;
mod layout;

pub(super) use super::error::ModelError;
pub(crate) use embedding::{
    EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions,
    EmbeddingTraceHeaders, ModelExecutionStats, ModelProgressReporter, input_text,
    same_execution_scope, validate_inputs, validate_result,
};
pub(crate) use layout::{
    LayoutImage, LayoutKind, LayoutModel, LayoutModelConfig, LayoutOptions, LayoutRegion,
};
