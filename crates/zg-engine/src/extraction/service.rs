//! Extraction routing and shared implementation helpers.

#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use super::TextSource;
use super::{
    ChunkOptions, EntityFragment, IndexingExtractionFragment, Source, SourceFile, SourceKind, code,
    image, markdown, text,
};
use crate::{
    EngineError,
    domain::{
        Content, EntityContent, EntityId, EntityMetadata, FileCategory, FileFormat, SymbolType,
        TableCellRole,
    },
    utils::sha256_hex,
};

pub(super) fn extract<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<EntityFragment>, EngineError> {
    Ok(extract_for_indexing(source, options)?
        .into_iter()
        .map(|item| item.fragment)
        .collect())
}

pub(super) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    let fragments = match source.into() {
        Source::Image(source) => image::extract(source),
        Source::Text(source) if is_code_source(&source.file) => {
            return code::extract_for_indexing(source, options);
        }
        Source::Text(source) if source.file.formats.contains(&FileFormat::Markdown) => {
            markdown::extract(source, options)
        }
        Source::Text(source) => text::extract(source, options),
    }?;

    Ok(fragments
        .into_iter()
        .map(|fragment| IndexingExtractionFragment {
            fragment,
            embedding_source: None,
        })
        .collect())
}

pub(super) fn source_kind(file: &SourceFile) -> Option<SourceKind> {
    if let Some(format) = file
        .formats
        .iter()
        .find(|format| format.categories().contains(&FileCategory::Image))
    {
        return Some(SourceKind::Image(*format));
    }
    if file.has_category(FileCategory::Code)
        || file.has_category(FileCategory::Data)
        || file.formats.iter().any(|format| {
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

pub(super) fn is_code_source(file: &SourceFile) -> bool {
    !file.has_category(FileCategory::Data) && file.has_category(FileCategory::Code)
}

pub(super) fn vector_content_for_fragment(
    fragment: &EntityFragment,
    embedding_content: Option<&[Content]>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    let mut contents = if let Some(contents) = embedding_content {
        contents.to_vec()
    } else if let Some(EntityContent::Outline(outline)) =
        fragment.as_entity().map(|entity| &entity.content)
    {
        vec![Content::Text(outline.clone())]
    } else {
        fragment.contents().to_vec()
    };
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
    let metadata = vector_metadata_text(fragment.metadata(), metadata_budget(max_chars));
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

pub(super) fn validate_source_file(file: &SourceFile) -> Result<(), EngineError> {
    file.validate()
}

pub(super) fn make_entity_id(file_id: &crate::domain::FileId, index: usize) -> EntityId {
    let file_id = file_id.as_str();
    let id = sha256_hex(format!("{file_id}\0{index}").as_bytes());
    EntityId::new(id).expect("SHA-256 digest is a non-empty ID")
}

pub(super) fn chunk_options_for_metadata(
    max_chunk_chars: usize,
    chunk_overlap_chars: usize,
    metadata: Option<&EntityMetadata>,
) -> (usize, usize) {
    let metadata_text = vector_metadata_text(metadata, metadata_budget(Some(max_chunk_chars)));
    let separator_chars = usize::from(!metadata_text.is_empty());
    let content_max = max_chunk_chars
        .saturating_sub(char_count(&metadata_text) + separator_chars)
        .max(1);
    let overlap = chunk_overlap_chars.min(content_max.saturating_sub(1));
    (content_max, overlap)
}

pub(super) fn fit_text_to_chars(value: &str, max_chars: usize) -> String {
    if char_count(value) <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let prefix = take_chars(value, max_chars - 3).trim_end();
    format!("{prefix}...")
}

fn vector_metadata_text(metadata: Option<&EntityMetadata>, max_chars: Option<usize>) -> String {
    let Some(metadata) = metadata else {
        return String::new();
    };

    let lines = match metadata {
        EntityMetadata::Code {
            symbol_type,
            symbol_name,
            scope,
            signature,
            documentation,
            modifiers,
            ..
        } => vec![
            Some(match symbol_name {
                Some(name) => format!("symbol: {} {name}", symbol_type_name(*symbol_type)),
                None => format!("symbol: {}", symbol_type_name(*symbol_type)),
            }),
            scope.as_ref().map(|value| format!("scope: {value}")),
            signature
                .as_ref()
                .map(|value| format!("signature: {}", one_line(value))),
            (!modifiers.is_empty()).then(|| format!("modifiers: {}", modifiers.join(" "))),
            documentation
                .as_ref()
                .map(|value| format!("doc: {}", one_line(value))),
        ],
        EntityMetadata::Markdown {
            heading,
            level,
            scope,
        } => vec![
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

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn symbol_type_name(symbol_type: SymbolType) -> &'static str {
    match symbol_type {
        SymbolType::Module => "module",
        SymbolType::Class => "class",
        SymbolType::Interface => "interface",
        SymbolType::Function => "function",
        SymbolType::Value => "value",
        SymbolType::Alias => "alias",
    }
}

pub(super) fn char_count(value: &str) -> usize {
    value.encode_utf16().count()
}

pub(super) fn take_chars(value: &str, count: usize) -> &str {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > count {
            return &value[..index];
        }
        units = next;
    }
    value
}

pub(super) fn byte_index_at_utf16(value: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > utf16_offset {
            return index;
        }
        units = next;
    }
    value.len()
}

pub(super) fn byte_index_at_utf16_ceil(value: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        if units >= utf16_offset {
            return index;
        }
        units += character.len_utf16();
        if units > utf16_offset {
            return index + character.len_utf8();
        }
    }
    value.len()
}

#[cfg(test)]
pub(super) fn test_source(format: FileFormat, relative_path: &str, text: &str) -> TextSource {
    TextSource {
        file: test_file(format, relative_path, text.len() as u64),
        text: text.to_owned(),
    }
}

#[cfg(test)]
pub(super) fn test_file(format: FileFormat, relative_path: &str, size_bytes: u64) -> SourceFile {
    let root = std::env::current_dir().expect("current directory");
    SourceFile {
        id: crate::domain::FileId::new(format!("file-{}", format.as_str())).expect("file id"),
        absolute_path: root.join(relative_path),
        relative_path: PathBuf::from(relative_path),
        root_path: root,
        formats: vec![format],
        snapshot: crate::domain::FileSnapshot {
            size_bytes,
            modified_epoch_ms: Some(1),
            content_hash: None,
        },
    }
}

#[cfg(test)]
pub(super) fn test_content(fragment: &EntityFragment) -> Content {
    match fragment {
        EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
            match &entity.content {
                EntityContent::Source(contents) => {
                    assert_eq!(contents.len(), 1);
                    contents[0].clone()
                }
                EntityContent::Outline(text) => Content::Text(text.clone()),
            }
        }
        EntityFragment::Window(window) => {
            assert_eq!(window.contents.len(), 1);
            window.contents[0].clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Entity, SourceRange, TableCell, TableCellRole, TableContent};

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
            assert_eq!(source_kind(&test_file(format, "fixture", 1)), expected);
        }
        let mut source = test_source(
            FileFormat::Json,
            "tsconfig.json",
            "{\"compilerOptions\": {}}",
        );
        source.file.formats.push(FileFormat::TypeScript);
        assert!(!is_code_source(&source.file));
        let fragments = extract(&source, ChunkOptions::default()).expect("data extraction");
        assert_eq!(fragments.len(), 1);
        assert_eq!(test_content(&fragments[0]), Content::Text(source.text));
        assert!(fragments[0].metadata().is_none());
    }

    #[test]
    fn projects_table_cells_in_order_and_preserves_embedded_images() {
        let image = Content::Image(
            crate::domain::ImageContent::new(vec![1], FileFormat::Png).expect("image"),
        );
        let source = test_file(FileFormat::Markdown, "fixture.md", 1);
        let fragment = EntityFragment::Standalone(Entity {
            id: make_entity_id(&source.id, 0),
            file_id: source.id,
            range: SourceRange::File,
            content: EntityContent::Source(vec![
                Content::Text("before".to_owned()),
                Content::Table(TableContent {
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
                }),
                Content::Text("after".to_owned()),
            ]),
            metadata: None,
        });
        assert_eq!(
            vector_content_for_fragment(&fragment, None, None),
            vec![
                Content::Text("before".to_owned()),
                Content::Text("cell 0,0 (1x1):".to_owned()),
                Content::Text("cell".to_owned()),
                image,
                Content::Text("after".to_owned()),
            ]
        );
    }
}
