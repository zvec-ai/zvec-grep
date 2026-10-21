//! Workspace index location, manifest persistence, and process coordination.

pub(crate) mod build;
pub(crate) mod layout;
pub(crate) mod lock;
pub(crate) mod manifest;
pub(crate) mod registry;

/// Compatibility version of the workspace index contents.
///
/// Version 3 stores embedding vectors as IEEE binary16. Earlier Rust indexes
/// (version 2) stored FP32 vectors and are not readable; open and search ask
/// for `zg --index --rebuild`.
pub(crate) const CURRENT_INDEX_VERSION: u32 = 3;
