//! Deterministic source extraction used by the indexing and lexical-enrichment paths.

mod chunking;
mod code;
mod image;
mod markdown;
mod service;
mod spi;
mod text;

// Extraction sources.
pub(crate) use spi::{ImageSource, Source, SourceKind, TextSource};

// Chunking options.
pub(crate) use spi::ChunkOptions;

// Indexing output.
pub(crate) use spi::{ExtractedEntity, ExtractedEntityFragment};

use crate::{
    EngineError,
    domain::{Content, EntityMetadata, FileFormat, TextRange},
};

// Shared implementation helpers used by the format-specific extractors.
use service::{chunk_options_for_metadata, validate_formats};

#[cfg(test)]
use service::{test_content, test_metadata, test_source};

pub(crate) fn source_kind(formats: &[FileFormat]) -> Option<SourceKind> {
    service::source_kind(formats)
}

pub(crate) fn extract<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    service::extract(source, options)
}

pub(crate) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    service::extract_for_indexing(source, options)
}

pub(crate) fn vector_content_for_fragment(
    content: &Content,
    metadata: Option<&EntityMetadata>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    service::vector_content_for_fragment(content, metadata, max_chars)
}
