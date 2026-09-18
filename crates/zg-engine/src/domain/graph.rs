//! Cross-module graph extraction contracts.
//!
//! Mirrors the TypeScript `src/engine/graph/types.ts` contract: the
//! extraction layer produces [`FileGraphResult`] per code file, the pipeline
//! carries it to the graph persistence layer, and the resolver consumes the
//! buffered pending references.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::metadata::SymbolType;

/// Edge kinds produced by the extraction layer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GraphEdgeKind {
    Contains,
    Calls,
    Imports,
    Extends,
    Implements,
}

/// Kinds of buffered name references. Each pending ref is resolved into the
/// [`GraphEdgeKind`] with the same name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GraphRefKind {
    Calls,
    Imports,
    Extends,
    Implements,
}

/// Evidence provenance recorded on persisted edges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EdgeProvenance {
    FileLocal,
    ImportScoped,
    PreferredFile,
    WorkspaceUnique,
}

/// An edge ready for persistence after partition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct FileEdge {
    pub kind: GraphEdgeKind,
    pub source: String,
    pub target: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub provenance: EdgeProvenance,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Status of a persisted pending reference.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingRefStatus {
    Pending,
    Resolved,
    Failed,
}

/// A reference that could not be resolved within its own file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct PendingRef {
    pub owner_id: String,
    pub ref_name: String,
    pub receiver_name: Option<String>,
    pub ref_kind: GraphRefKind,
    pub arity: Option<usize>,
    pub line: usize,
    pub column: usize,
    pub status: PendingRefStatus,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// A graph node derived from an indexed entity fragment. The pipeline layer
/// builds these from the existing fragment results; the extraction graph
/// layer only defines the shape.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileGraphNode {
    pub id: String,
    pub kind: SymbolType,
    pub name: Option<String>,
    /// `scope::name` breadcrumb joined with `::`, or the bare name.
    pub qualified_name: String,
    pub language: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub arity: Option<usize>,
    pub visibility: Option<String>,
    pub is_exported: bool,
}

/// Per-file graph extraction output, assembled after the walk and partition.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct FileGraphResult {
    pub nodes: Vec<FileGraphNode>,
    pub edges: Vec<FileEdge>,
    pub pending_refs: Vec<PendingRef>,
}
