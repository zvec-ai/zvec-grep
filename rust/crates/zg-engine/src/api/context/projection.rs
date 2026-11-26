//! Maps entity-local text selections into source coordinates for all retrieval paths.
use super::result::ContentRange;
use crate::{
    EngineError, EngineResult,
    domain::{Content, Entity, Range},
};

impl ContentRange {
    pub(crate) fn project_text(
        entity: &Entity,
        span: std::ops::Range<usize>,
    ) -> EngineResult<Self> {
        let (Content::Text(text) | Content::Code(text)) = entity.content() else {
            return Err(EngineError::invalid_argument(
                "source projection requires text",
            ));
        };
        let source = entity.source_range();
        // Validate UTF-8 boundaries even for sources whose coordinates are not lines.
        crate::utils::slice_text(text, span.start, span.end)?;
        match &source {
            Range::Pdf(_) => {
                let map = entity.source_map().ok_or_else(|| {
                    EngineError::invalid_argument("PDF text selection requires a source map")
                })?;
                let location = map.locate(span.clone())?;
                Ok(Self::Pdf {
                    pages: location.page_numbers(),
                    start_byte_offset: map.markdown_start + span.start,
                    end_byte_offset: map.markdown_start + span.end,
                })
            }
            Range::Full | Range::Text(_) => {
                let lines = crate::utils::line_byte_offsets(text);
                let local =
                    crate::utils::text_range_from_offsets(text, &lines, span.start, span.end)?;
                let source = match &source {
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
        entity: &Entity,
        spans: impl IntoIterator<Item = std::ops::Range<usize>>,
    ) -> EngineResult<Self> {
        let (Content::Text(text) | Content::Code(text)) = entity.content() else {
            return Err(EngineError::invalid_argument(
                "source projection requires text",
            ));
        };
        let mut start = text.len();
        let mut end = 0;
        let mut pages = std::collections::BTreeSet::new();
        for span in spans {
            start = start.min(span.start);
            end = end.max(span.end);
            if let Self::Pdf {
                pages: selected, ..
            } = Self::project_text(entity, span)?
            {
                pages.extend(selected);
            }
        }
        let mut range = Self::project_text(entity, start..end)?;
        if let Self::Pdf {
            pages: selected, ..
        } = &mut range
        {
            *selected = pages.into_iter().collect();
        }
        Ok(range)
    }
}
