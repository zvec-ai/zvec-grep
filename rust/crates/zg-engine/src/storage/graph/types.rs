use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::domain::{EdgeProvenance, PendingRef};

/// Extensible evidence attached to edges and unresolved references.
pub(crate) type Metadata = BTreeMap<String, Value>;

/// Direction of a one-hop query relative to its endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    In,
    Out,
    #[default]
    Both,
}

/// A pending reference with its database identity and resolution evidence.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredPendingRef {
    pub id: i64,
    /// Matches zvec's `FileId`; the resolver retrieves file paths from zvec.
    pub file_id: u32,
    pub reference: PendingRef,
    /// Ambiguity candidates recorded by the resolver; cleared on invalidation
    /// because they describe the previous snapshot.
    pub candidates: Option<Vec<String>>,
    /// Language of the owning file (`rust`, `python`, ...), the resolver's
    /// cross-language gate.
    pub language: String,
    /// Last component of the referenced name, the name-index key.
    pub name_tail: String,
}

/// One workspace name-index hit for a symbol name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NameIndexEntry {
    pub entity_id: String,
    pub file_id: u32,
    pub qualified_name: String,
    pub is_exported: bool,
    pub arity: Option<u32>,
    /// Pure declaration without a body (C/C++ prototypes).
    pub is_declaration: bool,
}

/// Name-index record for one entity. Entities without a symbol name are
/// omitted; `name` shares the namespace with the pending-ref `name_tail`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityRecord {
    pub entity_id: String,
    pub name: String,
    pub qualified_name: String,
    pub is_exported: bool,
    pub arity: Option<u32>,
    pub is_declaration: bool,
}

/// One import statement of a file as the resolver sees it: the extracted
/// module specifier plus the statement text, which carries the `as` alias
/// bindings the specifier alone loses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportReference {
    /// Module specifier from extraction (`./left`, `declare`, `left::work`).
    pub specifier: String,
    /// Import statement text as written in the source.
    pub raw_text: Option<String>,
}

/// A variable/parameter type binding declared by one entity
/// (`var x = new X()` binds `x` to `X` inside its function).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeclaredBinding {
    /// The bound name as the source spells it (the call's receiver).
    pub name: String,
    /// Declared receiver type (`X`).
    pub bound_type: String,
    /// One-based line of the declaring site; rebindings of the same name
    /// shadow earlier ones at later lines.
    pub line: u32,
}

/// Page over pending references. Restart pagination after file mutations.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PendingRefPage {
    pub refs: Vec<StoredPendingRef>,
    pub next_cursor: Option<i64>,
}

/// A resolver's proposed edge. The caller must validate the target in zvec and
/// serialize the entire read, resolution and writeback cycle with workspace writes/deletions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Resolution {
    /// ID of the pending row in `edges`.
    pub ref_id: i64,
    pub target_id: String,
    /// Must be a cross-file provenance; `FileLocal` is rejected.
    pub provenance: EdgeProvenance,
}

/// Applied proposals and missing/already resolved references in an atomic batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResolutionStats {
    pub resolved: usize,
    pub stale: usize,
}
