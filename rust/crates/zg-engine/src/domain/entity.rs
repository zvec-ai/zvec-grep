use std::collections::HashSet;

use crate::{EngineError, EngineResult};

use super::{Content, EntityMetadata, FileId, Range};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FragmentId(String);

impl EntityId {
    pub(crate) fn new(
        file_id: FileId,
        content: &Content,
        source_range: Range,
    ) -> EngineResult<Self> {
        let bytes = serde_json::to_vec(&(content, source_range)).map_err(|error| {
            EngineError::internal(format!("serialize entity identity: {error}"))
        })?;
        let hash = crate::utils::sha256_hex(&bytes);
        Ok(Self(format!("{:08x}{}", file_id.get(), &hash[..24])))
    }

    pub(crate) fn from_string(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FragmentId {
    pub(crate) fn new(entity_id: &EntityId, ordinal: u32) -> Self {
        Self(format!("{}{:08x}", entity_id.as_str(), ordinal))
    }

    pub(crate) fn from_string(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A logical search unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entity {
    pub id: EntityId,
    pub file_id: FileId,
    pub source_range: Range,
    pub content: Content,
    pub metadata: Option<EntityMetadata>,
    pub fragments: Vec<EntityFragment>,
}

/// A unit used by the underlying search engine during retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityFragment {
    pub id: FragmentId,
    pub range: Range,
}

impl Entity {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        let mut fragment_ids = HashSet::new();
        if !content_has_value(&self.content) || self.fragments.is_empty() {
            return Err(EngineError::invalid_argument(
                "entity requires content and at least one fragment",
            ));
        }
        // Build line starts once per entity so validating every fragment stays linear
        // in the content size, rather than rescanning each fragment's prefix.
        let text_coordinates = match &self.content {
            Content::Text(text) => Some((
                text.as_str(),
                crate::utils::line_byte_offsets(&text.split('\n').collect::<Vec<_>>()),
            )),
            _ => None,
        };
        for fragment in &self.fragments {
            if !fragment_ids.insert(&fragment.id) {
                return Err(EngineError::invalid_argument("duplicate fragment id"));
            }
            validate_fragment_content(&self.content, fragment.range)?;
            if let (Range::Byte(range), Some((text, lines))) = (fragment.range, &text_coordinates) {
                let start = usize::try_from(range.start_offset()).map_err(|_| {
                    EngineError::invalid_argument("fragment start offset exceeds platform limits")
                })?;
                let end = usize::try_from(range.end_offset()).map_err(|_| {
                    EngineError::invalid_argument("fragment end offset exceeds platform limits")
                })?;
                let local = crate::utils::text_range_from_offsets(text, lines, start, end)?;
                match self.source_range {
                    Range::Text(origin) => {
                        crate::utils::map_text_range(local, origin)?;
                    }
                    Range::Full => {}
                    Range::Byte(_) => {
                        return Err(EngineError::invalid_argument(
                            "fragment content coordinates cannot be mapped to this entity source",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_fragment_content(content: &Content, range: Range) -> EngineResult<()> {
    match (range, content) {
        (Range::Full, _) => Ok(()),
        (Range::Byte(range), Content::Text(text)) => {
            let start = usize::try_from(range.start_offset()).map_err(|_| {
                EngineError::invalid_argument("fragment start offset exceeds platform limits")
            })?;
            let end = usize::try_from(range.end_offset()).map_err(|_| {
                EngineError::invalid_argument("fragment end offset exceeds platform limits")
            })?;
            if crate::utils::slice_text(text, start, end)?.is_empty() {
                return Err(EngineError::invalid_argument(
                    "fragment byte range must not be empty",
                ));
            }
            Ok(())
        }
        _ => Err(EngineError::invalid_argument(
            "fragments use Full or entity-relative byte ranges for text; images and tables require Full",
        )),
    }
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
