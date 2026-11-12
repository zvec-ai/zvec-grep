//! Deterministic source extraction used by the indexing and lexical-enrichment paths.

mod chunking;
mod code;
mod graph;
mod image;
mod markdown;
mod service;
mod spi;
mod text;

pub(crate) use code::code_outline;

// Extraction sources.
pub(crate) use spi::{ImageSource, Source, SourceKind, TextSource};

// Chunking options.
pub(crate) use spi::ChunkOptions;

// Indexing output.
pub(crate) use spi::{ExtractedEntity, ExtractedEntityFragment, ExtractionOutput};

// Per-file graph support shared with the indexing pipeline.
#[allow(unused_imports)]
pub(crate) use graph::{FILE_SCOPE_INDEX, PartitionedGraph};
#[cfg(test)]
pub(crate) use graph::{PendingNameRef, ResolvedNameEdge};

use crate::{
    EngineError,
    domain::{Content, EntityMetadata, FileFormat, TextRange},
};

// Shared implementation helpers used by the format-specific extractors.
pub(crate) use service::fit_text_to_chars;
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

/// Extracts search content together with the walk-time code graph. Callers
/// that cannot persist a graph should use [`extract_without_graph`] instead.
pub(crate) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<ExtractionOutput, EngineError> {
    service::extract_for_indexing(source, options, true)
}

/// Extracts search content only. Direct search never stores a graph, so the
/// walk-time scans and the post-walk partition are skipped entirely.
pub(crate) fn extract_without_graph<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<ExtractionOutput, EngineError> {
    service::extract_for_indexing(source, options, false)
}

pub(crate) fn vector_content_for_fragment(
    content: &Content,
    metadata: Option<&EntityMetadata>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    service::vector_content_for_fragment(content, metadata, max_chars)
}
