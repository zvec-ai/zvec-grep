//! Prepared inputs and indexing options for extraction.

use crate::domain::{
    Content, EntityContent, EntityMetadata, FileFormat, FileGraphResult, ImageContent, SourcePath,
    SourceRange,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TextSource {
    pub relative_path: SourcePath,
    pub formats: Vec<FileFormat>,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageSource {
    pub content: ImageContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceKind {
    Text,
    Image(FileFormat),
}

pub(crate) enum Source<'source> {
    Text(&'source TextSource),
    Image(&'source ImageSource),
}

impl<'source> From<&'source TextSource> for Source<'source> {
    fn from(source: &'source TextSource) -> Self {
        Self::Text(source)
    }
}

impl<'source> From<&'source ImageSource> for Source<'source> {
    fn from(source: &'source ImageSource) -> Self {
        Self::Image(source)
    }
}

/// Chunk limits count UTF-16 code units, independently of UTF-8 text storage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChunkOptions {
    pub max_chunk_chars: Option<usize>,
    pub chunk_overlap_chars: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexingExtractionFragment {
    pub fragment: ExtractedFragment,
    /// Optional compacted content used only for embedding.
    pub embedding_source: Option<Vec<Content>>,
}

/// Per-source extraction result: search fragments plus an optional code graph.
///
/// The graph is produced by the code path once the walk-time edge collection
/// and file-local partition are implemented; every other source kind returns
/// `None`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexingExtractionOutput {
    pub fragments: Vec<IndexingExtractionFragment>,
    pub graph: Option<FileGraphResult>,
}

/// Source fragments whose indices and ownership are local to one extraction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExtractedFragment {
    Standalone(ExtractedEntity),
    Representative(ExtractedEntity),
    Window(ExtractedWindow),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedEntity {
    pub index: usize,
    pub range: SourceRange,
    pub content: EntityContent,
    pub metadata: Option<EntityMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedWindow {
    pub index: usize,
    pub entity_index: usize,
    pub range: SourceRange,
    pub contents: Vec<Content>,
}

impl ExtractedFragment {
    pub(crate) fn index(&self) -> usize {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => entity.index,
            Self::Window(window) => window.index,
        }
    }

    pub(crate) fn entity_index(&self) -> usize {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => entity.index,
            Self::Window(window) => window.entity_index,
        }
    }

    pub(crate) fn range(&self) -> &SourceRange {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => &entity.range,
            Self::Window(window) => &window.range,
        }
    }

    pub(crate) fn contents(&self) -> &[Content] {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => match &entity.content {
                EntityContent::Source(contents) => contents,
                EntityContent::Outline(_) => &[],
            },
            Self::Window(window) => &window.contents,
        }
    }

    pub(crate) fn as_entity(&self) -> Option<&ExtractedEntity> {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => Some(entity),
            Self::Window(_) => None,
        }
    }
}
