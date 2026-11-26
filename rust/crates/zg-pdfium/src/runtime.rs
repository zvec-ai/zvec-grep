use std::{path::PathBuf, sync::OnceLock};

/// Load the packaged `PDFium` and acquire its process-wide lock.
///
/// # Errors
/// Returns a missing-library, loader, or initialization error.
pub fn acquire() -> Result<liteparse_pdfium::Library, String> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let path = runtime_path(
                std::env::var_os("PDFIUM_LIB_PATH").map(PathBuf::from),
                &std::env::current_exe().map_err(|e| e.to_string())?,
            )?;
            liteparse_pdfium_sys::dynamic::load(&path)
        })
        .as_ref()
        .map_err(Clone::clone)?;
    liteparse_pdfium::Library::try_init().map_err(|e| e.to_string())
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

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
