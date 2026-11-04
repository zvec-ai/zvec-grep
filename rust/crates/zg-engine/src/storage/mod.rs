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

pub(crate) use store::{IndexStore, ReadView};

// Enable only when extraction and resolution maintain the graph during indexing.
pub(crate) const GRAPH_ENABLED: bool = false;
