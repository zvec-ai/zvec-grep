//! Prepared inputs and indexing options for extraction.

use crate::domain::{Content, EntityFragment, FileFormat, ImageContent, SourceFile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TextSource {
    pub file: SourceFile,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageSource {
    pub file: SourceFile,
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
    pub fragment: EntityFragment,
    /// Optional compacted content used only for embedding.
    pub embedding_source: Option<Vec<Content>>,
}
