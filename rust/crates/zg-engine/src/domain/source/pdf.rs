//! Provenance for PDF text after layout assembly and Markdown extraction.

use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ops::Range};

/// A raw rectangle in top-left, 72-DPI page coordinates (points).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PdfRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// An original content block. Pages and block IDs are one-based.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfBlock {
    pub page: u32,
    pub id: u32,
    pub page_width: f32,
    pub page_height: f32,
    pub bbox: Option<PdfRect>,
}

// Provenance identity compares exact float representations, without rounding.
impl PartialEq for PdfBlock {
    fn eq(&self, other: &Self) -> bool {
        let bits = |b: PdfRect| [b.x, b.y, b.width, b.height].map(f32::to_bits);
        self.page == other.page
            && self.id == other.id
            && self.page_width.to_bits() == other.page_width.to_bits()
            && self.page_height.to_bits() == other.page_height.to_bits()
            && self.bbox.map(bits) == other.bbox.map(bits)
    }
}
impl Eq for PdfBlock {}

/// A half-open UTF-8 interval mapped to an original PDF block.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfSourcePart {
    pub range: Range<usize>,
    pub block: PdfBlock,
}

/// Parts are relative to the owning entity's text, not the PDF binary.
/// `markdown_start` locates the entity in the transient assembled document and
/// distinguishes identical entities without persisting that whole document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfSourceMap {
    pub markdown_start: usize,
    pub text_len: usize,
    pub parts: Vec<PdfSourcePart>,
}

impl PdfSourceMap {
    pub(crate) fn validate(&self, text: &str) -> EngineResult<()> {
        if self.text_len != text.len()
            || self.markdown_start.checked_add(self.text_len).is_none()
            || self.parts.is_empty()
        {
            return Err(EngineError::invalid_argument("invalid PDF text source map"));
        }
        let mut previous_end = 0;
        for part in &self.parts {
            let range = &part.range;
            if range.start < previous_end
                || range.start >= range.end
                || range.end > text.len()
                || !text.is_char_boundary(range.start)
                || !text.is_char_boundary(range.end)
                || part.block.page == 0
                || part.block.id == 0
                || !part.block.page_width.is_finite()
                || part.block.page_width <= 0.0
                || !part.block.page_height.is_finite()
                || part.block.page_height <= 0.0
                || part.block.bbox.is_some_and(|b| {
                    ![b.x, b.y, b.width, b.height].iter().all(|v| v.is_finite())
                        || b.width < 0.0
                        || b.height < 0.0
                })
            {
                return Err(EngineError::invalid_argument("invalid PDF source part"));
            }
            // Only whitespace separators inserted by the serializer may be unmapped.
            if !text[previous_end..range.start].trim().is_empty() {
                return Err(EngineError::invalid_argument(
                    "PDF text is missing provenance",
                ));
            }
            previous_end = range.end;
        }
        if !text[previous_end..].trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "PDF text is missing provenance",
            ));
        }
        Ok(())
    }

    pub(crate) fn pages(&self, range: Range<usize>) -> Vec<u32> {
        self.parts
            .iter()
            .filter(|part| part.range.start < range.end && range.start < part.range.end)
            .map(|part| part.block.page)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Resolve even empty matches or separators to a nearby source block.
    pub(crate) fn selection_pages(&self, range: Range<usize>) -> Vec<u32> {
        let pages = self.pages(range.clone());
        if !pages.is_empty() {
            return pages;
        }
        self.parts
            .iter()
            .rev()
            .find(|part| part.range.start <= range.start)
            .or_else(|| self.parts.first())
            .map(|part| vec![part.block.page])
            .unwrap_or_default()
    }

    /// Selects an entity or fragment and rebases its source parts.
    pub(crate) fn slice(&self, range: Range<usize>, text: &str) -> EngineResult<Self> {
        if range.start > range.end || range.end > self.text_len || range.len() != text.len() {
            return Err(EngineError::invalid_argument(
                "invalid PDF source selection",
            ));
        }
        let parts = self
            .parts
            .iter()
            .filter_map(|part| {
                let start = part.range.start.max(range.start);
                let end = part.range.end.min(range.end);
                (start < end).then(|| PdfSourcePart {
                    range: start - range.start..end - range.start,
                    block: part.block.clone(),
                })
            })
            .collect();
        let selected = Self {
            markdown_start: self
                .markdown_start
                .checked_add(range.start)
                .ok_or_else(|| EngineError::invalid_argument("PDF source offset overflow"))?,
            text_len: text.len(),
            parts,
        };
        selected.validate(text)?;
        Ok(selected)
    }
}
