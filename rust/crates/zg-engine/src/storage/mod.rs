//! Persistent index tables and their coordinated lifecycle.
mod directories;
mod entities;
mod files;
mod fragments;
pub(crate) mod graph;
mod path;
pub(crate) mod read_session;
mod record;
mod store;
pub(crate) mod types;
mod zvec;

pub(crate) use store::{IndexStore, ReadView, ResolutionSnapshot};

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum VectorEngine {
    #[default]
    #[serde(rename = "zvec_hnsw_fp16_v1")]
    Hnsw,
    #[serde(rename = "zvec_diskann_fp16_v1")]
    Diskann,
}
