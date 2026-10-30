#[cfg(test)]
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::domain::{EdgeProvenance, PendingRef};

/// Extensible evidence attached to edges and unresolved references.
pub(crate) type Metadata = BTreeMap<String, Value>;

/// Direction of a one-hop query relative to its endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg(test)]
pub(crate) enum Direction {
    In,
    Out,
    #[default]
    Both,
}

/// A pending reference with its database identity.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredPendingRef {
    pub id: i64,
    /// Matches zvec's `FileId`; the resolver retrieves file paths from zvec.
    pub file_id: u32,
    pub reference: PendingRef,
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
