mod content;
mod entity;
mod file_filter;
mod graph;
mod metadata;
pub(crate) mod model;
mod source;
mod workspace;

// Source files.
pub(crate) use source::{FileId, FileIndexStatus, FileRecord, FileSnapshot};

// Source directories and shared path invariants.
pub(crate) use source::{DirectoryId, DirectoryRecord, SourcePath};

// File formats.
pub use source::{FileCategory, FileFormat};

// Source ranges.
pub(crate) use source::{ByteRange, SourceRange, TextRange};

// Content.
pub(crate) use content::{Content, ImageContent, TableCell, TableCellRole, TableContent};

// Entities.
pub(crate) use entity::{Entity, EntityContent, EntityId};

// Metadata.
pub(crate) use metadata::IndexField;
pub use metadata::{CodeMetadata, EntityMetadata, MarkdownMetadata, SymbolType};

// Graph extraction contracts. The walk-time collectors and the persistence
// layer are still pending, so most types have no consumers yet.
#[allow(unused_imports)]
pub(crate) use graph::{
    EdgeProvenance, FileEdge, FileGraphNode, FileGraphResult, GraphEdgeKind, GraphRefKind,
    PendingRef, PendingRefStatus,
};

// Fragments.
pub(crate) use entity::{EntityFragment, FragmentId, WindowFragment, validate_fragments};

// File filters.
pub use file_filter::{FileFilter, GlobRule};

// Workspaces.
pub(crate) use workspace::{FTS_CONFIG, FtsConfig, IndexDescriptor, IndexState, Workspace};

// Models.
pub(crate) use model::{EmbeddingModelInfo, Metric};
