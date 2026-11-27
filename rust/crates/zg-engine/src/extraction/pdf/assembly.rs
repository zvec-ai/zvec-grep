//! `LiteParse` owns structure and Markdown. This adapter records exact serializer
//! byte intervals; it never fuzzy-matches text or reconstructs headings/tables.
use super::{PdfMarkdownSource, service::check};
use crate::{
    EngineError,
    domain::{FileFormat, PdfBlock, PdfRect, PdfSourceMap, PdfSourcePart, SourcePath},
    extraction::TextSource,
};
use liteparse::{
    LiteParse, LiteParseConfig, OutputFormat, stages,
    types::{OutlineTarget, Page, Rect},
};
use tokio_util::sync::CancellationToken;

pub(super) fn parser() -> LiteParse {
    LiteParse::new(LiteParseConfig {
        ocr_enabled: false,
        output_format: OutputFormat::Markdown,
        extract_blocks: true,
        quiet: true,
        max_pages: usize::MAX,
        continue_on_page_error: false,
        ..LiteParseConfig::default()
    })
}

pub(super) fn finish(
    mut pages: Vec<Page>,
    outline: Vec<OutlineTarget>,
    path: SourcePath,
    signal: &CancellationToken,
) -> Result<PdfMarkdownSource, EngineError> {
    check(signal)?;
    let parser = parser();
    stages::apply_content_filters(&mut pages, &parser.content_filters());
    let result = parser.parse_from_pages(pages, outline);
    let signals = stages::document_signals(&result.pages, parser.config().keep_headers_footers);
    let options = parser.block_options(&result.outline);
    let mut text = String::new();
    let mut parts = Vec::new();
    for page in &result.pages {
        check(signal)?;
        // Empty/scanned pages must not become searchable fenced-code placeholders.
        if page.text.trim().is_empty() {
            continue;
        }
        let blocks = stages::extract_blocks(page, &signals, &options);
        let rendered = stages::render_page_markdown(page, blocks.as_deref());
        if rendered != page.markdown {
            return Err(EngineError::internal(
                "LiteParse block rendering changed page Markdown",
            ));
        }
        if rendered.trim().is_empty() {
            continue;
        }
        // zg uses whitespace between pages, not LiteParse's synthetic horizontal
        // rule. Every non-whitespace byte below belongs to a real source block.
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let page_start = text.len();
        let page_number = u32::try_from(page.page_number)
            .map_err(|_| EngineError::invalid_argument("PDF page number overflow"))?;
        if let Some(blocks) = blocks {
            // PositionedBlock uses the projected layout frame. The public view
            // maps it back into the physical page, including column transforms.
            let located = page
                .blocks
                .as_ref()
                .ok_or_else(|| EngineError::internal("LiteParse public blocks missing"))?;
            if located.len() != blocks.len() {
                return Err(EngineError::internal(
                    "LiteParse public block boundaries changed",
                ));
            }
            let spans = block_spans(&blocks, &rendered)?;
            for (index, range) in spans.into_iter().enumerate() {
                if range.is_empty() {
                    continue;
                }
                parts.push(PdfSourcePart {
                    range: page_start + range.start..page_start + range.end,
                    block: PdfBlock {
                        page: page_number,
                        id: u32::try_from(index + 1).map_err(|_| {
                            EngineError::invalid_argument("PDF block number overflow")
                        })?,
                        page_width: page.page_width,
                        page_height: page.page_height,
                        bbox: located[index].bbox.as_ref().map(raw_box),
                    },
                });
            }
        } else {
            // Native projection fallback is still page-locatable; no invented box.
            parts.push(PdfSourcePart {
                range: page_start..page_start + rendered.len(),
                block: PdfBlock {
                    page: page_number,
                    id: 1,
                    page_width: page.page_width,
                    page_height: page.page_height,
                    bbox: None,
                },
            });
        }
        text.push_str(&rendered);
    }
    let source_map = PdfSourceMap {
        markdown_start: 0,
        text_len: text.len(),
        parts,
    };
    if !text.is_empty() {
        source_map.validate(&text)?;
    }
    Ok(PdfMarkdownSource {
        warnings: Vec::new(),
        text: TextSource {
            relative_path: path,
            formats: vec![FileFormat::Pdf],
            text,
        },
        source_map,
    })
}

fn block_spans(
    blocks: &[stages::PositionedBlock],
    rendered: &str,
) -> Result<Vec<std::ops::Range<usize>>, EngineError> {
    let singles: Vec<_> = blocks
        .iter()
        .map(|b| stages::render_blocks(std::slice::from_ref(b)))
        .collect();
    let mut rebuilt = String::new();
    let mut spans = Vec::with_capacity(blocks.len());
    for (index, single) in singles.iter().enumerate() {
        if index > 0 {
            let pair = stages::render_blocks(&blocks[index - 1..=index]);
            let previous = &singles[index - 1];
            if pair.len() < previous.len() + single.len()
                || !pair.starts_with(previous)
                || !pair.ends_with(single)
            {
                return Err(EngineError::internal(
                    "LiteParse serializer has nonlocal block boundaries",
                ));
            }
            rebuilt.push_str(&pair[previous.len()..pair.len() - single.len()]);
        }
        let start = rebuilt.len();
        rebuilt.push_str(single);
        spans.push(start..rebuilt.len());
    }
    if rebuilt != rendered {
        return Err(EngineError::internal(
            "LiteParse block spans do not reconstruct page Markdown",
        ));
    }
    Ok(spans)
}

fn raw_box(b: &Rect) -> PdfRect {
    PdfRect {
        x: b.x,
        y: b.y,
        width: b.width,
        height: b.height,
    }
}

#[cfg(test)]
#[path = "assembly_tests.rs"]
mod tests;
