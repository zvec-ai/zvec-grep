use std::collections::HashSet;

use crate::{EngineError, EngineResult};

use super::{Content, EntityMetadata, FileId, Range, TextRange};

/// A logical result unit owning its complete original content and search fragments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entity {
    pub id: EntityId,
    pub file_id: FileId,
    /// Coordinates in the original file (decoded UTF-8 for text).
    pub source_range: Range,
    pub content: Content,
    pub metadata: Option<EntityMetadata>,
    pub fragments: Vec<EntityFragment>,
}

/// A searchable range of its owning entity's original content.
/// Each fragment has its own identity. File ownership and metadata come from
/// the entity; disk search projections repeat the fields needed for filtering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityFragment {
    pub id: FragmentId,
    /// Full content, or UTF-8 byte offsets relative to the owning entity's text.
    pub range: Range,
}

impl Entity {
    /// Restores source coordinates for a fragment of directly stored source text.
    pub(crate) fn fragment_source_range(&self, fragment: &EntityFragment) -> EngineResult<Range> {
        fragment.range.validate_content(&self.content)?;
        match (fragment.range, &self.content) {
            (Range::Full, _) => Ok(self.source_range),
            (Range::Byte(range), Content::Text(text)) => {
                let lines = crate::utils::line_byte_offsets(&text.split('\n').collect::<Vec<_>>());
                self.text_fragment_source_range(range.text_range(text, &lines)?)
            }
            _ => unreachable!("validated content selector"),
        }
    }

    fn text_fragment_source_range(&self, local: TextRange) -> EngineResult<Range> {
        match self.source_range {
            Range::Text(origin) => local.within(origin).map(Range::Text),
            Range::Full => Ok(Range::Text(local)),
            Range::Byte(_) => Err(EngineError::invalid_argument(
                "fragment content coordinates cannot be mapped to this entity source",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FragmentId(String);

impl EntityId {
    #[track_caller]
    pub(crate) fn new(value: impl Into<String>) -> EngineResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EngineError::invalid_argument("entity id must not be blank"));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FragmentId {
    #[track_caller]
    pub(crate) fn new(value: impl Into<String>) -> EngineResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "fragment id must not be blank",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checks canonical identities and fragment ranges for one complete source file.
pub(crate) fn validate_entities(file_id: FileId, entities: &[Entity]) -> EngineResult<()> {
    let mut entity_ids = HashSet::new();
    let mut fragment_ids = HashSet::new();
    for entity in entities {
        if entity.file_id != file_id {
            return Err(EngineError::invalid_argument(
                "entity belongs to a different source file",
            ));
        }
        if !entity_ids.insert(&entity.id) {
            return Err(EngineError::invalid_argument("duplicate entity id"));
        }
        entity.source_range.validate()?;
        if !content_has_value(&entity.content) || entity.fragments.is_empty() {
            return Err(EngineError::invalid_argument(
                "entity requires content and at least one fragment",
            ));
        }
        // Build line starts once per entity so validating every fragment stays linear
        // in the content size, rather than rescanning each fragment's prefix.
        let text_coordinates = match &entity.content {
            Content::Text(text) => Some((
                text.as_str(),
                crate::utils::line_byte_offsets(&text.split('\n').collect::<Vec<_>>()),
            )),
            _ => None,
        };
        for fragment in &entity.fragments {
            if !fragment_ids.insert(&fragment.id) {
                return Err(EngineError::invalid_argument("duplicate fragment id"));
            }
            fragment.range.validate_content(&entity.content)?;
            if let (Range::Byte(range), Some((text, lines))) = (fragment.range, &text_coordinates) {
                entity.text_fragment_source_range(range.text_range(text, lines)?)?;
            }
        }
    }
    Ok(())
}

fn content_has_value(content: &Content) -> bool {
    match content {
        Content::Text(text) => !text.trim().is_empty(),
        Content::Image(image) => !image.data().is_empty(),
        Content::Table(table) => table
            .cells
            .iter()
            .any(|cell| cell.contents.iter().any(content_has_value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ByteRange;

    fn range(text: &str, start: usize, end: usize) -> Range {
        let lines = crate::utils::line_byte_offsets(&text.split('\n').collect::<Vec<_>>());
        Range::Text(TextRange::from_offsets(text, &lines, start, end).expect("range"))
    }

    fn byte_range(start_offset: u64, end_offset: u64) -> Range {
        Range::Byte(ByteRange {
            start_offset,
            end_offset,
        })
    }

    fn entity() -> Entity {
        Entity {
            id: EntityId::new("entity").expect("id"),
            file_id: FileId::new(1),
            source_range: range("0123456789a中文b", 10, 18),
            content: Content::Text("a中文b".into()),
            metadata: None,
            fragments: vec![EntityFragment {
                id: FragmentId::new("fragment").expect("id"),
                range: Range::Full,
            }],
        }
    }

    #[test]
    fn validates_relative_utf8_ranges_and_overlapping_fragments() {
        let mut entity = entity();
        let slice = EntityFragment {
            id: FragmentId::new("part").expect("id"),
            range: byte_range(1, 7),
        };
        assert_eq!(
            slice.range.extract(&entity.content).expect("slice"),
            Content::Text("中文".into())
        );
        assert_eq!(
            entity
                .fragment_source_range(&slice)
                .expect("source location"),
            range("0123456789a中文b", 11, 17)
        );
        entity.fragments.insert(0, slice);
        validate_entities(entity.file_id, &[entity]).expect("full and partial ranges may overlap");
    }

    #[test]
    fn rejects_invalid_ranges_and_wrong_content_types() {
        for (start, end) in [(2, 7), (1, 9), (7, 1), (1, 1), (0, u64::MAX)] {
            assert!(byte_range(start, end).extract(&entity().content).is_err());
        }
        let image = Content::Image(
            crate::domain::ImageContent::new(vec![1], crate::domain::FileFormat::Png)
                .expect("image"),
        );
        assert!(byte_range(0, 1).extract(&image).is_err());
        assert_eq!(Range::Full.extract(&image).expect("whole image"), image);
        let mut entity = entity();
        entity.fragments[0].range = range("a中文b", 1, 7);
        assert!(validate_entities(entity.file_id, &[entity]).is_err());
        let mut entity = self::entity();
        entity.source_range = byte_range(10, 18);
        validate_entities(entity.file_id, std::slice::from_ref(&entity))
            .expect("whole content can have a binary source location");
        entity.fragments[0].range = byte_range(1, 7);
        assert!(validate_entities(entity.file_id, &[entity]).is_err());
    }

    #[test]
    fn rejects_duplicate_entities_and_cross_entity_fragment_ids() {
        let entity = entity();
        assert!(validate_entities(entity.file_id, &[entity.clone(), entity.clone()]).is_err());
        let mut other = entity.clone();
        other.id = EntityId::new("other").expect("id");
        assert!(validate_entities(entity.file_id, &[entity, other]).is_err());
    }

    #[test]
    fn rejects_wrong_files_empty_entities_and_outside_fragments() {
        let entity = entity();
        assert!(validate_entities(FileId::new(2), std::slice::from_ref(&entity)).is_err());
        let mut invalid = entity.clone();
        invalid.content = Content::Text(" ".into());
        assert!(validate_entities(entity.file_id, &[invalid]).is_err());
        let mut invalid = entity.clone();
        invalid.fragments.clear();
        assert!(validate_entities(entity.file_id, &[invalid]).is_err());
        let mut invalid = entity.clone();
        invalid.fragments[0].range = byte_range(10, 19);
        assert!(validate_entities(entity.file_id, &[invalid]).is_err());
        let mut invalid = entity.clone();
        invalid.source_range = range("0123456789ab", 10, 12);
        invalid.fragments[0].range = byte_range(1, 7);
        assert!(validate_entities(entity.file_id, &[invalid]).is_err());
    }

    #[test]
    fn maps_multiline_unicode_fragments_from_nonzero_source_origins() {
        let content = "a中文b\r\n尾😀\nend";
        let source = format!("intro\n  {content} trailing");
        let mut entity = entity();
        entity.source_range = range(&source, 8, 8 + content.len());
        entity.content = Content::Text(content.into());
        let fragment = EntityFragment {
            id: FragmentId::new("part").expect("id"),
            range: byte_range(1, 17),
        };
        entity.fragments.push(fragment.clone());
        validate_entities(entity.file_id, std::slice::from_ref(&entity))
            .expect("local UTF-8 byte offsets");
        assert_eq!(
            fragment.range.extract(&entity.content).expect("text"),
            Content::Text("中文b\r\n尾😀".into())
        );
        assert_eq!(
            entity.fragment_source_range(&fragment).expect("source"),
            range(&source, 9, 25)
        );
        assert_eq!(
            entity
                .fragment_source_range(&entity.fragments[0])
                .expect("full source"),
            entity.source_range
        );

        let overflowing = TextRange::from_coordinates(
            usize::MAX - 2,
            usize::MAX,
            1,
            1,
            usize::MAX - 2,
            usize::MAX,
        )
        .expect("origin");
        entity.source_range = Range::Text(overflowing);
        assert!(validate_entities(entity.file_id, &[entity]).is_err());
    }
}
