//! Shared domain types for source content, indexing, and search.

mod entity;
mod glob;
mod metadata;
pub(crate) mod model;
mod source;
mod workspace;

// Entities.
pub(crate) use entity::{Entity, EntityFragment, EntityId, FragmentId};

// Glob rules.
pub use glob::GlobRule;

// Metadata.
pub(crate) use metadata::IndexField;
pub use metadata::{CodeMetadata, EntityMetadata, MarkdownMetadata, SymbolType};

// Models.
pub(crate) use model::{EmbeddingModelInfo, Metric};

// Sources.
pub(crate) use source::{
    ByteRange, Content, DirectoryId, DirectoryRecord, FileId, FileIndexStatus, FileRecord,
    FileSnapshot, ImageContent, Range, SourcePath, TableCell, TableCellRole, TableContent,
    TextRange,
};
pub use source::{ContentKind, FileCategory, FileFormat};

// Workspaces.
pub use workspace::ScanRules;
pub(crate) use workspace::{FTS_CONFIG, FtsConfig, IndexDescriptor, IndexState, Workspace};
