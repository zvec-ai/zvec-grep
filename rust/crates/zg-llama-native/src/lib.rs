//! Selective CPU plugin loading, without ggml's all-backend directory scan or
//! `GGML_BACKEND_PATH` override, plus an optional CUDA driver preflight used only
//! for GPU requests. Registry access is coordinated by the engine with its
//! model/context initialization; plugins stay registered for process life.

use std::{
    ffi::CString,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

mod cuda;
pub use cuda::{check_cuda12_driver, check_cuda13_driver};

static REGISTRY: Mutex<()> = Mutex::new(());

/// Serializes registration with the engine's registry-dependent native calls.
pub struct RegistryGuard {
    _guard: MutexGuard<'static, ()>,
}

/// Hold this guard through plugin registration and model/context initialization.
#[must_use]
pub fn lock_registry() -> RegistryGuard {
    RegistryGuard {
        _guard: REGISTRY.lock().unwrap_or_else(PoisonError::into_inner),
    }
}

impl RegistryGuard {
    /// Load only the best supported CPU plugin from a trusted native bundle.
    ///
    /// # Errors
    /// Returns an error if the directory has no usable CPU plugin or its path cannot
    /// be passed to ggml. CUDA and other GPU plugins are never opened by this loader.
    pub fn load_cpu_backend(&self, directory: &Path) -> Result<(), String> {
        let path = select_cpu_backend(directory, initialize_core, cpu_score)?;
        let path_string = path
            .to_str()
            .ok_or("llama.cpp backend path must be UTF-8")?;
        let path_c = CString::new(path_string).map_err(|error| error.to_string())?;
        // SAFETY: the NUL-terminated path lives through the call. ggml owns the
        // registered library afterwards; it is never unloaded here. The caller
        // serializes this with all registry-dependent llama model/context creation.
        let backend = unsafe { llama_cpp_sys_2::ggml_backend_load(path_c.as_ptr()) };
        if backend.is_null() {
            return Err(format!(
                "Unable to load llama.cpp CPU backend {}",
                path.display()
            ));
        }
        Ok(())
    }
}

fn initialize_core() {
    // Resolve our ggml-base import BEFORE temporarily loading CPU variants.
    // Windows zg delay-loads this DLL: without an executable-owned reference,
    // dropping a scoring library can unload ggml-base while its process-wide
    // std::terminate handler remains installed. Reloading then asserts.
    // The delay-load import holds the core for process life, independently of
    // the temporary scoring handles. Do not initialize the backend registry or
    // call the all-backend loader here: explicit CPU must not probe any GPU.
    // SAFETY: the pinned ggml API exposes this idempotent timer initializer.
    // It neither enumerates devices nor loads/registers backend plugins.
    unsafe { llama_cpp_sys_2::ggml_time_init() };
}

fn cpu_score(path: &Path) -> Option<i32> {
    // SAFETY: only CPU-named plugins from the caller's trusted native bundle are
    // opened. The pinned ggml ABI defines ggml_backend_score as int(void).
    // Keep the library alive until the score function has returned.
    unsafe {
        let library = libloading::Library::new(path).ok()?;
        let score = library
            .get::<unsafe extern "C" fn() -> i32>(b"ggml_backend_score\0")
            .ok()?;
        Some(score())
    }
}

fn select_cpu_backend(
    directory: &Path,
    initialize_core: impl FnOnce(),
    mut score: impl FnMut(&Path) -> Option<i32>,
) -> Result<PathBuf, String> {
    let (prefix, extension) = if cfg!(target_os = "windows") {
        ("ggml-cpu", ".dll")
    } else {
        ("libggml-cpu", ".so")
    };
    let mut variants = std::fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "Unable to read CPU backend directory {}: {error}",
                directory.display()
            )
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&format!("{prefix}-")) && name.ends_with(extension)
                    })
        })
        .collect::<Vec<_>>();
    variants.sort();
    // Match ggml's CPU variant scoring: highest positive score, then the base
    // CPU plugin if no variant is supported. Never score a GPU plugin.
    let mut best = None;
    let mut best_score = 0;
    initialize_core();
    for path in variants {
        if let Some(value) = score(&path)
            && value > best_score
        {
            best_score = value;
            best = Some(path);
        }
    }
    best.or_else(|| {
        let base = directory.join(format!("{prefix}{extension}"));
        base.is_file().then_some(base)
    })
    .ok_or_else(|| {
        format!(
            "No supported llama.cpp CPU backend in {}",
            directory.display()
        )
    })
}

#[cfg(test)]
mod tests;
