use std::path::PathBuf;
#[cfg(feature = "cuda")]
use std::{env, sync::OnceLock};

#[cfg(feature = "cuda")]
use crate::models::spi::ModelError;

#[cfg(feature = "cuda")]
static RUNTIME: OnceLock<Result<(), String>> = OnceLock::new();

#[cfg(feature = "cuda")]
pub(super) fn initialize() -> Result<(), ModelError> {
    RUNTIME.get_or_init(|| {
        let path = runtime_path(
            env::var_os("ORT_DYLIB_PATH").map(PathBuf::from),
            env::var_os("ZVEC_GREP_NATIVE_LIB_DIR").map(PathBuf::from),
            env::current_exe().ok(),
        )?;
        ort::init_from(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .commit();
        tracing::debug!(path = %path.display(), cuda_version = "12", "Packaged ONNX Runtime initialized");
        Ok(())
    }).as_ref().map_err(|error| {
        ModelError::internal("Unable to load packaged ONNX Runtime; keep it under ../lib relative to zg or set ORT_DYLIB_PATH")
            .with_cause(error)
    }).copied()
}

fn runtime_path(
    explicit: Option<PathBuf>,
    directory: Option<PathBuf>,
    executable: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let directory = if let Some(directory) = directory {
        directory
    } else {
        let executable =
            executable.ok_or_else(|| "Unable to locate the packaged ONNX Runtime".to_owned())?;
        let adjacent = executable
            .parent()
            .ok_or_else(|| "Unable to locate the packaged ONNX Runtime".to_owned())?;
        adjacent
            .parent()
            .map(|root| root.join("lib"))
            .filter(|packaged| packaged.is_dir())
            .unwrap_or_else(|| adjacent.to_path_buf())
    };
    let filename = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.1.dylib"
    } else {
        "libonnxruntime.so.1"
    };
    Ok(directory.join(filename))
}

#[cfg(test)]
#[path = "native_runtime_tests.rs"]
mod tests;
