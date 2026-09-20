use std::collections::{HashMap, HashSet};

use crate::{EngineError, EngineResult};

use super::content::ContentKind;
use super::{Content, EntityMetadata, FileId, SourceRange};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityId(String);

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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FragmentId(String);

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

/// A logical result unit with exactly one content object and its provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entity {
    pub id: EntityId,
    pub file_id: FileId,
    pub range: SourceRange,
    pub content: EntityContent,
    pub metadata: Option<EntityMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EntityContent {
    /// One atomic content object, routed as a whole to one embedding model.
    /// A future composite representation must be an explicit variant with its
    /// own composition contract; arbitrary lists are intentionally unsupported.
    Source(Content),
    /// Describes the entity's source range without reproducing its original content.
    Outline(String),
}

impl EntityContent {
    pub(crate) fn kind(&self) -> ContentKind {
        match self {
            Self::Source(content) => content.kind(),
            Self::Outline(_) => ContentKind::Text,
        }
    }
}

/// Retrieval ownership for fragments of one entity's content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EntityFragment {
    Standalone(Entity),
    Representative(Entity),
    Window(WindowFragment),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowFragment {
    pub id: FragmentId,
    pub entity_id: EntityId,
    pub file_id: FileId,
    pub range: SourceRange,
    pub content: Content,
}

impl EntityFragment {
    /// Entity and window IDs share one index document namespace.
    pub(crate) fn document_id(&self) -> &str {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => entity.id.as_str(),
            Self::Window(window) => window.id.as_str(),
        }
    }

    pub(crate) fn entity_id(&self) -> &EntityId {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => &entity.id,
            Self::Window(window) => &window.entity_id,
        }
    }

    pub(crate) fn file_id(&self) -> &FileId {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => &entity.file_id,
            Self::Window(window) => &window.file_id,
        }
    }

    pub(crate) fn range(&self) -> &SourceRange {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => &entity.range,
            Self::Window(window) => &window.range,
        }
    }

    /// A single source content object; outlines have no recorded source content.
    pub(crate) fn contents(&self) -> &[Content] {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => match &entity.content {
                EntityContent::Source(content) => std::slice::from_ref(content),
                EntityContent::Outline(_) => &[],
            },
            Self::Window(window) => std::slice::from_ref(&window.content),
        }
    }

    pub(crate) fn as_entity(&self) -> Option<&Entity> {
        match self {
            Self::Standalone(entity) | Self::Representative(entity) => Some(entity),
            Self::Window(_) => None,
        }
    }
}

/// Checks identities, ownership, and ranges for the fragments of one source file.
pub(crate) fn validate_fragments<'a>(
    file_id: FileId,
    fragments: impl IntoIterator<Item = &'a EntityFragment> + Clone,
) -> EngineResult<()> {
    let mut document_ids = HashSet::new();
    let mut representatives = HashMap::new();

    for fragment in fragments.clone() {
        if *fragment.file_id() != file_id {
            return Err(EngineError::invalid_argument(format!(
                "fragment {} belongs to a different source file",
                fragment.document_id()
            )));
        }
        if !document_ids.insert(fragment.document_id()) {
            return Err(EngineError::invalid_argument(format!(
                "duplicate fragment id: {}",
                fragment.document_id()
            )));
        }
        fragment.range().validate()?;
        let has_content = match fragment {
            EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
                match &entity.content {
                    EntityContent::Source(content) => content_has_value(content),
                    EntityContent::Outline(outline) => !outline.trim().is_empty(),
                }
            }
            EntityFragment::Window(window) => content_has_value(&window.content),
        };
        if !has_content {
            return Err(EngineError::invalid_argument(format!(
                "fragment {} has no content",
                fragment.document_id()
            )));
        }
        if let EntityFragment::Representative(entity) = fragment {
            representatives.insert(&entity.id, entity);
        }
    }

    for fragment in fragments {
        let EntityFragment::Window(window) = fragment else {
            continue;
        };
        let entity = representatives.get(&window.entity_id).ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "window {} has no representative entity: {}",
                window.id.as_str(),
                window.entity_id.as_str()
            ))
        })?;
        if !entity.range.contains(&window.range) {
            return Err(EngineError::invalid_argument(format!(
                "window {} lies outside entity {}",
                window.id.as_str(),
                window.entity_id.as_str()
            )));
        }
        if entity.content.kind() != window.content.kind() {
            return Err(EngineError::invalid_argument(format!(
                "window {} has a different content kind than entity {}",
                window.id.as_str(),
                window.entity_id.as_str()
            )));
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
    use crate::domain::{ByteRange, FileFormat, ImageContent, TextRange};

    fn file_id() -> FileId {
        FileId::new(1)
    }

    fn range(start: usize, end: usize) -> SourceRange {
        SourceRange::Text(
            TextRange::from_coordinates(start, end, 1, 1, start, end).expect("valid range"),
        )
    }

    fn entity(id: &str) -> Entity {
        Entity {
            id: EntityId::new(id).expect("entity id"),
            file_id: file_id(),
            range: range(0, 20),
            content: EntityContent::Source(Content::Text("abcdefghijklmnopqrst".to_owned())),
            metadata: None,
        }
    }

    fn window(id: &str, owner: &str) -> WindowFragment {
        WindowFragment {
            id: FragmentId::new(id).expect("fragment id"),
            entity_id: EntityId::new(owner).expect("owner id"),
            file_id: file_id(),
            range: range(5, 15),
            content: Content::Text("fghijklmno".to_owned()),
        }
    }

    fn assert_invalid(fragments: &[EntityFragment], message: &str) {
        let error = validate_fragments(file_id(), fragments).expect_err("invalid fragments");
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
        assert!(error.message().contains(message), "{error}");
    }

    #[test]
    fn resolves_overlapping_windows_without_requiring_input_order() {
        let fragments = vec![
            EntityFragment::Window(window("window-1", "group")),
            EntityFragment::Standalone(entity("single")),
            EntityFragment::Window(window("window-2", "group")),
            EntityFragment::Representative(entity("group")),
        ];

        validate_fragments(file_id(), &fragments).expect("valid file batch");
        assert_eq!(fragments.len(), 4);
        assert_eq!(
            fragments
                .iter()
                .filter_map(EntityFragment::as_entity)
                .count(),
            2
        );
        assert_eq!(fragments[0].entity_id(), fragments[3].entity_id());
        assert_ne!(fragments[0].document_id(), fragments[3].document_id());
    }

    #[test]
    fn rejects_collisions_across_entity_and_window_id_types() {
        let cases = [
            vec![
                EntityFragment::Standalone(entity("same")),
                EntityFragment::Representative(entity("same")),
            ],
            vec![
                EntityFragment::Representative(entity("same")),
                EntityFragment::Window(window("same", "same")),
            ],
            vec![
                EntityFragment::Representative(entity("group")),
                EntityFragment::Window(window("same", "group")),
                EntityFragment::Window(window("same", "group")),
            ],
        ];

        for fragments in cases {
            assert_invalid(&fragments, "duplicate fragment id");
        }
    }

    #[test]
    fn windows_must_reference_representatives() {
        let cases = [
            vec![EntityFragment::Window(window("window", "missing"))],
            vec![
                EntityFragment::Standalone(entity("single")),
                EntityFragment::Window(window("window", "single")),
            ],
            vec![
                EntityFragment::Representative(entity("group")),
                EntityFragment::Window(window("window-1", "group")),
                EntityFragment::Window(window("window-2", "window-1")),
            ],
        ];

        for fragments in cases {
            assert_invalid(&fragments, "no representative entity");
        }
    }

    #[test]
    fn rejects_wrong_files_invalid_ranges_and_outside_windows() {
        let mut wrong_file = window("window", "group");
        wrong_file.file_id = FileId::new(2);
        assert_invalid(
            &[
                EntityFragment::Representative(entity("group")),
                EntityFragment::Window(wrong_file),
            ],
            "different source file",
        );

        let mut outside = window("window", "group");
        outside.range = range(15, 21);
        assert_invalid(
            &[
                EntityFragment::Representative(entity("group")),
                EntityFragment::Window(outside),
            ],
            "lies outside",
        );

        let mut invalid = entity("invalid");
        invalid.range = SourceRange::Byte(ByteRange {
            start_offset: 2,
            end_offset: 1,
        });
        assert!(validate_fragments(file_id(), &[EntityFragment::Standalone(invalid)]).is_err());
    }

    #[test]
    fn keeps_all_windows_in_their_entity_content_kind() {
        let mut image_window = window("image-window", "text-entity");
        image_window.content =
            Content::Image(ImageContent::new(vec![1], FileFormat::Png).expect("image content"));
        assert_invalid(
            &[
                EntityFragment::Representative(entity("text-entity")),
                EntityFragment::Window(image_window),
            ],
            "different content kind",
        );
    }

    #[test]
    fn exposes_one_content_object_for_source_entities_and_windows() {
        let source = EntityFragment::Standalone(entity("source"));
        let fragment = EntityFragment::Window(window("window", "source"));
        assert_eq!(
            source.contents(),
            &[Content::Text("abcdefghijklmnopqrst".to_owned())]
        );
        assert_eq!(
            fragment.contents(),
            &[Content::Text("fghijklmno".to_owned())]
        );
        let mut image_entity = entity("image");
        image_entity.content = EntityContent::Source(Content::Image(
            ImageContent::new(vec![1], FileFormat::Png).expect("image content"),
        ));
        assert_eq!(image_entity.content.kind(), ContentKind::Image);
        let image = EntityFragment::Standalone(image_entity);
        validate_fragments(file_id(), [&image]).expect("single image entity");
    }

    #[test]
    fn representation_and_content_role_are_independent() {
        let source = entity("group");
        let mut outline = source.clone();
        outline.content = EntityContent::Outline("abcdefghijklmnopqrst".to_owned());
        assert_ne!(source.content, outline.content);

        for representative in [source, outline] {
            let mut equal_window = window("window", "group");
            equal_window.range = representative.range;
            equal_window.content = Content::Text("abcdefghijklmnopqrst".to_owned());
            let fragments = vec![
                EntityFragment::Representative(representative),
                EntityFragment::Window(equal_window),
            ];
            validate_fragments(file_id(), &fragments).expect("either content role is valid");
            validate_fragments(file_id(), &fragments[..1]).expect("windows are optional");
        }
        validate_fragments(file_id(), &[]).expect("empty source produces no fragments");
        let mut empty = entity("empty");
        empty.content = EntityContent::Source(Content::Text(String::new()));
        assert_invalid(&[EntityFragment::Standalone(empty)], "has no content");
        let mut empty_outline = entity("empty-outline");
        empty_outline.content = EntityContent::Outline(" ".to_owned());
        assert_invalid(
            &[EntityFragment::Representative(empty_outline)],
            "has no content",
        );
        let mut empty_window = window("empty-window", "group");
        empty_window.content = Content::Text(String::new());
        assert_invalid(&[EntityFragment::Window(empty_window)], "has no content");
    }

    #[test]
    fn ids_reject_blank_values_without_rewriting_valid_keys() {
        for id in ["", " ", "\t\n"] {
            assert!(EntityId::new(id).is_err());
            assert!(FragmentId::new(id).is_err());
        }
        let key = "  existing-id  ";
        assert_eq!(EntityId::new(key).expect("entity id").as_str(), key);
        assert_eq!(FragmentId::new(key).expect("fragment id").as_str(), key);
    }
}
