//! Workspace index location, manifest persistence, and process coordination.

pub(crate) mod build;
pub(crate) mod layout;
pub(crate) mod lock;
pub(crate) mod manifest;
pub(crate) mod registry;

/// Current format version of the workspace index contents.
pub(crate) const CURRENT_INDEX_VERSION: u32 = 5;
