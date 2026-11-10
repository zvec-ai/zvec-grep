use crate::{EngineError, EngineResult};

use super::{Content, EntityMetadata, FileId, Range};

/// A logical search unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entity {
    id: EntityId,
    file_id: FileId,
    /// Range within the original source.
    source_range: Range,
    content: Content,
    metadata: Option<EntityMetadata>,
    fragments: Vec<EntityFragment>,
}

impl Entity {
    pub(crate) fn new(
        id: EntityId,
        file_id: FileId,
        source_range: Range,
        content: Content,
        metadata: Option<EntityMetadata>,
        fragments: Vec<EntityFragment>,
    ) -> EngineResult<Self> {
        if !content_has_value(&content) || fragments.is_empty() {
            return Err(EngineError::invalid_argument(
                "entity requires content and at least one fragment",
            ));
        }
        for fragment in &fragments {
            validate_fragment_range(&content, fragment.range)?;
        }
        Ok(Self {
            id,
            file_id,
            source_range,
            content,
            metadata,
            fragments,
        })
    }

    pub(crate) fn id(&self) -> &EntityId {
        &self.id
    }

    pub(crate) fn file_id(&self) -> FileId {
        self.file_id
    }

    pub(crate) fn source_range(&self) -> Range {
        self.source_range
    }

    pub(crate) fn content(&self) -> &Content {
        &self.content
    }

    pub(crate) fn metadata(&self) -> Option<&EntityMetadata> {
        self.metadata.as_ref()
    }

    pub(crate) fn fragments(&self) -> &[EntityFragment] {
        &self.fragments
    }
}

/// A unit used by the underlying search engine during retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntityFragment {
    pub id: FragmentId,
    /// Range within the owning entity's content.
    pub range: Range,
}

/// `hex8(file_id) + hex(SHA256(content.fingerprint() + JSON(source_range)))[..24]`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityId(String);

impl EntityId {
    pub(crate) fn new(
        file_id: FileId,
        content: &Content,
        source_range: Range,
    ) -> EngineResult<Self> {
        let fingerprint = content.fingerprint();
        let range = serde_json::to_vec(&source_range).map_err(|error| {
            EngineError::internal(format!("serialize entity source range: {error}"))
        })?;
        let hash = crate::utils::sha256_hex_parts([fingerprint.as_slice(), range.as_slice()]);
        Ok(Self(format!("{:08x}{}", file_id.get(), &hash[..24])))
    }

    pub(crate) fn from_string(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// `entity_id + hex8(ordinal)`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FragmentId(String);

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

fn validate_fragment_range(content: &Content, range: Range) -> EngineResult<()> {
    match (range, content) {
        (Range::Full, _) => Ok(()),
        (Range::Byte(range), Content::Text(text) | Content::Code(text)) => {
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
            "fragments use Full or entity-relative byte ranges for text and code; images require Full",
        )),
    }
}

fn content_has_value(content: &Content) -> bool {
    match content {
        Content::Text(text) | Content::Code(text) => !text.trim().is_empty(),
        Content::Image(image) => !image.data().is_empty(),
    }
}
