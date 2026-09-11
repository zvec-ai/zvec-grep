//! Deterministic source extraction used by the indexing and lexical-enrichment paths.

mod code;
mod image;
mod markdown;
mod service;
mod spi;
mod text;

// Interface exposed to the rest of `zg-engine`.
pub(crate) use spi::{
    ChunkOptions, ImageSource, IndexingExtractionFragment, Source, SourceKind, TextSource,
};

use crate::{
    EngineError,
    domain::{Content, EntityFragment, SourceFile, TextRange},
};

pub(crate) fn source_kind(file: &SourceFile) -> Option<SourceKind> {
    service::source_kind(file)
}

pub(crate) fn extract<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<EntityFragment>, EngineError> {
    service::extract(source, options)
}

pub(crate) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    service::extract_for_indexing(source, options)
}

pub(crate) fn vector_content_for_fragment(
    fragment: &EntityFragment,
    embedding_content: Option<&[Content]>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    service::vector_content_for_fragment(fragment, embedding_content, max_chars)
}

// Shared implementation helpers used by the format-specific extractors.
use service::{
    byte_index_at_utf16, byte_index_at_utf16_ceil, char_count, chunk_options_for_metadata,
    fit_text_to_chars, make_entity_id, symbol_type_name, take_chars, validate_source_file,
};

#[cfg(test)]
use service::{test_content, test_file, test_source};
