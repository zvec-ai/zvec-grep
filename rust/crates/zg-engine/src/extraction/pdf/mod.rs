//! PDF conversion produces transient Markdown plus provenance. Entity and
//! fragment extraction stays shared with ordinary Markdown documents.

use std::sync::Arc;

use crate::{
    EngineError,
    domain::{Content, FileFormat, PdfSourceMap, Range},
};

use super::{ChunkOptions, ExtractedEntity, TextSource, markdown};

mod assembly;
mod render;
mod service;
pub(crate) use service::{
    PdfExtractionError, PdfExtractionOperation, PdfExtractionPort, PdfExtractionService,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PdfMarkdownSource {
    pub text: TextSource,
    pub source_map: PdfSourceMap,
}

pub(super) fn extract(
    source: &PdfMarkdownSource,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    if !source.text.formats.contains(&FileFormat::Pdf) {
        return Err(EngineError::invalid_argument(
            "PDF extraction requires a PDF source",
        ));
    }
    if source.text.text.trim().is_empty() && source.source_map.parts.is_empty() {
        return Ok(Vec::new());
    }
    source.source_map.validate(&source.text.text)?;
    let markdown_source = TextSource {
        formats: vec![FileFormat::Markdown],
        ..source.text.clone()
    };
    let mut entities = markdown::extract(&markdown_source, options)?;
    for entity in &mut entities {
        let Range::Text(range) = &entity.source_range else {
            return Err(EngineError::internal(
                "Markdown entity has no text coordinates",
            ));
        };
        let Content::Text(text) = &entity.content else {
            return Err(EngineError::internal(
                "Markdown entity has non-text content",
            ));
        };
        entity.source_range = Range::Pdf(Arc::new(
            source
                .source_map
                .slice(range.start_byte_offset()..range.end_byte_offset(), text)?,
        ));
    }
    Ok(entities)
}

#[cfg(test)]
mod tests;
