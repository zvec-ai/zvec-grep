use super::{
    assembly::{Bookmarks, Item},
    service::{PdfExtractionError, check},
};
use crate::{EngineError, models::layout::LayoutImage};
use pdf_inspector::{PositionFrame, extract_text_with_positions_mem_in_frame, extractor::ItemType};
use pdfium_render::prelude::*;
use std::{collections::BTreeMap, sync::OnceLock};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub(super) struct RenderedPage {
    pub number: u32,
    pub width: f64,
    pub height: f64,
    pub items: Vec<Item>,
    pub image: Option<LayoutImage>,
}

fn runtime() -> Result<&'static Pdfium, PdfExtractionError> {
    static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();
    PDFIUM
        .get_or_init(|| {
            Pdfium::bind_to_statically_linked_library()
                .map(Pdfium::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| {
            PdfExtractionError::Runtime(EngineError::internal(format!(
                "Unable to initialize statically linked PDFium: {e}"
            )))
        })
}
fn document_error(e: impl std::fmt::Display) -> PdfExtractionError {
    EngineError::invalid_argument(format!("Cannot extract PDF: {e}")).into()
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_lines,
    reason = "Raster dimensions are finite and clamped to 1600 pixels"
)]
pub(super) fn produce(
    bytes: &[u8],
    sender: &Sender<RenderedPage>,
    signal: &CancellationToken,
) -> Result<Bookmarks, PdfExtractionError> {
    check(signal)?;
    let native = extract_text_with_positions_mem_in_frame(bytes, None, PositionFrame::Display)
        .map_err(document_error)?;
    check(signal)?;
    let mut by_page = BTreeMap::new();
    for i in native {
        if matches!(i.item_type, ItemType::Text | ItemType::FormField) && !i.text.trim().is_empty()
        {
            if ![i.x, i.y, i.width, i.height, i.font_size]
                .into_iter()
                .all(f32::is_finite)
            {
                return Err(document_error("non-finite native text geometry"));
            }
            by_page.entry(i.page).or_insert_with(Vec::new).push(i);
        }
    }
    let pdf = runtime()?
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(document_error)?;
    let mut bookmarks = Bookmarks::new();
    for mark in pdf.bookmarks().iter() {
        check(signal)?;
        if let (Some(title), Some(dest)) = (mark.title(), mark.destination())
            && let Ok(index) = dest
                .page_index()
                .and_then(|i| u32::try_from(i).map_err(|_| PdfiumError::PageIndexOutOfBounds))
        {
            let mut level = 1;
            let mut parent = mark.parent();
            while let Some(p) = parent {
                level += 1;
                if level > 128 {
                    break;
                }
                parent = p.parent();
            }
            bookmarks.entry(index + 1).or_default().push((title, level));
        }
    }
    for index in 0..pdf.pages().len() {
        let page = pdf
            .pages()
            .get(index)
            .map_err(|e| document_error(format!("page {}: {e}", index + 1)))?;
        check(signal)?;
        let number = u32::try_from(index).map_err(|_| document_error("invalid page index"))? + 1;
        let width = f64::from(page.width().value);
        let height = f64::from(page.height().value);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(document_error(format!("page {number}: invalid page size")));
        }
        let items = by_page
            .remove(&number)
            .unwrap_or_default()
            .into_iter()
            .map(|i| Item {
                text: i.text,
                bounds: [
                    f64::from(i.x),
                    height - f64::from(i.y) - f64::from(i.height),
                    f64::from(i.x) + f64::from(i.width),
                    height - f64::from(i.y),
                ],
                size: f64::from(i.font_size),
                bold: i.is_bold,
            })
            .collect::<Vec<_>>();
        let image = if items.is_empty() {
            None
        } else {
            let scale = 1.5_f64.min(1600.0 / width.max(height));
            let w = (width * scale).ceil().max(1.0) as usize;
            let h = (height * scale).ceil().max(1.0) as usize;
            let bitmap = page
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_fixed_size(w as i32, h as i32)
                        .set_format(PdfBitmapFormat::BGR)
                        .set_reverse_byte_order(true)
                        .render_annotations(true),
                )
                .map_err(|e| document_error(format!("page {number}: {e}")))?;
            let bytes = bitmap.as_raw_bytes();
            let stride = bytes.len() / h;
            if stride < w * 3 || bytes.len() % h != 0 {
                return Err(document_error("invalid RGB raster stride"));
            }
            let mut rgb = Vec::with_capacity(w * h * 3);
            for row in bytes.chunks_exact(stride) {
                rgb.extend_from_slice(&row[..w * 3]);
            }
            Some(LayoutImage {
                width: w,
                height: h,
                rgb: rgb.into(),
            })
        };
        if sender
            .blocking_send(RenderedPage {
                number,
                width,
                height,
                items,
                image,
            })
            .is_err()
        {
            return Err(EngineError::cancelled("PDF extraction consumer closed").into());
        }
    }
    Ok(bookmarks)
}
