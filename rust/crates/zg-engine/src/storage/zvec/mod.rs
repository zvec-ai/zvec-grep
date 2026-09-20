//! File, entity, full-text and vector storage backed by zvec.
mod codec;
mod collections;
mod directories;
mod file_ids;
mod path;
mod storage;
pub(crate) mod types;

pub(crate) use storage::ZvecStorage;
