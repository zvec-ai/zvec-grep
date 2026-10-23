//! Shared domain types for source content, indexing, and search.

mod content;
mod entity;
mod glob;
mod graph;
mod index;
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

// Indexes.
pub(crate) use index::FTS_CONFIG;
pub(crate) use index::FtsConfig;
pub(crate) use index::IndexDescriptor;
pub(crate) use index::IndexState;

// Metadata.
pub use metadata::CodeMetadata;
pub use metadata::EntityMetadata;
pub(crate) use metadata::IndexField;
pub use metadata::Language;
pub use metadata::MarkdownMetadata;
pub use metadata::SymbolType;
// Visibility has no consumers yet.
#[allow(unused_imports)]
pub use metadata::Visibility;

// Shared graph contracts for extraction and persistence.
pub(crate) use graph::EdgeProvenance;
#[cfg(test)]
pub(crate) use graph::FileEdge;
#[cfg(test)]
pub(crate) use graph::FileGraphNode;
pub(crate) use graph::FileGraphResult;
#[cfg(test)]
pub(crate) use graph::GraphEdgeKind;
pub(crate) use graph::PendingRef;
pub(crate) use graph::PendingRefStatus;

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
pub use workspace::ScanRules;
pub(crate) use workspace::Workspace;
