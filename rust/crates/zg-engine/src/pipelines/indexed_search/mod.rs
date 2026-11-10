//! Search over an existing workspace index using FTS and vectors.

pub(crate) mod context;
pub(super) mod format_filter;
pub(super) mod path_filter;
mod pipeline;
mod ranking;
pub(crate) mod service;
pub(crate) mod storage;

pub(crate) mod writer;
