//! Safe raster allocation and packaged-library loading for `LiteParse`'s `PDFium`.

use liteparse_pdfium::{Bitmap, Page, PdfiumError};
use std::path::Path;

/// Load the same patched `PDFium` used for native extraction.
///
/// # Errors
/// Returns the loader's error when the library or a required symbol is missing.
pub fn load(path: &Path) -> Result<(), String> {
    liteparse_pdfium_sys::dynamic::load(path)
}

/// Render a page with annotations into packed RGB pixels for layout inference.
///
/// # Errors
/// Returns an error on invalid dimensions or failed bitmap allocation.
pub fn render_rgb(page: &Page<'_, '_>, width: i32, height: i32) -> Result<Vec<u8>, PdfiumError> {
    if width <= 0 || height <= 0 || width > 1600 || height > 1600 {
        return Err(PdfiumError::OperationFailed);
    }
    // SAFETY: Page borrows a live Library holding PDFium's process-wide lock.
    // The bitmap is used and dropped here, before that page/lock can be dropped.
    let bitmap = unsafe {
        Bitmap::new_with_format(
            width,
            height,
            liteparse_pdfium_sys::FPDFBitmap_BGR.cast_signed(),
        )?
    };
    bitmap.fill_rect(0, 0, width, height, 0xffff_ffff);
    page.render_into(
        &bitmap,
        0,
        0,
        width,
        height,
        None,
        (liteparse_pdfium_sys::FPDF_ANNOT | liteparse_pdfium_sys::FPDF_REVERSE_BYTE_ORDER)
            .cast_signed(),
    );
    let stride = usize::try_from(bitmap.stride()).map_err(|_| PdfiumError::OperationFailed)?;
    let width = usize::try_from(width).map_err(|_| PdfiumError::OperationFailed)?;
    let height = usize::try_from(height).map_err(|_| PdfiumError::OperationFailed)?;
    if stride < width * 3 || bitmap.buffer().len() != stride * height {
        return Err(PdfiumError::OperationFailed);
    }
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in bitmap.buffer().chunks_exact(stride) {
        rgb.extend_from_slice(&row[..width * 3]);
    }
    Ok(rgb)
}
