use std::path::Path;

#[cfg(feature = "cuda")]
pub(super) fn load(directory: &Path) {
    let selected = load_with_fallback(directory, |path| {
        llama_cpp_2::llama_backend::load_backends_from_path(path);
        llama_cpp_2::list_llama_ggml_backend_devices()
            .iter()
            .any(|device| device.backend.eq_ignore_ascii_case("CUDA"))
    });
    if let Some(cuda_version) = selected {
        tracing::debug!(cuda_version, "llama.cpp CUDA backend library selected");
    }
}

// Both plugins are built from the same locked llama.cpp source. Register only
// one usable CUDA implementation; a loadable DLL with zero devices is not usable.
fn load_with_fallback(
    directory: &Path,
    mut load: impl FnMut(&Path) -> bool,
) -> Option<&'static str> {
    if load(directory) {
        return Some("13.1");
    }
    let fallback = directory.join("cuda12");
    if fallback.is_dir() && load(&fallback) {
        return Some("12.4");
    }
    None
}

#[cfg(test)]
#[path = "backend_loading_tests.rs"]
mod tests;
