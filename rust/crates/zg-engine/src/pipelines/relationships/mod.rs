//! Query call relationships through canonical entities and the persisted graph.

mod pipeline;
mod rust_modules;
pub(crate) mod service;
mod storage;

#[cfg(test)]
mod tests;
