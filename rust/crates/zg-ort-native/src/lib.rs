//! Loader boundary for the ONNX CUDA provider's process lifetime.

/// Retain one process-lifetime reference to an already loaded CUDA provider.
///
/// Returns false if no reference could be obtained. Never loads a new module:
/// only ORT may load CUDA, after initializing its shared provider bridge. The
/// caller must serialize attempts and stop after success to bound references.
#[must_use]
pub fn retain_loaded_cuda_provider() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the name is NUL-terminated; NOLOAD cannot run a new module's
        // constructors. Lookup uses the loaded object's name/SONAME, independent
        // of the core ORT library's directory. Intentionally never dlclose this
        // reference: ReleaseEnv must run before the provider's static destructors.
        let handle = unsafe {
            libc::dlopen(
                c"libonnxruntime_providers_cuda.so".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_NOLOAD,
            )
        };
        !handle.is_null()
    }
    #[cfg(target_os = "windows")]
    {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the UTF-16 name and output pointer are valid for this call.
        // Flags=0 increments the reference count of an existing module only;
        // never FreeLibrary this reference (the same lifetime as on Linux).
        unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleExW(
                0,
                windows_sys::w!("onnxruntime_providers_cuda.dll"),
                &raw mut handle,
            ) != 0
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        false
    }
}
