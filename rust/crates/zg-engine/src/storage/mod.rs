//! Persistent index tables and their coordinated lifecycle.
mod directories;
mod entities;
mod files;
mod fragments;
mod path;
mod record;
mod store;
pub(crate) mod types;
mod zvec;

pub(crate) use store::IndexStore;
