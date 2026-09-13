use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    EngineError, EngineResult,
    domain::{
        Content, Entity, EntityContent, EntityFragment, EntityId, EntityMetadata, FileFormat,
        FileId, FileSnapshot, FragmentId, ImageContent, SourceFile, SourceRange, SymbolType,
        TableCell, TableCellRole, TableContent, TextRange, WindowFragment,
    },
};

const VERSION: u16 = 2;
// Nested tables add several JSON containers; keep records below serde's recursion limit.
const MAX_TABLE_DEPTH: usize = 16;

pub(crate) fn encode_file(file: &SourceFile) -> EngineResult<String> {
    file.validate()?;
    encode(FileRecord::from_file(file)?, "source file")
}

pub(crate) fn decode_file(json: &str) -> EngineResult<SourceFile> {
    let record: FileRecord<'static> = decode(json, "source file")?;
    record
        .into_file()
        .map_err(|error| invalid_record("source file", &error))
}

pub(crate) fn encode_fragment(fragment: &EntityFragment) -> EngineResult<String> {
    validate_fragment(fragment)?;
    encode(FragmentRecord::from(fragment), "fragment")
}

pub(crate) fn decode_fragment(json: &str) -> EngineResult<EntityFragment> {
    let record: FragmentRecord<'static> = decode(json, "fragment")?;
    let fragment = record
        .into_fragment()
        .map_err(|error| invalid_record("fragment", &error))?;
    validate_fragment(&fragment).map_err(|error| invalid_record("fragment", &error))?;
    Ok(fragment)
}

fn encode(value: impl Serialize, kind: &str) -> EngineResult<String> {
    serde_json::to_string(&Record {
        version: VERSION,
        value,
    })
    .map_err(|error| EngineError::storage_failure(format!("failed to encode {kind}: {error}")))
}

fn decode<T: DeserializeOwned>(json: &str, kind: &str) -> EngineResult<T> {
    let decode_error =
        |error| EngineError::storage_failure(format!("failed to decode stored {kind}: {error}"));
    let record: Record<&serde_json::value::RawValue> =
        serde_json::from_str(json).map_err(decode_error)?;
    if record.version != VERSION {
        return Err(EngineError::storage_failure(format!(
            "unsupported stored {kind} version {}; expected {VERSION}; rebuild the index",
            record.version
        )));
    }
    serde_json::from_str(record.value.get()).map_err(decode_error)
}

fn invalid_record(kind: &str, error: &EngineError) -> EngineError {
    EngineError::storage_failure(format!("invalid stored {kind}: {}", error.message()))
}

#[derive(Serialize, Deserialize)]
struct Record<T> {
    version: u16,
    value: T,
}

#[derive(Serialize, Deserialize)]
struct FileRecord<'a> {
    id: Cow<'a, str>,
    absolute_path: PathRecord,
    relative_path: PathRecord,
    root_path: PathRecord,
    formats: Vec<u16>,
    snapshot: SnapshotRecord<'a>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotRecord<'a> {
    size_bytes: u64,
    modified_epoch_ms: Option<u64>,
    content_hash: Option<Cow<'a, str>>,
}

impl<'a> FileRecord<'a> {
    fn from_file(file: &'a SourceFile) -> EngineResult<Self> {
        Ok(Self {
            id: file.id.as_str().into(),
            absolute_path: PathRecord::from_path(&file.absolute_path)?,
            relative_path: PathRecord::from_path(&file.relative_path)?,
            root_path: PathRecord::from_path(&file.root_path)?,
            formats: file.formats.iter().map(|format| *format as u16).collect(),
            snapshot: SnapshotRecord {
                size_bytes: file.snapshot.size_bytes,
                modified_epoch_ms: file.snapshot.modified_epoch_ms,
                content_hash: file.snapshot.content_hash.as_deref().map(Cow::Borrowed),
            },
        })
    }

    fn into_file(self) -> EngineResult<SourceFile> {
        let file = SourceFile {
            id: FileId::new(self.id.into_owned())?,
            absolute_path: self.absolute_path.into_path()?,
            relative_path: self.relative_path.into_path()?,
            root_path: self.root_path.into_path()?,
            formats: self
                .formats
                .into_iter()
                .map(format_from_id)
                .collect::<EngineResult<_>>()?,
            snapshot: FileSnapshot {
                size_bytes: self.snapshot.size_bytes,
                modified_epoch_ms: self.snapshot.modified_epoch_ms,
                content_hash: self.snapshot.content_hash.map(Cow::into_owned),
            },
        };
        file.validate()?;
        for path in [&file.absolute_path, &file.relative_path, &file.root_path] {
            validate_path(path)?;
        }
        Ok(file)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
enum PathRecord {
    Utf8(String),
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl PathRecord {
    fn from_path(path: &Path) -> EngineResult<Self> {
        validate_path(path)?;
        if let Some(value) = path.to_str() {
            return Ok(Self::Utf8(value.to_owned()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self::UnixBytes(path.as_os_str().as_bytes().to_vec()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self::WindowsWide(path.as_os_str().encode_wide().collect()))
        }
        #[cfg(not(any(unix, windows)))]
        Err(EngineError::storage_failure(
            "cannot store a non-Unicode path on this platform",
        ))
    }

    fn into_path(self) -> EngineResult<PathBuf> {
        match self {
            Self::Utf8(value) => Ok(PathBuf::from(value)),
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(std::ffi::OsString::from_vec(bytes).into())
                }
                #[cfg(not(unix))]
                String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
                    EngineError::invalid_argument(
                        "stored Unix path cannot be represented on this platform",
                    )
                })
            }
            Self::WindowsWide(units) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    Ok(std::ffi::OsString::from_wide(&units).into())
                }
                #[cfg(not(windows))]
                String::from_utf16(&units).map(PathBuf::from).map_err(|_| {
                    EngineError::invalid_argument(
                        "stored Windows path cannot be represented on this platform",
                    )
                })
            }
        }
    }
}

fn validate_path(path: &Path) -> EngineResult<()> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(EngineError::invalid_argument(
            "source file path must not contain NUL",
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum FragmentRecord<'a> {
    Standalone(EntityRecord<'a>),
    Representative(EntityRecord<'a>),
    Window(WindowRecord<'a>),
}

#[derive(Serialize, Deserialize)]
struct EntityRecord<'a> {
    id: Cow<'a, str>,
    file_id: Cow<'a, str>,
    range: RangeRecord,
    content: EntityContentRecord<'a>,
    metadata: Option<MetadataRecord<'a>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum EntityContentRecord<'a> {
    Source(Vec<ContentRecord<'a>>),
    Outline(Cow<'a, str>),
}

#[derive(Serialize, Deserialize)]
struct WindowRecord<'a> {
    id: Cow<'a, str>,
    entity_id: Cow<'a, str>,
    file_id: Cow<'a, str>,
    range: RangeRecord,
    contents: Vec<ContentRecord<'a>>,
    metadata: Option<MetadataRecord<'a>>,
}

impl<'a> From<&'a EntityFragment> for FragmentRecord<'a> {
    fn from(fragment: &'a EntityFragment) -> Self {
        match fragment {
            EntityFragment::Standalone(entity) => Self::Standalone(entity.into()),
            EntityFragment::Representative(entity) => Self::Representative(entity.into()),
            EntityFragment::Window(window) => Self::Window(WindowRecord {
                id: window.id.as_str().into(),
                entity_id: window.entity_id.as_str().into(),
                file_id: window.file_id.as_str().into(),
                range: window.range.into(),
                contents: encode_contents(&window.contents),
                metadata: window.metadata.as_ref().map(Into::into),
            }),
        }
    }
}

impl FragmentRecord<'_> {
    fn into_fragment(self) -> EngineResult<EntityFragment> {
        Ok(match self {
            Self::Standalone(entity) => EntityFragment::Standalone(entity.into_entity()?),
            Self::Representative(entity) => EntityFragment::Representative(entity.into_entity()?),
            Self::Window(window) => EntityFragment::Window(WindowFragment {
                id: FragmentId::new(window.id.into_owned())?,
                entity_id: EntityId::new(window.entity_id.into_owned())?,
                file_id: FileId::new(window.file_id.into_owned())?,
                range: window.range.into(),
                contents: decode_contents(window.contents)?,
                metadata: window.metadata.map(Into::into),
            }),
        })
    }
}

impl<'a> From<&'a Entity> for EntityRecord<'a> {
    fn from(entity: &'a Entity) -> Self {
        Self {
            id: entity.id.as_str().into(),
            file_id: entity.file_id.as_str().into(),
            range: entity.range.into(),
            content: match &entity.content {
                EntityContent::Source(contents) => {
                    EntityContentRecord::Source(encode_contents(contents))
                }
                EntityContent::Outline(outline) => {
                    EntityContentRecord::Outline(outline.as_str().into())
                }
            },
            metadata: entity.metadata.as_ref().map(Into::into),
        }
    }
}

impl EntityRecord<'_> {
    fn into_entity(self) -> EngineResult<Entity> {
        Ok(Entity {
            id: EntityId::new(self.id.into_owned())?,
            file_id: FileId::new(self.file_id.into_owned())?,
            range: self.range.into(),
            content: match self.content {
                EntityContentRecord::Source(contents) => {
                    EntityContent::Source(decode_contents(contents)?)
                }
                EntityContentRecord::Outline(outline) => {
                    EntityContent::Outline(outline.into_owned())
                }
            },
            metadata: self.metadata.map(Into::into),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ContentRecord<'a> {
    Text(Cow<'a, str>),
    Image {
        format: u16,
        #[serde(with = "image_bytes")]
        data: Cow<'a, [u8]>,
    },
    Table {
        row_count: usize,
        column_count: usize,
        cells: Vec<CellRecord<'a>>,
    },
}

mod image_bytes {
    use std::{borrow::Cow, fmt};

    use base64::{Engine as _, display::Base64Display, engine::general_purpose::STANDARD};
    use serde::{
        Deserializer, Serializer,
        de::{Error, Visitor},
    };

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&Base64Display::new(bytes, &STANDARD))
    }

    pub(super) fn deserialize<'de, 'a, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Cow<'a, [u8]>, D::Error> {
        struct BytesVisitor;

        impl Visitor<'_> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a base64-encoded image")
            }

            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                STANDARD.decode(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(BytesVisitor).map(Cow::Owned)
    }
}

#[derive(Serialize, Deserialize)]
struct CellRecord<'a> {
    row: usize,
    column: usize,
    row_span: usize,
    column_span: usize,
    contents: Vec<ContentRecord<'a>>,
    role: CellRoleRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CellRoleRecord {
    Unknown,
    Data,
    Header,
}

fn encode_contents(contents: &[Content]) -> Vec<ContentRecord<'_>> {
    contents
        .iter()
        .map(|content| match content {
            Content::Text(text) => ContentRecord::Text(text.as_str().into()),
            Content::Image(image) => ContentRecord::Image {
                format: image.format() as u16,
                data: image.data().into(),
            },
            Content::Table(table) => ContentRecord::Table {
                row_count: table.row_count,
                column_count: table.column_count,
                cells: table
                    .cells
                    .iter()
                    .map(|cell| CellRecord {
                        row: cell.row,
                        column: cell.column,
                        row_span: cell.row_span,
                        column_span: cell.column_span,
                        contents: encode_contents(&cell.contents),
                        role: match cell.kind {
                            TableCellRole::Unknown => CellRoleRecord::Unknown,
                            TableCellRole::Data => CellRoleRecord::Data,
                            TableCellRole::Header => CellRoleRecord::Header,
                        },
                    })
                    .collect(),
            },
        })
        .collect()
}

fn decode_contents(contents: Vec<ContentRecord<'_>>) -> EngineResult<Vec<Content>> {
    contents
        .into_iter()
        .map(|content| {
            Ok(match content {
                ContentRecord::Text(text) => Content::Text(text.into_owned()),
                ContentRecord::Image { format, data } => Content::Image(ImageContent::new(
                    data.into_owned(),
                    format_from_id(format)?,
                )?),
                ContentRecord::Table {
                    row_count,
                    column_count,
                    cells,
                } => Content::Table(TableContent {
                    row_count,
                    column_count,
                    cells: cells
                        .into_iter()
                        .map(|cell| {
                            Ok(TableCell {
                                row: cell.row,
                                column: cell.column,
                                row_span: cell.row_span,
                                column_span: cell.column_span,
                                contents: decode_contents(cell.contents)?,
                                kind: match cell.role {
                                    CellRoleRecord::Unknown => TableCellRole::Unknown,
                                    CellRoleRecord::Data => TableCellRole::Data,
                                    CellRoleRecord::Header => TableCellRole::Header,
                                },
                            })
                        })
                        .collect::<EngineResult<_>>()?,
                }),
            })
        })
        .collect()
}

fn format_from_id(id: u16) -> EngineResult<FileFormat> {
    FileFormat::from_id(id)
        .ok_or_else(|| EngineError::invalid_argument(format!("unrecognized file format ID: {id}")))
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RangeRecord {
    File,
    Text {
        start_line: usize,
        end_line: usize,
        start_byte_offset: usize,
        end_byte_offset: usize,
    },
    Byte {
        start_offset: u64,
        end_offset: u64,
    },
    Page {
        page: usize,
    },
    PageText {
        page: usize,
        start_byte_offset: usize,
        end_byte_offset: usize,
    },
    PageRegion {
        page: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

impl From<SourceRange> for RangeRecord {
    fn from(range: SourceRange) -> Self {
        match range {
            SourceRange::File => Self::File,
            SourceRange::Text(range) => Self::Text {
                start_line: range.start_line,
                end_line: range.end_line,
                start_byte_offset: range.start_byte_offset,
                end_byte_offset: range.end_byte_offset,
            },
            SourceRange::Byte {
                start_offset,
                end_offset,
            } => Self::Byte {
                start_offset,
                end_offset,
            },
            SourceRange::Page { page } => Self::Page { page },
            SourceRange::PageText {
                page,
                start_byte_offset,
                end_byte_offset,
            } => Self::PageText {
                page,
                start_byte_offset,
                end_byte_offset,
            },
            SourceRange::PageRegion {
                page,
                x,
                y,
                width,
                height,
            } => Self::PageRegion {
                page,
                x,
                y,
                width,
                height,
            },
        }
    }
}

impl From<RangeRecord> for SourceRange {
    fn from(range: RangeRecord) -> Self {
        match range {
            RangeRecord::File => Self::File,
            RangeRecord::Text {
                start_line,
                end_line,
                start_byte_offset,
                end_byte_offset,
            } => Self::Text(TextRange {
                start_line,
                end_line,
                start_byte_offset,
                end_byte_offset,
            }),
            RangeRecord::Byte {
                start_offset,
                end_offset,
            } => Self::Byte {
                start_offset,
                end_offset,
            },
            RangeRecord::Page { page } => Self::Page { page },
            RangeRecord::PageText {
                page,
                start_byte_offset,
                end_byte_offset,
            } => Self::PageText {
                page,
                start_byte_offset,
                end_byte_offset,
            },
            RangeRecord::PageRegion {
                page,
                x,
                y,
                width,
                height,
            } => Self::PageRegion {
                page,
                x,
                y,
                width,
                height,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MetadataRecord<'a> {
    Code {
        symbol_type: SymbolRecord,
        symbol_name: Option<Cow<'a, str>>,
        scope: Option<Cow<'a, str>>,
        node_type: Option<Cow<'a, str>>,
        signature: Option<Cow<'a, str>>,
        documentation: Option<Cow<'a, str>>,
        modifiers: Vec<Cow<'a, str>>,
    },
    Markdown {
        heading: Option<Cow<'a, str>>,
        level: Option<usize>,
        scope: Option<Cow<'a, str>>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SymbolRecord {
    Module,
    Class,
    Interface,
    Function,
    Value,
    Alias,
}

impl<'a> From<&'a EntityMetadata> for MetadataRecord<'a> {
    fn from(metadata: &'a EntityMetadata) -> Self {
        match metadata {
            EntityMetadata::Code {
                symbol_type,
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            } => Self::Code {
                symbol_type: match symbol_type {
                    SymbolType::Module => SymbolRecord::Module,
                    SymbolType::Class => SymbolRecord::Class,
                    SymbolType::Interface => SymbolRecord::Interface,
                    SymbolType::Function => SymbolRecord::Function,
                    SymbolType::Value => SymbolRecord::Value,
                    SymbolType::Alias => SymbolRecord::Alias,
                },
                symbol_name: symbol_name.as_deref().map(Cow::Borrowed),
                scope: scope.as_deref().map(Cow::Borrowed),
                node_type: node_type.as_deref().map(Cow::Borrowed),
                signature: signature.as_deref().map(Cow::Borrowed),
                documentation: documentation.as_deref().map(Cow::Borrowed),
                modifiers: modifiers
                    .iter()
                    .map(|value| Cow::Borrowed(value.as_str()))
                    .collect(),
            },
            EntityMetadata::Markdown {
                heading,
                level,
                scope,
            } => Self::Markdown {
                heading: heading.as_deref().map(Cow::Borrowed),
                level: *level,
                scope: scope.as_deref().map(Cow::Borrowed),
            },
        }
    }
}

impl From<MetadataRecord<'_>> for EntityMetadata {
    fn from(metadata: MetadataRecord<'_>) -> Self {
        match metadata {
            MetadataRecord::Code {
                symbol_type,
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            } => Self::Code {
                symbol_type: match symbol_type {
                    SymbolRecord::Module => SymbolType::Module,
                    SymbolRecord::Class => SymbolType::Class,
                    SymbolRecord::Interface => SymbolType::Interface,
                    SymbolRecord::Function => SymbolType::Function,
                    SymbolRecord::Value => SymbolType::Value,
                    SymbolRecord::Alias => SymbolType::Alias,
                },
                symbol_name: symbol_name.map(Cow::into_owned),
                scope: scope.map(Cow::into_owned),
                node_type: node_type.map(Cow::into_owned),
                signature: signature.map(Cow::into_owned),
                documentation: documentation.map(Cow::into_owned),
                modifiers: modifiers.into_iter().map(Cow::into_owned).collect(),
            },
            MetadataRecord::Markdown {
                heading,
                level,
                scope,
            } => Self::Markdown {
                heading: heading.map(Cow::into_owned),
                level,
                scope: scope.map(Cow::into_owned),
            },
        }
    }
}

fn validate_fragment(fragment: &EntityFragment) -> EngineResult<()> {
    fragment.range().validate()?;
    if let Some(EntityContent::Outline(outline)) =
        fragment.as_entity().map(|entity| &entity.content)
    {
        if outline.trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "fragment outline must not be blank",
            ));
        }
    } else if fragment.contents().is_empty() {
        return Err(EngineError::invalid_argument(
            "fragment must contain source content",
        ));
    }
    validate_contents(fragment.contents(), 0)
}

fn validate_contents(contents: &[Content], table_depth: usize) -> EngineResult<()> {
    for content in contents {
        if let Content::Table(table) = content {
            if table_depth == MAX_TABLE_DEPTH {
                return Err(EngineError::invalid_argument(format!(
                    "stored table nesting must not exceed {MAX_TABLE_DEPTH} levels"
                )));
            }
            validate_table(table)?;
            for cell in &table.cells {
                validate_contents(&cell.contents, table_depth + 1)?;
            }
        }
    }
    Ok(())
}

fn validate_table(table: &TableContent) -> EngineResult<()> {
    let mut previous_position = None;
    // Track only cells covering the current row.
    let mut active_columns = BTreeMap::new();
    let mut row_endings = BinaryHeap::new();
    for cell in &table.cells {
        let position = (cell.row, cell.column);
        if previous_position.is_some_and(|previous| previous >= position) {
            return Err(EngineError::invalid_argument(
                "table cells must be ordered by row and column",
            ));
        }
        previous_position = Some(position);
        let row_end = cell
            .row
            .checked_add(cell.row_span)
            .filter(|end| cell.row_span > 0 && *end <= table.row_count);
        let column_end = cell
            .column
            .checked_add(cell.column_span)
            .filter(|end| cell.column_span > 0 && *end <= table.column_count);
        let (Some(row_end), Some(column_end)) = (row_end, column_end) else {
            return Err(EngineError::invalid_argument(
                "table cell span is empty, out of bounds, or overflowing",
            ));
        };
        while let Some(Reverse((end_row, column))) = row_endings.peek().copied() {
            if end_row > cell.row {
                break;
            }
            row_endings.pop();
            active_columns.remove(&column);
        }
        if active_columns
            .range(..=cell.column)
            .next_back()
            .is_some_and(|(_, end)| *end > cell.column)
            || active_columns
                .range(cell.column..)
                .next()
                .is_some_and(|(start, _)| *start < column_end)
        {
            return Err(EngineError::invalid_argument("table cells overlap"));
        }
        active_columns.insert(cell.column, column_end);
        row_endings.push(Reverse((row_end, cell.column)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn file() -> SourceFile {
        let root = std::env::current_dir().expect("current directory");
        SourceFile {
            id: FileId::new("source").expect("file ID"),
            absolute_path: root.join("nested/tsconfig.json"),
            relative_path: PathBuf::from("nested/tsconfig.json"),
            root_path: root,
            formats: vec![FileFormat::Json, FileFormat::TypeScript],
            snapshot: FileSnapshot {
                size_bytes: 123,
                modified_epoch_ms: Some(456),
                content_hash: Some("content-hash".to_owned()),
            },
        }
    }

    fn text_range() -> SourceRange {
        SourceRange::Text(TextRange {
            start_line: 2,
            end_line: 2,
            start_byte_offset: "前言\n".len(),
            end_byte_offset: "前言\n正文 😀".len(),
        })
    }

    fn cell(
        row: usize,
        column: usize,
        row_span: usize,
        column_span: usize,
        kind: TableCellRole,
    ) -> TableCell {
        TableCell {
            row,
            column,
            row_span,
            column_span,
            contents: vec![Content::Text(format!("{row},{column}"))],
            kind,
        }
    }

    fn fragment() -> EntityFragment {
        let image =
            Content::Image(ImageContent::new(vec![0, 1, 255], FileFormat::Png).expect("image"));
        let mut nested = cell(1, 1, 1, 1, TableCellRole::Unknown);
        nested.contents.push(Content::Table(TableContent {
            row_count: 1,
            column_count: 1,
            cells: vec![cell(0, 0, 1, 1, TableCellRole::Header)],
        }));
        let mut mixed = cell(0, 1, 1, 2, TableCellRole::Data);
        mixed.contents.push(image.clone());
        EntityFragment::Standalone(Entity {
            id: EntityId::new("entity").expect("entity ID"),
            file_id: file().id,
            range: text_range(),
            content: EntityContent::Source(vec![
                Content::Text("正文 😀".to_owned()),
                image,
                Content::Table(TableContent {
                    row_count: 2,
                    column_count: 3,
                    cells: vec![
                        cell(0, 0, 2, 1, TableCellRole::Header),
                        mixed,
                        nested,
                        cell(1, 2, 1, 1, TableCellRole::Data),
                    ],
                }),
            ]),
            metadata: None,
        })
    }

    fn round_trip(fragment: &EntityFragment) {
        let encoded = encode_fragment(fragment).expect("encode fragment");
        assert_eq!(
            decode_fragment(&encoded).expect("decode fragment"),
            *fragment
        );
    }

    fn assert_corrupt_fragment(record: &Value) {
        let error = decode_fragment(&record.to_string()).expect_err("invalid stored fragment");
        assert_eq!(error.code(), EngineError::STORAGE_FAILURE, "{error}");
    }

    #[test]
    fn source_records_preserve_format_ids_snapshots_and_native_paths() {
        let mut source = file();
        let encoded = encode_file(&source).expect("encode source");
        let record: Value = serde_json::from_str(&encoded).expect("JSON record");
        assert_eq!(
            record["value"]["formats"],
            json!([FileFormat::Json as u16, FileFormat::TypeScript as u16])
        );
        assert_eq!(decode_file(&encoded).expect("decode source"), source);

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            source.relative_path = std::ffi::OsString::from_vec(b"file-\xff.rs".to_vec()).into();
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            source.relative_path =
                std::ffi::OsString::from_wide(&[0x0066, 0xd800, 0x002e, 0x0072, 0x0073]).into();
        }
        source.absolute_path = source.root_path.join(&source.relative_path);
        let encoded = encode_file(&source).expect("encode native path");
        assert_eq!(decode_file(&encoded).expect("decode native path"), source);
    }

    #[test]
    fn fragment_records_preserve_compound_content_ranges_metadata_and_ownership() {
        round_trip(&fragment());
        let restored = decode_fragment(&encode_fragment(&fragment()).expect("encode fragment"))
            .expect("decode fragment");
        let SourceRange::Text(range) = restored.range() else {
            panic!("text range");
        };
        assert_eq!(
            "前言\n正文 😀".get(range.start_byte_offset..range.end_byte_offset),
            Some("正文 😀")
        );
        let encoded: Value =
            serde_json::from_str(&encode_fragment(&fragment()).expect("encode fragment"))
                .expect("fragment JSON");
        assert_eq!(
            encoded["value"]["value"]["content"]["value"][1]["value"]["data"],
            "AAH/"
        );
        let ranges = [
            SourceRange::File,
            text_range(),
            SourceRange::Byte {
                start_offset: 2,
                end_offset: u64::MAX,
            },
            SourceRange::Page { page: 3 },
            SourceRange::PageText {
                page: 3,
                start_byte_offset: 3,
                end_byte_offset: 7,
            },
            SourceRange::PageRegion {
                page: 3,
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
        ];
        let symbols = [
            SymbolType::Module,
            SymbolType::Class,
            SymbolType::Interface,
            SymbolType::Function,
            SymbolType::Value,
            SymbolType::Alias,
        ];
        for (range, symbol_type) in ranges.into_iter().zip(symbols) {
            let EntityFragment::Standalone(mut entity) = fragment() else {
                unreachable!()
            };
            entity.range = range;
            entity.metadata = Some(EntityMetadata::Code {
                symbol_type,
                symbol_name: Some("symbol".to_owned()),
                scope: Some("module".to_owned()),
                node_type: Some("node".to_owned()),
                signature: Some("fn symbol()".to_owned()),
                documentation: Some("documentation".to_owned()),
                modifiers: vec!["public".to_owned(), "async".to_owned()],
            });
            round_trip(&EntityFragment::Standalone(entity));
        }
        let representative = EntityFragment::Representative(Entity {
            id: EntityId::new("group").expect("entity ID"),
            file_id: file().id,
            range: SourceRange::File,
            content: EntityContent::Outline("section outline".to_owned()),
            metadata: Some(EntityMetadata::Markdown {
                heading: Some("heading".to_owned()),
                level: Some(2),
                scope: Some("parent".to_owned()),
            }),
        });
        let window = EntityFragment::Window(WindowFragment {
            id: FragmentId::new("window").expect("window ID"),
            entity_id: representative.entity_id().clone(),
            file_id: file().id,
            range: text_range(),
            contents: vec![Content::Text("window text".to_owned())],
            metadata: representative.metadata().cloned(),
        });
        for fragment in [&representative, &window] {
            round_trip(fragment);
        }
        crate::domain::validate_fragments(&file().id, [&representative, &window])
            .expect("valid group");
    }

    #[test]
    fn rejects_corrupt_versions_ids_formats_images_and_ranges() {
        let file_record: Value =
            serde_json::from_str(&encode_file(&file()).expect("encode file")).expect("file JSON");
        for (field, value) in [
            ("id", json!(" ")),
            ("formats", json!([65535])),
            ("formats", json!([])),
            (
                "formats",
                json!([FileFormat::Rust as u16, FileFormat::Rust as u16]),
            ),
            (
                "relative_path",
                json!({"encoding":"utf8", "value":"../escape"}),
            ),
            (
                "relative_path",
                json!({"encoding":"utf8", "value":"bad\u{0}path"}),
            ),
        ] {
            let mut record = file_record.clone();
            record["value"][field] = value;
            assert_eq!(
                decode_file(&record.to_string())
                    .expect_err("invalid source")
                    .code(),
                EngineError::STORAGE_FAILURE
            );
        }
        let original: Value =
            serde_json::from_str(&encode_fragment(&fragment()).expect("encode fragment"))
                .expect("fragment JSON");
        for (kind, mut record) in [("source file", file_record), ("fragment", original.clone())] {
            record["version"] = json!(1);
            if kind == "fragment" {
                record["value"]["value"]["range"] = json!({
                    "kind": "text", "start_line": 2, "end_line": 2,
                    "start_utf16_offset": 3, "end_utf16_offset": 8,
                });
            }
            let json = record.to_string();
            let error = if kind == "source file" {
                decode_file(&json).expect_err("legacy source record")
            } else {
                decode_fragment(&json).expect_err("legacy UTF-16 range")
            };
            assert!(
                error
                    .message()
                    .contains(&format!("unsupported stored {kind} version 1"))
            );
            assert!(error.message().contains("rebuild the index"));
        }
        let mut record = original.clone();
        record["version"] = json!(VERSION + 1);
        assert_corrupt_fragment(&record);
        for field in ["id", "file_id"] {
            let mut record = original.clone();
            record["value"]["value"][field] = json!("");
            assert_corrupt_fragment(&record);
        }
        for (field, value) in [
            ("format", json!(FileFormat::Rust as u16)),
            ("format", json!(65535)),
            ("data", json!("")),
            ("data", json!("invalid base64!")),
        ] {
            let mut record = original.clone();
            record["value"]["value"]["content"]["value"][1]["value"][field] = value;
            assert_corrupt_fragment(&record);
        }
        let mut record = original.clone();
        record["value"]["value"]["range"]["start_line"] = json!(0);
        assert_corrupt_fragment(&record);
        let mut record = original.clone();
        record["value"]["value"]["content"]["value"] = json!([]);
        assert_corrupt_fragment(&record);
        let mut record = original;
        record["value"]["value"]["content"] = json!({"kind":"outline", "value":" "});
        assert_corrupt_fragment(&record);
        assert!(decode_fragment("not JSON").is_err());
    }

    #[test]
    fn rejects_invalid_table_geometry_without_allocating_a_dense_grid() {
        let original: Value =
            serde_json::from_str(&encode_fragment(&fragment()).expect("encode fragment"))
                .expect("fragment JSON");
        for (field, value) in [
            ("row_span", 0),
            ("row_span", 3),
            ("column_span", 2),
            ("column", usize::MAX),
        ] {
            let mut record = original.clone();
            record["value"]["value"]["content"]["value"][2]["value"]["cells"][0][field] =
                json!(value);
            assert_corrupt_fragment(&record);
        }
        let mut table = TableContent {
            row_count: usize::MAX,
            column_count: usize::MAX,
            cells: vec![
                cell(0, 0, 1, 1, TableCellRole::Unknown),
                cell(usize::MAX - 1, usize::MAX - 1, 1, 1, TableCellRole::Data),
            ],
        };
        validate_table(&table).expect("sparse table");
        table.cells.reverse();
        assert!(validate_table(&table).is_err());
        table.cells = vec![
            cell(0, 0, 2, 2, TableCellRole::Data),
            cell(1, 1, 1, 1, TableCellRole::Data),
        ];
        assert!(validate_table(&table).is_err());

        let EntityFragment::Standalone(mut entity) = fragment() else {
            unreachable!()
        };
        let mut content = Content::Text("nested".to_owned());
        for depth in 1..=MAX_TABLE_DEPTH + 1 {
            let mut nested = cell(0, 0, 1, 1, TableCellRole::Data);
            nested.contents = vec![content];
            content = Content::Table(TableContent {
                row_count: 1,
                column_count: 1,
                cells: vec![nested],
            });
            entity.content = EntityContent::Source(vec![content.clone()]);
            let fragment = EntityFragment::Standalone(entity.clone());
            if depth <= MAX_TABLE_DEPTH {
                round_trip(&fragment);
            } else {
                assert!(encode_fragment(&fragment).is_err());
            }
        }
    }
}
