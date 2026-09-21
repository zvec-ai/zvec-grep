//! Workspace index location, manifest persistence, and process coordination.

pub(crate) mod build;
pub(crate) mod layout;
pub(crate) mod lock;
pub(crate) mod manifest;
pub(crate) mod registry;

/// One format version covers the manifest, storage schema, records and indexing semantics.
/// Version 1 belongs to the released Node.js implementation; incompatible indexes require rebuild.
pub(crate) const CURRENT_INDEX_VERSION: u32 = 2;
