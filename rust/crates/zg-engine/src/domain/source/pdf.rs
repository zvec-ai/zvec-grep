//! Physical PDF locations and provenance for extracted text.
//! `PdfRange` contains pages/regions only. Entity-local byte selectors resolve
//! through `PdfSourceMap`, which is stored once alongside the entity's text.

use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

/// A raw rectangle in top-left, 72-DPI page coordinates (points).
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PdfRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PartialEq for PdfRect {
    fn eq(&self, other: &Self) -> bool {
        [self.x, self.y, self.width, self.height].map(f32::to_bits)
            == [other.x, other.y, other.width, other.height].map(f32::to_bits)
    }
}
impl Eq for PdfRect {}

/// A physical PDF selection, independent of extracted text and its offsets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfRange {
    pub pages: Vec<PdfPageLocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfPageLocation {
    pub page: u32,
    pub page_width: f32,
    pub page_height: f32,
    /// Empty when only the page is known. Coordinates remain block-level.
    pub rects: Vec<PdfRect>,
}

impl PartialEq for PdfPageLocation {
    fn eq(&self, other: &Self) -> bool {
        self.page == other.page
            && self.page_width.to_bits() == other.page_width.to_bits()
            && self.page_height.to_bits() == other.page_height.to_bits()
            && self.rects == other.rects
    }
}
impl Eq for PdfPageLocation {}

impl PdfRange {
    pub(crate) fn page_numbers(&self) -> Vec<u32> {
        self.pages
            .iter()
            .map(|page| page.page)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn validate(&self) -> EngineResult<()> {
        let mut seen = BTreeSet::new();
        if self.pages.is_empty()
            || self.pages.iter().any(|page| {
                page.page == 0
                    || !seen.insert(page.page)
                    || !valid_dimensions(page.page_width, page.page_height)
                    || page.rects.iter().any(|rect| !valid_rect(*rect))
            })
        {
            return Err(EngineError::invalid_argument("invalid PDF location"));
        }
        Ok(())
    }
}

fn valid_dimensions(width: f32, height: f32) -> bool {
    width.is_finite() && width > 0.0 && height.is_finite() && height > 0.0
}

fn valid_rect(rect: PdfRect) -> bool {
    [rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|v| v.is_finite())
        && rect.width >= 0.0
        && rect.height >= 0.0
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
        self.page == other.page
            && self.id == other.id
            && self.page_width.to_bits() == other.page_width.to_bits()
            && self.page_height.to_bits() == other.page_height.to_bits()
            && self.bbox == other.bbox
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
    /// Resolve an entity-local text selection into physical pages and regions.
    /// Empty matches and generated separators keep the existing nearest-block policy.
    pub(crate) fn locate(&self, range: Range<usize>) -> EngineResult<PdfRange> {
        if range.start > range.end || range.end > self.text_len {
            return Err(EngineError::invalid_argument(
                "invalid PDF source selection",
            ));
        }
        let mut blocks = self
            .parts
            .iter()
            .filter(|part| part.range.start < range.end && range.start < part.range.end)
            .map(|part| &part.block)
            .collect::<Vec<_>>();
        if blocks.is_empty() {
            blocks.extend(
                self.parts
                    .iter()
                    .rev()
                    .find(|part| part.range.start <= range.start)
                    .or_else(|| self.parts.first())
                    .map(|part| &part.block),
            );
        }
        let mut pages = BTreeMap::<u32, PdfPageLocation>::new();
        let mut seen = BTreeSet::new();
        for block in blocks {
            let page = pages.entry(block.page).or_insert_with(|| PdfPageLocation {
                page: block.page,
                page_width: block.page_width,
                page_height: block.page_height,
                rects: Vec::new(),
            });
            if let Some(rect) = block.bbox
                && seen.insert((block.page, block.id))
            {
                page.rects.push(rect);
            }
        }
        let location = PdfRange {
            pages: pages.into_values().collect(),
        };
        location.validate()?;
        Ok(location)
    }

    /// Blocks with coordinates, once per (page, id), in first-seen source order.
    pub(crate) fn blocks_with_boxes(&self) -> impl Iterator<Item = (&PdfBlock, PdfRect)> {
        let mut seen = BTreeSet::new();
        self.parts.iter().filter_map(move |part| {
            let block = &part.block;
            let bbox = block.bbox?;
            seen.insert((block.page, block.id)).then_some((block, bbox))
        })
    }

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
                || !valid_dimensions(part.block.page_width, part.block.page_height)
                || part.block.bbox.is_some_and(|rect| !valid_rect(rect))
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

#[cfg(test)]
mod tests;
