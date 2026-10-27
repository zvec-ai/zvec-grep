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
