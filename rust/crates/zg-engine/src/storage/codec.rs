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
        ByteRange, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileFormat, FileId,
        FileIndexStatus, FileRecord, FileSnapshot, FragmentId, ImageContent, Range, SourcePath,
        TableCell, TableCellRole, TableContent, TextRange,
    },
};

const VERSION: u16 = 10;
// Nested tables add several JSON containers; keep records below serde's recursion limit.
const MAX_TABLE_DEPTH: usize = 16;

pub(crate) fn encode_file(file: &FileRecord) -> EngineResult<String> {
    file.validate()?;
    encode(FilePayload::from_file(file)?, "source file")
}

pub(crate) fn decode_file(json: &str) -> EngineResult<FileRecord> {
    let record: FilePayload<'static> = decode(json, "source file")?;
    record
        .into_file()
        .map_err(|error| invalid_record("source file", &error))
}

/// One canonical entity record contains its content once and all fragment selectors.
pub(super) fn encode_entity(entity: &Entity) -> EngineResult<String> {
    validate_entity(entity)?;
    encode(EntityRecord::from(entity), "entity")
}

pub(super) fn decode_entity(json: &str, metadata: Option<&EntityMetadata>) -> EngineResult<Entity> {
    let record: EntityRecord<'static> = decode(json, "entity")?;
    let entity = record
        .into_entity(metadata)
        .map_err(|error| invalid_record("entity", &error))?;
    validate_entity(&entity).map_err(|error| invalid_record("entity", &error))?;
    Ok(entity)
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
struct FilePayload<'a> {
    id: u32,
    relative_path: PathRecord,
    snapshot: SnapshotRecord<'a>,
    index_status: IndexStatusRecord<'a>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotRecord<'a> {
    size_bytes: u64,
    modified_epoch_ms: Option<u64>,
    content_hash: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum IndexStatusRecord<'a> {
    NotIndexed {},
    Indexed {
        indexed_epoch_ms: u64,
        entity_count: u64,
    },
    Failed {
        error: Cow<'a, str>,
    },
}

impl<'a> From<&'a FileIndexStatus> for IndexStatusRecord<'a> {
    fn from(status: &'a FileIndexStatus) -> Self {
        match status {
            FileIndexStatus::NotIndexed => Self::NotIndexed {},
            FileIndexStatus::Indexed {
                indexed_epoch_ms,
                entity_count,
            } => Self::Indexed {
                indexed_epoch_ms: *indexed_epoch_ms,
                entity_count: *entity_count,
            },
            FileIndexStatus::Failed { error } => Self::Failed {
                error: error.as_str().into(),
            },
        }
    }
}

impl From<IndexStatusRecord<'_>> for FileIndexStatus {
    fn from(status: IndexStatusRecord<'_>) -> Self {
        match status {
            IndexStatusRecord::NotIndexed {} => Self::NotIndexed,
            IndexStatusRecord::Indexed {
                indexed_epoch_ms,
                entity_count,
            } => Self::Indexed {
                indexed_epoch_ms,
                entity_count,
            },
            IndexStatusRecord::Failed { error } => Self::Failed {
                error: error.into_owned(),
            },
        }
    }
}

impl<'a> FilePayload<'a> {
    fn from_file(file: &'a FileRecord) -> EngineResult<Self> {
        Ok(Self {
            index_status: (&file.index_status).into(),
            id: file.id.get(),
            relative_path: PathRecord::from_path(&file.relative_path)?,
            snapshot: SnapshotRecord {
                size_bytes: file.snapshot.size_bytes,
                modified_epoch_ms: file.snapshot.modified_epoch_ms,
                content_hash: file.snapshot.content_hash.as_deref().map(Cow::Borrowed),
            },
        })
    }

    fn into_file(self) -> EngineResult<FileRecord> {
        let file = FileRecord {
            index_status: self.index_status.into(),
            id: FileId::new(self.id),
            relative_path: SourcePath::new(self.relative_path.into_path()?)?,
            snapshot: FileSnapshot {
                size_bytes: self.snapshot.size_bytes,
                modified_epoch_ms: self.snapshot.modified_epoch_ms,
                content_hash: self.snapshot.content_hash.map(Cow::into_owned),
            },
        };
        file.validate()?;
        Ok(file)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
pub(crate) enum PathRecord {
    Utf8(String),
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl PathRecord {
    pub(crate) fn from_path(path: &Path) -> EngineResult<Self> {
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

    pub(crate) fn into_path(self) -> EngineResult<PathBuf> {
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
#[serde(deny_unknown_fields)]
struct EntityRecord<'a> {
    id: Cow<'a, str>,
    file_id: u32,
    source_range: RangeRecord,
    content: ContentRecord<'a>,
    fragments: Vec<FragmentRecord<'a>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentRecord<'a> {
    id: Cow<'a, str>,
    range: RangeRecord,
}

impl<'a> From<&'a Entity> for EntityRecord<'a> {
    fn from(entity: &'a Entity) -> Self {
        Self {
            id: entity.id.as_str().into(),
            file_id: entity.file_id.get(),
            source_range: entity.source_range.into(),
            content: encode_content(&entity.content),
            fragments: entity
                .fragments
                .iter()
                .map(|fragment| FragmentRecord {
                    id: fragment.id.as_str().into(),
                    range: fragment.range.into(),
                })
                .collect(),
        }
    }
}

impl EntityRecord<'_> {
    fn into_entity(self, metadata: Option<&EntityMetadata>) -> EngineResult<Entity> {
        Ok(Entity {
            id: EntityId::new(self.id.into_owned())?,
            file_id: FileId::new(self.file_id),
            source_range: self.source_range.try_into()?,
            content: decode_content(self.content)?,
            metadata: metadata.cloned(),
            fragments: self
                .fragments
                .into_iter()
                .map(|fragment| {
                    Ok(EntityFragment {
                        id: FragmentId::new(fragment.id.into_owned())?,
                        range: fragment.range.try_into()?,
                    })
                })
                .collect::<EngineResult<_>>()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ContentRecord<'a> {
    Text(Cow<'a, str>),
    Image {
        format: FileFormat,
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
    contents.iter().map(encode_content).collect()
}

fn decode_contents(contents: Vec<ContentRecord<'_>>) -> EngineResult<Vec<Content>> {
    contents.into_iter().map(decode_content).collect()
}

fn encode_content(content: &Content) -> ContentRecord<'_> {
    match content {
        Content::Text(text) => ContentRecord::Text(text.as_str().into()),
        Content::Image(image) => ContentRecord::Image {
            format: image.format(),
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
    }
}
fn decode_content(content: ContentRecord<'_>) -> EngineResult<Content> {
    Ok(match content {
        ContentRecord::Text(text) => Content::Text(text.into_owned()),
        ContentRecord::Image { format, data } => {
            Content::Image(ImageContent::new(data.into_owned(), format)?)
        }
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
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RangeRecord {
    Full,
    Text {
        start_line: usize,
        end_line: usize,
        start_byte_offset: usize,
        end_byte_offset: usize,
        start_byte_column: usize,
        end_byte_column: usize,
    },
    Byte {
        start_offset: u64,
        end_offset: u64,
    },
}

impl From<Range> for RangeRecord {
    fn from(range: Range) -> Self {
        match range {
            Range::Full => Self::Full,
            Range::Text(range) => Self::Text {
                start_line: range.start_line(),
                end_line: range.end_line(),
                start_byte_offset: range.start_byte_offset(),
                end_byte_offset: range.end_byte_offset(),
                start_byte_column: range.start_byte_column(),
                end_byte_column: range.end_byte_column(),
            },
            Range::Byte(range) => Self::Byte {
                start_offset: range.start_offset,
                end_offset: range.end_offset,
            },
        }
    }
}

impl TryFrom<RangeRecord> for Range {
    type Error = EngineError;

    fn try_from(range: RangeRecord) -> EngineResult<Self> {
        Ok(match range {
            RangeRecord::Full => Self::Full,
            RangeRecord::Text {
                start_line,
                end_line,
                start_byte_offset,
                end_byte_offset,
                start_byte_column,
                end_byte_column,
            } => Self::Text(TextRange::from_coordinates(
                start_byte_offset,
                end_byte_offset,
                start_line,
                end_line,
                start_byte_column,
                end_byte_column,
            )?),
            RangeRecord::Byte {
                start_offset,
                end_offset,
            } => Self::Byte(ByteRange {
                start_offset,
                end_offset,
            }),
        })
    }
}

pub(super) fn validate_entity(entity: &Entity) -> EngineResult<()> {
    crate::domain::validate_entities(entity.file_id, std::slice::from_ref(entity))?;
    validate_contents(std::slice::from_ref(&entity.content), 0)
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
    use super::*;
    use crate::domain::{CodeMetadata, MarkdownMetadata, SymbolType};
    use serde_json::{Value, json};

    fn file() -> FileRecord {
        FileRecord {
            index_status: FileIndexStatus::NotIndexed,
            id: FileId::new(1),
            relative_path: SourcePath::new("nested/tsconfig.json").expect("source path"),
            snapshot: FileSnapshot {
                size_bytes: 123,
                modified_epoch_ms: Some(456),
                content_hash: Some("content-hash".into()),
            },
        }
    }

    fn text_range() -> Range {
        Range::Text(TextRange::from_coordinates(7, 18, 2, 2, 0, 11).expect("text range"))
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

    fn entity() -> Entity {
        let image =
            Content::Image(ImageContent::new(vec![0, 1, 255], FileFormat::Png).expect("image"));
        let mut nested = cell(1, 1, 1, 1, TableCellRole::Unknown);
        nested.contents.push(Content::Table(TableContent {
            row_count: 1,
            column_count: 1,
            cells: vec![cell(0, 0, 1, 1, TableCellRole::Header)],
        }));
        let mut mixed = cell(0, 1, 1, 2, TableCellRole::Data);
        mixed.contents.push(image);
        Entity {
            id: EntityId::new("entity").expect("entity id"),
            file_id: file().id,
            source_range: text_range(),
            content: Content::Table(TableContent {
                row_count: 2,
                column_count: 3,
                cells: vec![
                    cell(0, 0, 2, 1, TableCellRole::Header),
                    mixed,
                    nested,
                    cell(1, 2, 1, 1, TableCellRole::Data),
                ],
            }),
            metadata: None,
            fragments: vec![EntityFragment {
                id: FragmentId::new("fragment").expect("fragment id"),
                range: Range::Full,
            }],
        }
    }

    fn round_trip(entity: &Entity) {
        let encoded = encode_entity(entity).expect("encode entity");
        assert_eq!(
            decode_entity(&encoded, entity.metadata.as_ref()).expect("decode entity"),
            *entity
        );
    }

    fn assert_corrupt_entity(record: &Value) {
        let error = decode_entity(&record.to_string(), None).expect_err("invalid stored entity");
        assert_eq!(error.code(), EngineError::STORAGE_FAILURE, "{error}");
    }

    #[test]
    fn source_records_preserve_snapshots_and_native_paths_without_formats() {
        let mut source = file();
        let encoded = encode_file(&source).expect("encode source");
        let record: Value = serde_json::from_str(&encoded).expect("JSON record");
        assert_eq!(record["version"], VERSION);
        assert_eq!(
            record["value"],
            json!({
                "id": 1, "index_status": {"kind": "not_indexed"},
                "relative_path": {"encoding": "utf8", "value": "nested/tsconfig.json"},
                "snapshot": {"size_bytes": 123, "modified_epoch_ms": 456, "content_hash": "content-hash"},
            })
        );
        assert_eq!(decode_file(&encoded).expect("decode source"), source);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            source.relative_path =
                SourcePath::new(std::ffi::OsString::from_vec(b"file-\xff.rs".to_vec()))
                    .expect("source path");
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            source.relative_path = SourcePath::new(std::ffi::OsString::from_wide(&[
                0x0066, 0xd800, 0x002e, 0x0072, 0x0073,
            ]))
            .expect("source path");
        }
        assert_eq!(
            decode_file(&encode_file(&source).expect("encode native path"))
                .expect("decode native path"),
            source
        );
    }

    #[test]
    fn source_identities_round_trip_the_full_u32_range() {
        for id in [0, u32::MAX] {
            let mut source = file();
            source.id = FileId::new(id);
            assert_eq!(
                decode_file(&encode_file(&source).expect("encode source")).expect("decode source"),
                source
            );
            let mut entity = entity();
            entity.file_id = source.id;
            round_trip(&entity);
        }
    }

    #[test]
    fn stored_file_identities_reject_values_outside_u32() {
        let mut record: Value =
            serde_json::from_str(&encode_file(&file()).expect("source")).expect("JSON");
        let mut entity: Value =
            serde_json::from_str(&encode_entity(&entity()).expect("entity")).expect("JSON");
        for invalid in [json!(-1), json!(u64::from(u32::MAX) + 1), json!(1e20)] {
            record["value"]["id"] = invalid.clone();
            assert!(decode_file(&record.to_string()).is_err());
            entity["value"]["file_id"] = invalid;
            assert_corrupt_entity(&entity);
        }
    }

    #[test]
    fn file_status_round_trips_and_rejects_invalid_persisted_states() {
        let mut file = file();
        for status in [
            FileIndexStatus::NotIndexed,
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 789,
                entity_count: 0,
            },
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 789,
                entity_count: u64::MAX,
            },
            FileIndexStatus::Failed {
                error: "extraction failed".into(),
            },
        ] {
            file.index_status = status;
            assert_eq!(
                decode_file(&encode_file(&file).expect("encode status")).expect("decode status"),
                file
            );
        }
        let mut record: Value =
            serde_json::from_str(&encode_file(&file).expect("valid file")).expect("JSON");
        for invalid in [
            json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":0, "error":"failure"}),
            json!({"kind":"indexed", "indexed_epoch_ms":null, "entity_count":0}),
            json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":-1}),
            json!({"kind":"not_indexed", "entity_count":1}),
            json!({"kind":"running"}),
            Value::Null,
        ] {
            record["value"]["index_status"] = invalid;
            assert!(decode_file(&record.to_string()).is_err());
        }
        record["value"]["index_status"] =
            json!({"kind":"indexed", "indexed_epoch_ms":1, "entity_count":0});
        record["value"]["snapshot"]["content_hash"] = Value::Null;
        assert!(decode_file(&record.to_string()).is_err());
    }

    #[test]
    fn entity_records_preserve_structured_content_ranges_and_external_metadata() {
        round_trip(&entity());
        let record: Value =
            serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
        assert_eq!(
            record["value"]["content"]["value"]["cells"][1]["contents"][1]["value"]["data"],
            "AAH/"
        );
        assert_eq!(
            record["value"]["content"]["value"]["cells"][1]["contents"][1]["value"]["format"],
            "png"
        );
        assert!(
            record["value"].get("metadata").is_none(),
            "metadata is supplied by its dedicated storage column"
        );
        for (range, stored_range) in [
            (Range::Full, json!({"kind":"full"})),
            (
                text_range(),
                json!({"kind":"text", "start_line":2, "end_line":2, "start_byte_offset":7, "end_byte_offset":18, "start_byte_column":0, "end_byte_column":11}),
            ),
            (
                Range::Byte(ByteRange {
                    start_offset: 2,
                    end_offset: u64::MAX,
                }),
                json!({"kind":"byte", "start_offset":2, "end_offset":u64::MAX}),
            ),
        ] {
            let mut entity = entity();
            entity.source_range = range;
            entity.metadata = Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("symbol".into()),
                scope: Some("module".into()),
                signature: Some("pub async fn symbol()".into()),
                documentation: Some("documentation".into()),
            }));
            round_trip(&entity);
            let record: Value =
                serde_json::from_str(&encode_entity(&entity).expect("encode entity"))
                    .expect("JSON");
            assert_eq!(record["value"]["source_range"], stored_range);
        }
    }

    #[test]
    fn fragment_selectors_preserve_utf8_slices_without_repeating_content_or_ownership() {
        let mut entity = entity();
        entity.content = Content::Text("a中文b".into());
        entity.source_range =
            Range::Text(TextRange::from_coordinates(10, 18, 1, 1, 10, 18).expect("range"));
        entity.fragments.push(EntityFragment {
            id: FragmentId::new("slice").expect("fragment id"),
            range: Range::Byte(ByteRange {
                start_offset: 1,
                end_offset: 7,
            }),
        });
        entity.metadata = Some(EntityMetadata::Markdown(MarkdownMetadata {
            heading: Some("heading".into()),
            level: Some(2),
            scope: Some("parent".into()),
        }));
        round_trip(&entity);
        let encoded = encode_entity(&entity).expect("encode entity");
        let record: Value = serde_json::from_str(&encoded).expect("JSON");
        assert_eq!(
            record["value"]["fragments"][1],
            json!({
                "id":"slice", "range":{"kind":"byte", "start_offset":1, "end_offset":7},
            })
        );
        let restored = decode_entity(&encoded, entity.metadata.as_ref()).expect("decode entity");
        assert_eq!(
            restored.fragments[1]
                .range
                .extract(&restored.content)
                .expect("extract"),
            Content::Text("中文".into())
        );
        assert_eq!(
            restored
                .fragment_source_range(&restored.fragments[1])
                .expect("source location"),
            Range::Text(TextRange::from_coordinates(11, 17, 1, 1, 11, 17).expect("source range"))
        );
        for (start, end) in [(2, 7), (1, 9), (1, 1)] {
            let mut invalid = record.clone();
            invalid["value"]["fragments"][1]["range"] = json!({
                "kind":"byte", "start_offset":start, "end_offset":end,
            });
            assert_corrupt_entity(&invalid);
        }
        let mut invalid = record.clone();
        invalid["value"]["fragments"][1]["range"] = json!({
            "kind":"text", "start_line":1, "end_line":1, "start_byte_offset":1,
            "end_byte_offset":7, "start_byte_column":1, "end_byte_column":7,
        });
        assert_corrupt_entity(&invalid);
        let mut invalid = record.clone();
        invalid["value"]["fragments"][1]["range"]["start_line"] = json!(1);
        assert_corrupt_entity(&invalid);
        let mut invalid = record.clone();
        invalid["value"]["fragments"][1]["content_range"] = json!({"kind":"full"});
        assert_corrupt_entity(&invalid);
        let mut invalid = record.clone();
        invalid["value"]["range"] = invalid["value"]["source_range"].clone();
        assert_corrupt_entity(&invalid);
        let mut invalid = record.clone();
        invalid["value"]["fragments"][1]["id"] = record["value"]["fragments"][0]["id"].clone();
        assert_corrupt_entity(&invalid);
        let mut invalid = record;
        invalid["value"]["fragments"] = json!([]);
        assert_corrupt_entity(&invalid);
    }

    #[test]
    fn rejects_invalid_source_paths_during_decoding() {
        let original: Value =
            serde_json::from_str(&encode_file(&file()).expect("encode file")).expect("JSON");
        for path in [
            "../escape",
            "src//file.rs",
            "src/./file.rs",
            "",
            "bad\0path",
        ] {
            let mut record = original.clone();
            record["value"]["relative_path"] = json!({"encoding":"utf8", "value":path});
            assert_eq!(
                decode_file(&record.to_string())
                    .expect_err("invalid path")
                    .code(),
                EngineError::STORAGE_FAILURE
            );
        }
    }

    #[test]
    fn rejects_corrupt_versions_identities_images_and_ranges() {
        let file_record: Value =
            serde_json::from_str(&encode_file(&file()).expect("encode file")).expect("JSON");
        let original: Value =
            serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
        for (kind, record) in [("source file", file_record), ("entity", original.clone())] {
            for version in (1..VERSION).chain([VERSION + 1]) {
                let mut invalid = record.clone();
                invalid["version"] = json!(version);
                let json = invalid.to_string();
                let error = if kind == "source file" {
                    decode_file(&json).expect_err("wrong version")
                } else {
                    decode_entity(&json, None).expect_err("wrong version")
                };
                assert!(
                    error
                        .message()
                        .contains(&format!("unsupported stored {kind} version {version}"))
                );
                assert!(error.message().contains("rebuild the index"));
            }
        }
        for field in ["id", "file_id"] {
            let mut record = original.clone();
            record["value"][field] = json!("");
            assert_corrupt_entity(&record);
        }
        for (field, value) in [
            ("format", json!("rust")),
            ("format", json!("not-a-format")),
            ("format", json!(65535)),
            ("data", json!("")),
            ("data", json!("invalid base64!")),
        ] {
            let mut record = original.clone();
            record["value"]["content"]["value"]["cells"][1]["contents"][1]["value"][field] = value;
            assert_corrupt_entity(&record);
        }
        for (field, value) in [("start_line", 0), ("end_byte_column", 12)] {
            let mut record = original.clone();
            record["value"]["source_range"][field] = json!(value);
            assert_corrupt_entity(&record);
        }
        let mut record = original;
        record["value"]["fragments"][0]["id"] = json!("");
        assert_corrupt_entity(&record);
        assert!(decode_entity("not JSON", None).is_err());
    }

    #[test]
    fn rejects_invalid_table_geometry_without_allocating_a_dense_grid() {
        let original: Value =
            serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
        for (field, value) in [
            ("row_span", 0),
            ("row_span", 3),
            ("column_span", 2),
            ("column", usize::MAX),
        ] {
            let mut record = original.clone();
            record["value"]["content"]["value"]["cells"][0][field] = json!(value);
            assert_corrupt_entity(&record);
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
        let mut entity = entity();
        let mut content = Content::Text("nested".into());
        for depth in 1..=MAX_TABLE_DEPTH + 1 {
            let mut nested = cell(0, 0, 1, 1, TableCellRole::Data);
            nested.contents = vec![content];
            content = Content::Table(TableContent {
                row_count: 1,
                column_count: 1,
                cells: vec![nested],
            });
            entity.content = content.clone();
            if depth <= MAX_TABLE_DEPTH {
                round_trip(&entity);
            } else {
                assert!(encode_entity(&entity).is_err());
            }
        }
    }
}
