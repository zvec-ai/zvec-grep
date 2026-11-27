//! Safe raster allocation and packaged-library loading for `LiteParse`'s `PDFium`.

use liteparse_pdfium::{Bitmap, Page, PdfiumError};
mod runtime;
pub use runtime::acquire;

/// Packed RGB raster with its actual, bounded dimensions.
pub struct RenderedRgb {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

/// Render a page with annotations into packed RGB pixels for layout inference.
///
/// # Errors
/// Returns an error on invalid dimensions or failed bitmap allocation.
pub fn render_rgb(page: &Page<'_, '_>, scale: f64) -> Result<RenderedRgb, PdfiumError> {
    let (width, height) = dimensions(f64::from(page.width()), f64::from(page.height()), scale)?;
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
    Ok(RenderedRgb { width, height, rgb })
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "Positive finite dimensions are capped before conversion"
)]
fn dimensions(width: f64, height: f64, scale: f64) -> Result<(i32, i32), PdfiumError> {
    if ![width, height, scale]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
    {
        return Err(PdfiumError::OperationFailed);
    }
    let scale = scale.min(1600.0 / width.max(height));
    Ok((
        (width * scale).ceil().clamp(1.0, 1600.0) as i32,
        (height * scale).ceil().clamp(1.0, 1600.0) as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raster_dimensions_preserve_scale_and_bound_allocation() {
        assert_eq!(
            dimensions(612.0, 792.0, 1.5).expect("valid test fixture"),
            (918, 1188)
        );
        assert_eq!(
            dimensions(2400.0, 1200.0, 1.5).expect("valid test fixture"),
            (1600, 800)
        );
        assert_eq!(
            dimensions(0.1, 0.1, 1.5).expect("valid test fixture"),
            (1, 1)
        );
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(dimensions(invalid, 792.0, 1.5).is_err());
            assert!(dimensions(612.0, invalid, 1.5).is_err());
            assert!(dimensions(612.0, 792.0, invalid).is_err());
        }
    }
}
