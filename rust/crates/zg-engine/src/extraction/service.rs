//! Extraction routing and shared implementation helpers.

#[cfg(test)]
use super::TextSource;
use super::{ChunkOptions, ExtractedEntity, Source, SourceKind, code, image, markdown, text};
use crate::{
    EngineError,
    domain::{
        CodeMetadata, Content, EntityMetadata, FileCategory, FileFormat, MarkdownMetadata, Range,
        TableCellRole,
    },
    utils::{collapse_whitespace, take_utf16, utf16_len},
};

pub(super) fn extract<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    let source = source.into();
    let source_text = match &source {
        Source::Text(source) => Some(source.text.as_str()),
        Source::Image(_) => None,
    };
    let entities = match source {
        Source::Text(source) if is_code_source(&source.formats) => {
            code::extract_for_indexing(source, options)
        }
        Source::Image(source) => Ok(image::extract(source)),
        Source::Text(source) if source.formats.contains(&FileFormat::Markdown) => {
            markdown::extract(source, options)
        }
        Source::Text(source) => text::extract(source, options),
    }?;
    if let Some(source_text) = source_text {
        for entity in &entities {
            if let Range::Text(range) = &entity.source_range {
                let original = crate::utils::slice_text(
                    source_text,
                    range.start_byte_offset(),
                    range.end_byte_offset(),
                )?;
                if !matches!(&entity.content, Content::Text(content) if content == original) {
                    return Err(EngineError::internal(
                        "entity content differs from its source range",
                    ));
                }
            }
        }
    }
    Ok(entities)
}

pub(super) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    extract(source, options)
}

pub(super) fn source_kind(formats: &[FileFormat]) -> Option<SourceKind> {
    if let Some(format) = formats
        .iter()
        .find(|format| format.categories().contains(&FileCategory::Image))
    {
        return Some(SourceKind::Image(*format));
    }
    if has_category(formats, FileCategory::Code)
        || has_category(formats, FileCategory::Data)
        || formats.iter().any(|format| {
            matches!(
                format,
                FileFormat::AsciiDoc
                    | FileFormat::Eml
                    | FileFormat::Markdown
                    | FileFormat::Mhtml
                    | FileFormat::Org
                    | FileFormat::Rst
                    | FileFormat::Rtf
                    | FileFormat::Srt
                    | FileFormat::Text
                    | FileFormat::WebVtt
            )
        })
    {
        Some(SourceKind::Text)
    } else {
        None
    }
}

pub(super) fn is_code_source(formats: &[FileFormat]) -> bool {
    !has_category(formats, FileCategory::Data) && has_category(formats, FileCategory::Code)
}

fn has_category(formats: &[FileFormat], category: FileCategory) -> bool {
    formats
        .iter()
        .any(|format| format.categories().contains(&category))
}

pub(super) fn vector_content_for_fragment(
    content: &Content,
    metadata: Option<&EntityMetadata>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    let mut contents = vec![content.clone()];
    if contents
        .iter()
        .any(|content| matches!(content, Content::Table(_)))
    {
        let mut projected = Vec::with_capacity(contents.len());
        for content in contents {
            project_content(content, &mut projected);
        }
        contents = projected;
    }
    let metadata = vector_metadata_text(metadata, metadata_budget(max_chars));
    if !metadata.is_empty() {
        if let Some(Content::Text(text)) = contents.first_mut() {
            *text = format!("{metadata}\n{text}");
        } else {
            contents.insert(0, Content::Text(metadata));
        }
    }
    contents
}

fn project_content(content: Content, output: &mut Vec<Content>) {
    match content {
        Content::Table(table) => {
            for cell in table.cells {
                let role = match cell.kind {
                    TableCellRole::Header => "header",
                    TableCellRole::Data | TableCellRole::Unknown => "cell",
                };
                output.push(Content::Text(format!(
                    "{role} {},{} ({}x{}):",
                    cell.row, cell.column, cell.row_span, cell.column_span,
                )));
                for content in cell.contents {
                    project_content(content, output);
                }
            }
        }
        content => output.push(content),
    }
}

pub(super) fn validate_formats(formats: &[FileFormat]) -> Result<(), EngineError> {
    if formats.is_empty()
        || (formats.len() > 1 && formats.contains(&FileFormat::Unknown))
        || formats
            .iter()
            .enumerate()
            .any(|(index, format)| formats[..index].contains(format))
    {
        return Err(EngineError::invalid_argument(
            "source formats must be non-empty and unique; unknown must stand alone",
        ));
    }
    Ok(())
}

pub(super) fn chunk_options_for_metadata(
    max_chunk_chars: usize,
    chunk_overlap_chars: usize,
    metadata: Option<&EntityMetadata>,
) -> (usize, usize) {
    let metadata_text = vector_metadata_text(metadata, metadata_budget(Some(max_chunk_chars)));
    let separator_chars = usize::from(!metadata_text.is_empty());
    let content_max = max_chunk_chars
        .saturating_sub(utf16_len(&metadata_text) + separator_chars)
        .max(1);
    let overlap = chunk_overlap_chars.min(content_max.saturating_sub(1));
    (content_max, overlap)
}

pub(super) fn fit_text_to_chars(value: &str, max_chars: usize) -> String {
    if utf16_len(value) <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let prefix = take_utf16(value, max_chars - 3).trim_end();
    format!("{prefix}...")
}

fn vector_metadata_text(metadata: Option<&EntityMetadata>, max_chars: Option<usize>) -> String {
    let Some(metadata) = metadata else {
        return String::new();
    };

    let lines = match metadata {
        EntityMetadata::Code(CodeMetadata {
            symbol_type,
            symbol_name,
            scope,
            signature,
            documentation,
        }) => vec![
            match (symbol_type, symbol_name) {
                (Some(kind), Some(name)) => Some(format!("symbol: {} {name}", kind.as_str())),
                (Some(kind), None) => Some(format!("symbol: {}", kind.as_str())),
                (None, Some(name)) => Some(format!("symbol: {name}")),
                (None, None) => None,
            },
            scope.as_ref().map(|value| format!("scope: {value}")),
            signature
                .as_ref()
                .map(|value| format!("signature: {}", collapse_whitespace(value))),
            documentation
                .as_ref()
                .map(|value| format!("doc: {}", collapse_whitespace(value))),
        ],
        EntityMetadata::Markdown(MarkdownMetadata {
            heading,
            level,
            scope,
        }) => vec![
            heading.as_ref().map(|value| format!("heading: {value}")),
            level.map(|value| format!("heading_level: {value}")),
            scope.as_ref().map(|value| format!("scope: {value}")),
        ],
    };

    let text = lines.into_iter().flatten().collect::<Vec<_>>().join("\n");
    max_chars.map_or(text.clone(), |limit| fit_text_to_chars(&text, limit))
}

fn metadata_budget(max_chars: Option<usize>) -> Option<usize> {
    max_chars.map(|value| value / 4)
}

#[cfg(test)]
pub(super) fn test_source(format: FileFormat, relative_path: &str, text: &str) -> TextSource {
    TextSource {
        relative_path: crate::domain::SourcePath::new(relative_path).expect("source path"),
        formats: vec![format],
        text: text.to_owned(),
    }
}

#[cfg(test)]
pub(super) fn test_metadata(entity: &ExtractedEntity) -> Option<&EntityMetadata> {
    entity.metadata.as_ref()
}

#[cfg(test)]
pub(super) fn test_content(entity: &ExtractedEntity) -> Content {
    entity.content.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{TableCell, TableCellRole, TableContent};

    #[test]
    fn routes_supported_sources_without_confusing_formats_and_reader_capabilities() {
        for (format, expected) in [
            (FileFormat::Rust, Some(SourceKind::Text)),
            (FileFormat::Markdown, Some(SourceKind::Text)),
            (FileFormat::Json, Some(SourceKind::Text)),
            (FileFormat::Png, Some(SourceKind::Image(FileFormat::Png))),
            (FileFormat::Svg, Some(SourceKind::Image(FileFormat::Svg))),
            (FileFormat::Pdf, None),
            (FileFormat::Word, None),
            (FileFormat::Unknown, None),
            (FileFormat::Binary, None),
        ] {
            assert_eq!(source_kind(&[format]), expected);
        }
        let mut source = test_source(
            FileFormat::Json,
            "tsconfig.json",
            "{\"compilerOptions\": {}}",
        );
        source.formats.push(FileFormat::TypeScript);
        assert!(!is_code_source(&source.formats));
        let fragments = extract(&source, ChunkOptions::default()).expect("data extraction");
        assert_eq!(fragments.len(), 1);
        assert_eq!(test_content(&fragments[0]), Content::Text(source.text));
        assert!(test_metadata(&fragments[0]).is_none());
    }

    #[test]
    fn projects_table_cells_in_order_and_preserves_embedded_images() {
        let image = Content::Image(
            crate::domain::ImageContent::new(vec![1], FileFormat::Png).expect("image"),
        );
        let content = Content::Table(TableContent {
            row_count: 1,
            column_count: 1,
            cells: vec![TableCell {
                row: 0,
                column: 0,
                row_span: 1,
                column_span: 1,
                contents: vec![Content::Text("cell".to_owned()), image.clone()],
                kind: TableCellRole::Data,
            }],
        });
        assert_eq!(
            vector_content_for_fragment(&content, None, None),
            vec![
                Content::Text("cell 0,0 (1x1):".to_owned()),
                Content::Text("cell".to_owned()),
                image,
            ]
        );
    }

    #[test]
    fn rejects_invalid_format_sets_without_requiring_file_records() {
        let mut source = test_source(FileFormat::Text, "fixture.txt", "source text");
        for formats in [
            vec![],
            vec![FileFormat::Rust, FileFormat::Rust],
            vec![FileFormat::Markdown, FileFormat::Unknown],
        ] {
            source.formats = formats;
            assert!(extract(&source, ChunkOptions::default()).is_err());
        }
    }
}
