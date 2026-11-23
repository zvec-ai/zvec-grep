//! Optional mappings from parsed text to source locations, separate from splitting.
//!
//! Text offsets always address the map's borrowed UTF-8 text, never an original
//! binary file. Format adapters own location types, coverage policy, and storage.

use std::ops::Range;

use crate::{
    EngineError, EngineResult,
    domain::TextRange,
    utils::{line_byte_offsets, slice_text, text_range_from_offsets},
};

/// Selects a half-open interval in the associated text. Implementations reject
/// reversed, out-of-bounds, and non-UTF-8 boundaries. Empty selections are valid
/// when their boundaries are valid.
/// The output may describe multiple source locations at format-defined precision.
pub(super) trait SourceMap {
    type Selection;

    fn map_range(&self, range: Range<usize>) -> EngineResult<Self::Selection>;
}

/// Exact coordinates in decoded source text, with Markdown's existing line and
/// byte-column conventions. Decoded offsets need not equal on-disk byte offsets.
pub(super) struct TextSourceMap<'text> {
    text: &'text str,
    line_starts: Vec<usize>,
}

impl<'text> TextSourceMap<'text> {
    pub(super) fn new(text: &'text str) -> Self {
        Self {
            text,
            line_starts: line_byte_offsets(text),
        }
    }

    pub(super) fn line_starts(&self) -> &[usize] {
        &self.line_starts
    }
}

impl SourceMap for TextSourceMap<'_> {
    type Selection = TextRange;

    fn map_range(&self, range: Range<usize>) -> EngineResult<Self::Selection> {
        text_range_from_offsets(self.text, &self.line_starts, range.start, range.end)
    }
}

/// A nonempty interval relative to its owning map's text, with an opaque source
/// location. Slicing clips only `text_range`: a block location stays a whole block,
/// rather than implying character-level geometry or affine source offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "Opt-in mapping for converted-format adapters.")
)]
pub(super) struct SourcePart<L> {
    pub text_range: Range<usize>,
    pub location: L,
}

/// Provenance for converted text. Parts are ordered by start offset; overlaps
/// and repeated locations are allowed so text can have multiple origins. Gaps
/// are allowed, including generated non-whitespace text. The adapter decides
/// whether its format requires complete coverage or particular location values.
///
/// Construct a map over the complete parsed document, then select entities and
/// fragments with `map_range`. Selections borrow their text, own rebased parts,
/// and retain their position in that original document. No serialization or
/// domain range representation is imposed on callers.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "Opt-in mapping for converted-format adapters.")
)]
pub(super) struct SegmentSourceMap<'text, L> {
    text: &'text str,
    text_start: usize,
    parts: Vec<SourcePart<L>>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "Opt-in mapping for converted-format adapters.")
)]
impl<'text, L> SegmentSourceMap<'text, L> {
    pub(super) fn new(text: &'text str, parts: Vec<SourcePart<L>>) -> EngineResult<Self> {
        let mut previous_start = 0;
        for part in &parts {
            let range = &part.text_range;
            if range.start >= range.end || range.start < previous_start {
                return Err(EngineError::invalid_argument(
                    "source parts must be nonempty and ordered by text start",
                ));
            }
            slice_text(text, range.start, range.end)?;
            previous_start = range.start;
        }
        Ok(Self {
            text,
            text_start: 0,
            parts,
        })
    }

    pub(super) fn text(&self) -> &'text str {
        self.text
    }

    /// Position in the original parsed document, not in the binary source file.
    pub(super) fn text_range(&self) -> Range<usize> {
        self.text_start..self.text_start + self.text.len()
    }

    pub(super) fn parts(&self) -> &[SourcePart<L>] {
        &self.parts
    }
}

impl<L: Clone> SourceMap for SegmentSourceMap<'_, L> {
    type Selection = Self;

    fn map_range(&self, range: Range<usize>) -> EngineResult<Self::Selection> {
        let text = slice_text(self.text, range.start, range.end)?;
        let parts = self
            .parts
            .iter()
            .take_while(|part| part.text_range.start < range.end)
            .filter_map(|part| {
                let start = part.text_range.start.max(range.start);
                let end = part.text_range.end.min(range.end);
                (start < end).then(|| SourcePart {
                    text_range: start - range.start..end - range.start,
                    location: part.location.clone(),
                })
            })
            .collect();
        // Validation keeps every selection within the original borrowed text,
        // so the accumulated offset and selected end cannot overflow.
        Ok(Self {
            text,
            text_start: self.text_start + range.start,
            parts,
        })
    }
}

#[cfg(test)]
mod tests;
