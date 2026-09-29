use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Extensible evidence attached to edges and unresolved references.
pub(crate) type Metadata = Map<String, Value>;

/// Direction of a one-hop query relative to its endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    In,
    Out,
    #[default]
    Both,
}

/// Kinds of directed graph edges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EdgeKind {
    Contains,
    Calls,
    Imports,
    Extends,
    Implements,
}

impl EdgeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Calls => "calls",
            Self::Imports => "imports",
            Self::Extends => "extends",
            Self::Implements => "implements",
        }
    }
}

/// Evidence used to select an edge endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provenance {
    FileLocal,
    ImportScoped,
    PreferredFile,
    WorkspaceUnique,
}

impl Provenance {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FileLocal => "file_local",
            Self::ImportScoped => "import_scoped",
            Self::PreferredFile => "preferred_file",
            Self::WorkspaceUnique => "workspace_unique",
        }
    }
}

/// A resolved directed edge. Entity/file identity is supplied by the indexer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Edge {
    pub kind: EdgeKind,
    pub source: String,
    pub target: String,
    /// One-based relationship location in the file that produced this edge.
    pub line: Option<u32>,
    /// Zero-based source column, if known.
    pub column: Option<u32>,
    pub provenance: Provenance,
    pub metadata: Metadata,
}

/// Extraction output that still requires name resolution.
/// New snapshots insert unresolved references; the source is known and only the target requires resolution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PendingRef {
    /// Known source of the eventual edge, owned by the extraction file.
    pub from_node_id: String,
    pub reference_name: String,
    pub receiver_name: Option<String>,
    pub reference_kind: EdgeKind,
    pub arity: Option<u32>,
    /// One-based reference location in the file that produced this reference.
    pub line: u32,
    /// Zero-based source column.
    pub col: u32,
    pub metadata: Metadata,
    /// Candidate target IDs supplied by extraction or resolution preparation.
    pub candidates: Option<Vec<String>>,
    pub language: String,
    /// Last component of the referenced name, available for indexed lookup.
    pub name_tail: String,
}

/// A complete per-file snapshot. Node metadata is deliberately not stored here.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileGraph {
    /// Complete current entity IDs for ownership validation, not persisted as nodes.
    /// The zvec file key `f{file_id}` is an implicit local endpoint.
    pub entity_ids: Vec<String>,
    pub edges: Vec<Edge>,
    pub pending_refs: Vec<PendingRef>,
}

/// A pending reference with its database identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredPendingRef {
    pub id: i64,
    /// Matches zvec's `FileId`; the resolver retrieves file paths from zvec.
    pub file_id: u32,
    pub reference: PendingRef,
}

/// Page over pending references. Restart pagination after file mutations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
    pub provenance: Provenance,
}

/// Applied proposals and missing/already resolved references in an atomic batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResolutionStats {
    pub resolved: usize,
    pub stale: usize,
}
