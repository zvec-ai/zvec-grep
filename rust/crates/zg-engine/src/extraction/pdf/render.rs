//! One native reader owns the `PDFium` lock and streams bounded page rasters.
use super::{
    assembly, filter,
    service::{PdfExtractionError, check},
};
use crate::{EngineError, models::LayoutImage};
use liteparse::{
    stages,
    types::{OutlineTarget, Page},
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub(super) struct RenderedPage {
    pub page: Page,
    pub tables: Vec<[f64; 4]>,
    pub image: Result<Option<LayoutImage>, String>,
}

pub(super) fn document_error(e: impl std::fmt::Display) -> PdfExtractionError {
    EngineError::invalid_argument(format!("Cannot extract PDF: {e}")).into()
}

pub(super) fn produce(
    bytes: &[u8],
    sender: &Sender<RenderedPage>,
    signal: &CancellationToken,
    layout_disabled: &AtomicBool,
) -> Result<Vec<OutlineTarget>, PdfExtractionError> {
    check(signal)?;
    let parser = assembly::parser();
    let library = zg_pdfium::acquire().map_err(|error| {
        PdfExtractionError::Runtime(EngineError::internal(format!(
            "Cannot load LiteParse PDFium: {error}"
        )))
    })?;
    check(signal)?;
    let document = library
        .load_document_from_bytes(bytes, None)
        .map_err(document_error)?;
    let outline = stages::outline(&document);
    let extracted = stages::extract(&document, &parser.extract_request(None, usize::MAX))
        .map_err(document_error)?;
    if !extracted.page_errors.is_empty()
        || extracted.pages.len()
            != usize::try_from(document.page_count()).map_err(document_error)?
    {
        return Err(document_error("incomplete page extraction"));
    }
    check(signal)?;
    let mut native_pages = extracted.pages.clone();
    stages::apply_content_filters(&mut native_pages, &parser.content_filters());
    let native = parser.parse_from_pages(native_pages, outline.clone());
    let mut guards: HashMap<_, Vec<_>> = native
        .pages
        .into_iter()
        .map(|page| {
            let tables = page
                .blocks
                .unwrap_or_default()
                .into_iter()
                .filter(|block| matches!(block.kind.as_str(), "table" | "merged_table"))
                .filter_map(|block| block.bbox)
                .map(|b| filter::rect(&b))
                .filter(|b| b.iter().all(|v| v.is_finite()) && filter::area(*b) > 0.0)
                .collect();
            (page.page_number, tables)
        })
        .collect();
    for page in extracted.pages {
        check(signal)?;
        let width = f64::from(page.page_width);
        let height = f64::from(page.page_height);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(document_error(format!(
                "page {}: invalid page size",
                page.page_number
            )));
        }
        let image = if !layout_disabled.load(Ordering::Relaxed)
            && page
                .text_items
                .iter()
                .any(|item| !item.text.trim().is_empty())
        {
            (|| {
                let index = i32::try_from(page.page_number - 1).map_err(|e| e.to_string())?;
                let native = document.page(index).map_err(|e| e.to_string())?;
                let rendered = zg_pdfium::render_rgb(&native, 1.5).map_err(|e| e.to_string())?;
                Ok(Some(LayoutImage {
                    width: rendered.width,
                    height: rendered.height,
                    rgb: rendered.rgb.into(),
                }))
            })()
        } else {
            Ok(None)
        };
        let tables = guards.remove(&page.page_number).unwrap_or_default();
        sender
            .blocking_send(RenderedPage {
                page,
                tables,
                image,
            })
            .map_err(|_| EngineError::cancelled("PDF extraction consumer closed"))?;
    }
    Ok(outline)
}
