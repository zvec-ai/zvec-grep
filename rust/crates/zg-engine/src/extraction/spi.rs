//! Prepared inputs and indexing options for extraction.

use crate::domain::{Content, EntityMetadata, FileFormat, ImageContent, Range, SourcePath};

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

/// One semantic entity with complete source content and its retrieval ranges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedEntity {
    pub index: usize,
    pub source_range: Range,
    pub content: Content,
    pub metadata: Option<EntityMetadata>,
    pub fragments: Vec<ExtractedEntityFragment>,
}

/// Full content or UTF-8 byte offsets in the owning entity's text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractedEntityFragment {
    pub range: Range,
}

#[cfg(test)]
impl ExtractedEntity {
    pub(crate) fn index(&self) -> usize {
        self.index
    }
    pub(crate) fn source_range(&self) -> &Range {
        &self.source_range
    }
}
