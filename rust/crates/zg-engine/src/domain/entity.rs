use crate::{EngineError, EngineResult};

use super::{Content, EntityMetadata, FileId, PdfSourceMap, Range};

/// A logical search unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entity {
    id: EntityId,
    file_id: FileId,
    /// Range within the original source.
    source_range: Range,
    content: Content,
    /// Provenance belongs to the extracted text, not to its physical location.
    source_map: Option<std::sync::Arc<PdfSourceMap>>,
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
            validate_fragment_range(&content, fragment.range.clone())?;
        }
        if let Range::Pdf(location) = &source_range {
            location.validate()?;
        }
        Ok(Self {
            id,
            file_id,
            source_range,
            content,
            source_map: None,
            metadata,
            fragments,
        })
    }

    pub(crate) fn with_source_map(
        mut self,
        map: Option<std::sync::Arc<PdfSourceMap>>,
    ) -> EngineResult<Self> {
        if let Some(map) = &map {
            let Content::Text(text) = &self.content else {
                return Err(EngineError::invalid_argument(
                    "PDF source maps require text content",
                ));
            };
            map.validate(text)?;
            if self.source_range != Range::Pdf(map.locate(0..text.len())?.into()) {
                return Err(EngineError::invalid_argument(
                    "PDF source map disagrees with entity location",
                ));
            }
        } else if matches!(
            (&self.source_range, &self.content),
            (Range::Pdf(_), Content::Text(_) | Content::Code(_))
        ) {
            return Err(EngineError::invalid_argument(
                "PDF text requires a source map",
            ));
        }
        self.source_map = map;
        Ok(self)
    }

    pub(crate) fn source_map(&self) -> Option<&std::sync::Arc<PdfSourceMap>> {
        self.source_map.as_ref()
    }

    /// Offsets in extracted document text, when present, otherwise source bytes.
    pub(crate) fn source_byte_offsets(&self) -> Option<(u64, u64)> {
        self.source_map().map_or_else(
            || self.source_range.byte_offsets(),
            |map| {
                Some((
                    u64::try_from(map.markdown_start).ok()?,
                    u64::try_from(map.markdown_start.checked_add(map.text_len)?).ok()?,
                ))
            },
        )
    }

    pub(crate) fn id(&self) -> &EntityId {
        &self.id
    }

    pub(crate) fn file_id(&self) -> FileId {
        self.file_id
    }

    pub(crate) fn source_range(&self) -> Range {
        self.source_range.clone()
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

/// Content and physical location identify an entity within its file.
/// Converted text also includes its document offset to distinguish repeated text.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EntityId(String);

impl EntityId {
    pub(crate) fn new(
        file_id: FileId,
        content: &Content,
        source_range: &Range,
    ) -> EngineResult<Self> {
        let range = serde_json::to_vec(source_range).map_err(|error| {
            EngineError::internal(format!("serialize entity source range: {error}"))
        })?;
        let fingerprint = content.fingerprint();
        let hash = crate::utils::sha256_hex_parts([fingerprint.as_slice(), range.as_slice()]);
        Ok(Self(format!("{:08x}{}", file_id.get(), &hash[..24])))
    }

    pub(crate) fn new_with_source_map(
        file_id: FileId,
        content: &Content,
        source_range: &Range,
        source_map: Option<&PdfSourceMap>,
    ) -> EngineResult<Self> {
        let base = Self::new(file_id, content, source_range)?;
        let Some(map) = source_map else {
            return Ok(base);
        };
        // Repeated text can share a page/block rectangle. Its extracted-document
        // offset distinguishes those entities without putting offsets in PdfRange.
        let origin = u64::try_from(map.markdown_start)
            .map_err(|_| EngineError::invalid_argument("PDF text origin exceeds u64"))?
            .to_le_bytes();
        let hash = crate::utils::sha256_hex_parts([base.as_str().as_bytes(), origin.as_slice()]);
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

/// Whether content names something a search can return: text with no
/// non-whitespace character and images with no bytes are not indexable.
pub(crate) fn content_has_value(content: &Content) -> bool {
    match content {
        Content::Text(text) | Content::Code(text) => !text.trim().is_empty(),
        Content::Image(image) => !image.data().is_empty(),
    }
}
