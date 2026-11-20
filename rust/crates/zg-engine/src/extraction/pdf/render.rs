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
use std::{collections::HashMap, path::PathBuf, sync::OnceLock};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub(super) struct RenderedPage {
    pub page: Page,
    pub tables: Vec<[f64; 4]>,
    pub image: Option<LayoutImage>,
}

fn runtime() -> Result<stages::Library, PdfExtractionError> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let path = runtime_path(
                std::env::var_os("PDFIUM_LIB_PATH").map(PathBuf::from),
                &std::env::current_exe().map_err(|e| e.to_string())?,
            )?;
            zg_pdfium::load(&path)
        })
        .as_ref()
        .map_err(|e| {
            PdfExtractionError::Runtime(EngineError::internal(format!(
                "Cannot load LiteParse PDFium: {e}"
            )))
        })?;
    stages::Library::try_init().map_err(|e| {
        PdfExtractionError::Runtime(EngineError::internal(format!(
            "Cannot initialize LiteParse PDFium: {e}"
        )))
    })
}

fn runtime_path(
    explicit: Option<PathBuf>,
    executable: &std::path::Path,
) -> Result<PathBuf, String> {
    let name = if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else if cfg!(windows) {
        "pdfium.dll"
    } else {
        "libpdfium.so"
    };
    if let Some(directory) = explicit {
        return Ok(directory.join(name));
    }
    // macOS can report npm's global bin/zg symlink. Locate libraries relative
    // to the real executable, as the ORT loader does, not the global prefix.
    let executable = executable.canonicalize().map_err(|e| e.to_string())?;
    let directory = executable.parent().ok_or("Executable has no parent")?;
    [
        directory.join(name),
        directory.join("../lib").join(name),
        directory.join("..").join(name),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .ok_or_else(|| format!("Missing packaged {name}"))
}

pub(super) fn document_error(e: impl std::fmt::Display) -> PdfExtractionError {
    EngineError::invalid_argument(format!("Cannot extract PDF: {e}")).into()
}

pub(super) fn produce(
    bytes: &[u8],
    sender: &Sender<RenderedPage>,
    signal: &CancellationToken,
) -> Result<Vec<OutlineTarget>, PdfExtractionError> {
    check(signal)?;
    let parser = assembly::parser();
    let library = runtime()?;
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
        let image = if page
            .text_items
            .iter()
            .any(|item| !item.text.trim().is_empty())
        {
            let native = document
                .page(i32::try_from(page.page_number - 1).map_err(document_error)?)
                .map_err(document_error)?;
            let (w, h) = dimensions(width, height);
            let rgb = zg_pdfium::render_rgb(&native, w, h).map_err(document_error)?;
            Some(LayoutImage {
                width: usize::try_from(w).map_err(document_error)?,
                height: usize::try_from(h).map_err(document_error)?,
                rgb: rgb.into(),
            })
        } else {
            None
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

#[expect(
    clippy::cast_possible_truncation,
    reason = "Positive finite page dimensions are clamped to 1600 pixels"
)]
fn dimensions(width: f64, height: f64) -> (i32, i32) {
    let scale = 1.5_f64.min(1600.0 / width.max(height));
    (
        (width * scale).ceil().clamp(1.0, 1600.0) as i32,
        (height * scale).ceil().clamp(1.0, 1600.0) as i32,
    )
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
