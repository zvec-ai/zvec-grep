//! Canonical entity payloads: content is stored once, with fragment selectors.
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
};

use serde::{Deserialize, Serialize};

use super::super::record::{decode, encode, invalid_record};
use crate::{
    EngineError, EngineResult,
    domain::{
        ByteRange, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileFormat, FileId,
        FragmentId, ImageContent, Range, TableCell, TableCellRole, TableContent, TextRange,
    },
};

// Nested tables add several JSON containers; keep records below serde's recursion limit.
const MAX_TABLE_DEPTH: usize = 16;

/// One canonical entity record contains its content once and all fragment selectors.
/// The storage write entry point validates entities before encoding.
pub(super) fn encode_entity(entity: &Entity) -> EngineResult<String> {
    encode(EntityRecord::from(entity), "entity")
}

pub(super) fn decode_entity(json: &str, metadata: Option<&EntityMetadata>) -> EngineResult<Entity> {
    let record: EntityRecord<'static> = decode(json, "entity")?;
    let entity = record
        .into_entity(metadata)
        .map_err(|error| invalid_record("entity", &error))?;
    entity
        .validate()
        .and_then(|()| validate_content(&entity.content))
        .map_err(|error| invalid_record("entity", &error))?;
    Ok(entity)
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
            id: EntityId::from_string(self.id.into_owned()),
            file_id: FileId::new(self.file_id),
            source_range: self.source_range.try_into()?,
            content: decode_content(self.content)?,
            metadata: metadata.cloned(),
            fragments: self
                .fragments
                .into_iter()
                .map(|fragment| {
                    Ok(EntityFragment {
                        id: FragmentId::from_string(fragment.id.into_owned()),
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
                start_offset: range.start_offset(),
                end_offset: range.end_offset(),
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
            } => Self::Byte(ByteRange::new(start_offset, end_offset)?),
        })
    }
}

pub(in crate::storage) fn validate_content(content: &Content) -> EngineResult<()> {
    let Content::Table(table) = content else {
        return Ok(());
    };
    let mut tables = vec![(table, 1)];
    while let Some((table, depth)) = tables.pop() {
        if depth > MAX_TABLE_DEPTH {
            return Err(EngineError::invalid_argument(format!(
                "stored table nesting must not exceed {MAX_TABLE_DEPTH} levels"
            )));
        }
        validate_table(table)?;
        for cell in &table.cells {
            for content in &cell.contents {
                if let Content::Table(nested) = content {
                    tables.push((nested, depth + 1));
                }
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
mod tests;
