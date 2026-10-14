//! Canonical entity payloads: content is stored once, with fragment selectors.
use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::super::record::{decode, encode, invalid_record};
use crate::{
    EngineError, EngineResult,
    domain::{
        ByteRange, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileFormat, FileId,
        FragmentId, ImageContent, Range, TextRange,
    },
};

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
    Code(Cow<'a, str>),
    Image {
        format: FileFormat,
        #[serde(with = "image_bytes")]
        data: Cow<'a, [u8]>,
    },
}

mod image_bytes {
    use std::borrow::Cow;

    use serde::Deserializer;

    pub(super) use crate::utils::base64_bytes::serialize;

    pub(super) fn deserialize<'de, 'a, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Cow<'a, [u8]>, D::Error> {
        crate::utils::base64_bytes::deserialize(deserializer).map(Cow::Owned)
    }
}

fn encode_content(content: &Content) -> ContentRecord<'_> {
    match content {
        Content::Text(text) => ContentRecord::Text(text.as_str().into()),
        Content::Code(text) => ContentRecord::Code(text.as_str().into()),
        Content::Image(image) => ContentRecord::Image {
            format: image.format(),
            data: image.data().into(),
        },
    }
}
fn decode_content(content: ContentRecord<'_>) -> EngineResult<Content> {
    Ok(match content {
        ContentRecord::Text(text) => Content::Text(text.into_owned()),
        ContentRecord::Code(text) => Content::Code(text.into_owned()),
        ContentRecord::Image { format, data } => {
            Content::Image(ImageContent::new(data.into_owned(), format)?)
        }
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

#[cfg(test)]
mod tests;
