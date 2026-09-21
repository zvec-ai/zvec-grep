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
    /// Range within the original source.
    pub source_range: Range,
    pub content: Content,
    pub metadata: Option<EntityMetadata>,
    pub fragments: Vec<EntityFragment>,
}

/// A unit used by the underlying search engine during retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityFragment {
    pub id: FragmentId,
    /// Range within the owning entity's content.
    pub range: Range,
}

impl Entity {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if !content_has_value(&self.content) || self.fragments.is_empty() {
            return Err(EngineError::invalid_argument(
                "entity requires content and at least one fragment",
            ));
        }
        for fragment in &self.fragments {
            validate_fragment_range(&self.content, fragment.range)?;
        }
        Ok(())
    }
}

fn validate_fragment_range(content: &Content, range: Range) -> EngineResult<()> {
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
