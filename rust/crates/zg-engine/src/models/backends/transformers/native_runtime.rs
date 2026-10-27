use std::path::PathBuf;
use std::{
    env,
    sync::{Mutex, OnceLock, PoisonError},
};

use ort::environment::Environment;

use crate::models::spi::ModelError;

// ORT permits one environment per process. Keep it across model eviction/reload,
// then release it before the C++ globals initialized by loading the library.
static RUNTIME: OnceLock<Result<(), String>> = OnceLock::new();
static ENVIRONMENT: Mutex<Option<Environment>> = Mutex::new(None);

extern "C" fn release_environment() {
    // Do not log or panic from a C exit callback. Sessions keep their own clones.
    let environment = ENVIRONMENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    drop(environment);
}

pub(super) fn initialize() -> Result<Environment, ModelError> {
    RUNTIME.get_or_init(|| {
        let path = runtime_path(
            env::var_os("ORT_DYLIB_PATH").map(PathBuf::from),
            env::var_os("ZVEC_GREP_NATIVE_LIB_DIR").map(PathBuf::from),
            env::current_exe().ok(),
        )?;
        #[cfg(target_os = "windows")]
        preload_directml(&path)?;
        let environment = ort::init_from(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .build()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        *ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(environment);
        // Register after dlopen/CreateEnv, not in a static .fini_array: callbacks
        // run in reverse registration order, ahead of ORT's C++ destructors.
        if !shutdown_hooks::add_shutdown_hook(release_environment) {
            release_environment();
            return Err("Unable to register ONNX Runtime shutdown".to_owned());
        }
        tracing::debug!(path = %path.display(), "Packaged ONNX Runtime initialized");
        Ok(())
    }).as_ref().map_err(|error| {
        ModelError::internal("Unable to load packaged ONNX Runtime; keep it under ../lib relative to zg or set ORT_DYLIB_PATH")
            .with_cause(error)
    })?;
    ENVIRONMENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .ok_or_else(|| ModelError::internal("ONNX Runtime has shut down"))
}

#[cfg(target_os = "windows")]
fn preload_directml(path: &std::path::Path) -> Result<(), String> {
    // Loading an absolute ORT path does not add its parent to dependent-DLL
    // lookup. Select its own DirectML before Windows can resolve another copy
    // beside the executable, also when zg-engine is embedded without the CLI.
    let resolved = if path.is_relative() {
        env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join(path)))
            .filter(|candidate| candidate.exists())
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let directml = resolved.with_file_name("DirectML.dll");
    if directml.is_file() {
        ort::util::preload_dylib(&directml)
            .map_err(|error| format!("{}: {error}", directml.display()))?;
    }
    Ok(())
}

fn runtime_path(
    explicit: Option<PathBuf>,
    directory: Option<PathBuf>,
    executable: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let filename = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.1.dylib"
    } else {
        "libonnxruntime.so.1"
    };
    if let Some(directory) = directory {
        return Ok(directory.join(filename));
    }
    let executable =
        executable.ok_or_else(|| "Unable to locate the packaged ONNX Runtime".to_owned())?;
    let adjacent = executable
        .parent()
        .ok_or_else(|| "Unable to locate the packaged ONNX Runtime".to_owned())?;
    Ok(adjacent
        .parent()
        .map(|root| root.join("lib").join(filename))
        .filter(|packaged| packaged.is_file())
        .unwrap_or_else(|| adjacent.join(filename)))
}

#[cfg(test)]
#[path = "native_runtime_tests.rs"]
mod tests;
