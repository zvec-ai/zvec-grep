//! Shared domain types for source content, indexing, and search.

mod content;
mod entity;
mod glob;
mod metadata;
pub(crate) mod model;
mod source;
mod workspace;

// Content.
pub use content::Content;
pub use content::ContentKind;
pub use content::ImageContent;

// Entities.
pub(crate) use entity::Entity;
pub(crate) use entity::EntityFragment;
pub(crate) use entity::EntityId;
pub(crate) use entity::FragmentId;

// Glob rules.
pub use glob::GlobRule;

// Metadata.
pub use metadata::CodeMetadata;
pub use metadata::EntityMetadata;
pub(crate) use metadata::IndexField;
pub use metadata::MarkdownMetadata;
pub use metadata::SymbolType;

// Models.
pub(crate) use model::EmbeddingMetric;
pub(crate) use model::EmbeddingModelInfo;

// Sources.
pub(crate) use source::ByteRange;
pub(crate) use source::DirectoryId;
pub(crate) use source::DirectoryRecord;
pub use source::FileCategory;
pub use source::FileFormat;
pub(crate) use source::FileId;
pub(crate) use source::FileIndexStatus;
pub(crate) use source::FileRecord;
pub(crate) use source::FileSnapshot;
pub(crate) use source::Range;
pub(crate) use source::SourcePath;
pub(crate) use source::TextRange;

// Workspaces.
pub(crate) use workspace::FTS_CONFIG;
pub(crate) use workspace::FtsConfig;
pub(crate) use workspace::IndexDescriptor;
pub(crate) use workspace::IndexState;
pub use workspace::ScanRules;
pub(crate) use workspace::Workspace;
