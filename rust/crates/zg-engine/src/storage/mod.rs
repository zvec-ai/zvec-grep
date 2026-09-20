//! Canonical directories, files, and entity/fragment bundles backed by zvec.
//! Each configured embedding model owns one disjoint fragment search collection,
//! containing both the shared-config full-text index and its vector index.
//!
//! File IDs are cached from source records. The existing pending-file journal
//! owns recovery; no workspace-level identity database or allocation log exists.
mod backend;
mod codec;
mod directories;
mod file_ids;
mod path;
mod pending;
pub(crate) mod spi;
mod zvec;

pub(crate) use backend::ZvecStorageFactory;
