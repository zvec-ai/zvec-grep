//! Maps entity-local text selections into source coordinates for all retrieval paths.
use super::result::ContentRange;
use crate::{EngineError, EngineResult, domain::Range};

impl ContentRange {
    pub(crate) fn project_text(
        source: &Range,
        text: &str,
        span: std::ops::Range<usize>,
    ) -> EngineResult<Self> {
        // Validate UTF-8 boundaries even for sources whose coordinates are not lines.
        crate::utils::slice_text(text, span.start, span.end)?;
        match source {
            Range::Pdf(map) => Ok(Self::Pdf {
                pages: map.selection_pages(span.clone()),
                start_byte_offset: map.markdown_start + span.start,
                end_byte_offset: map.markdown_start + span.end,
            }),
            Range::Full | Range::Text(_) => {
                let lines = crate::utils::line_byte_offsets(text);
                let local =
                    crate::utils::text_range_from_offsets(text, &lines, span.start, span.end)?;
                let source = match source {
                    Range::Text(origin) => crate::utils::map_text_range(local, *origin)?,
                    _ => local,
                };
                Ok(source.into())
            }
            Range::Byte(_) => Err(EngineError::invalid_argument(
                "fragment content coordinates cannot be mapped to this entity source",
            )),
        }
    }

    /// Bounding offsets describe the selected span; page membership is the union
    /// of actual selections, excluding unmatched pages between separate hits.
    pub(crate) fn project_selections(
        source: &Range,
        text: &str,
        spans: impl IntoIterator<Item = std::ops::Range<usize>>,
    ) -> EngineResult<Self> {
        let mut start = text.len();
        let mut end = 0;
        let mut pages = std::collections::BTreeSet::new();
        for span in spans {
            start = start.min(span.start);
            end = end.max(span.end);
            if let Self::Pdf {
                pages: selected, ..
            } = Self::project_text(source, text, span)?
            {
                pages.extend(selected);
            }
        }
        let mut range = Self::project_text(source, text, start..end)?;
        if let Self::Pdf {
            pages: selected, ..
        } = &mut range
        {
            *selected = pages.into_iter().collect();
        }
        Ok(range)
    }
}
