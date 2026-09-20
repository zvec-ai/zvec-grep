mod content;
mod entity;
mod glob;
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
pub use content::ContentKind;
pub(crate) use content::{Content, ImageContent, TableCell, TableCellRole, TableContent};

// Entities.
pub(crate) use entity::{Entity, EntityContent, EntityId};

// Metadata.
pub(crate) use metadata::IndexField;
pub use metadata::{CodeMetadata, EntityMetadata, MarkdownMetadata, SymbolType};

// Fragments.
pub(crate) use entity::{EntityFragment, FragmentId, WindowFragment, validate_fragments};

// Path rules.
pub use glob::GlobRule;

// Workspaces.
pub use workspace::ScanRules;
pub(crate) use workspace::{FTS_CONFIG, FtsConfig, IndexDescriptor, IndexState, Workspace};

// Models.
pub(crate) use model::{EmbeddingModelInfo, Metric};
