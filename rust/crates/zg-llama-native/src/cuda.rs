//! Preflight for the distributed CUDA GGUF plugins, before ggml opens them.
//! This checks driver capability, not the installed Toolkit or inference success.

use std::{
    ffi::{OsStr, c_int},
    sync::OnceLock,
};

type Output = unsafe extern "system" fn(*mut c_int) -> c_int;
type Init = unsafe extern "system" fn(u32) -> c_int;
type Device = unsafe extern "system" fn(*mut c_int, c_int) -> c_int;
type Attribute = unsafe extern "system" fn(*mut c_int, c_int, c_int) -> c_int;

// Keep the driver loaded for process life. In particular, don't initialize it
// through a temporary Windows DLL handle and unload it before ggml uses it.
static DRIVER: OnceLock<Result<libloading::Library, String>> = OnceLock::new();

/// Check whether the driver satisfies the distributed CUDA 12.4 preflight.
///
/// `native_sm86` permits the distribution's SM86 cubin exception. Pass false for
/// custom plugins whose architecture contract is unknown. Only call on a GPU
/// request: this can load the driver and enumerate devices, but never loads a
/// GGML plugin, creates a CUDA context, or executes kernels.
///
/// # Errors
/// Rejects an unavailable/unknown driver, pre-CUDA-12 capability, or a driver
/// below CUDA 12.4 when any visible device requires PTX 8.4 JIT compilation.
pub fn check_cuda12_driver(native_sm86: bool) -> Result<(), String> {
    let (driver, version) = active_driver()?;
    check_capability(version, native_sm86, forced_ptx(), || capabilities(driver))
}

/// Check whether the driver/devices satisfy the CUDA 13.1 primary preflight.
///
/// `distribution_architectures` asserts the distribution's native SM75/80/86/89/90
/// plus compute90 PTX contract. Unknown custom targets require CUDA 13.1 driver
/// capability without a native-code exception. Like [`check_cuda12_driver`],
/// this must only run on GPU requests and never opens a GGML plugin.
///
/// # Errors
/// Rejects failed queries, insufficient driver capability, and known device/code
/// incompatibilities (including forced PTX on devices older than compute90).
pub fn check_cuda13_driver(distribution_architectures: bool) -> Result<(), String> {
    let (driver, version) = active_driver()?;
    check_cuda13_capability(version, distribution_architectures, forced_ptx(), || {
        capabilities(driver)
    })
}

fn active_driver() -> Result<(&'static libloading::Library, c_int), String> {
    let driver = DRIVER
        .get_or_init(|| {
            let name = match std::env::consts::OS {
                "windows" => "nvcuda.dll",
                "linux" => "libcuda.so.1",
                platform => return Err(format!("CUDA driver is unsupported on {platform}")),
            };
            // SAFETY: load the platform CUDA driver by its standard loader name,
            // honoring the active user-space driver (including cuda-compat).
            unsafe { libloading::Library::new(name) }
                .map_err(|error| format!("Unable to open CUDA driver: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let mut version = 0;
    // SAFETY: CUDA defines these stable ABIs as CUresult(unsigned int) and
    // CUresult(int*). The output pointer is valid through the call and DRIVER
    // retains the library for process life.
    unsafe {
        let init = driver
            .get::<Init>(b"cuInit\0")
            .map_err(|error| symbol_error(&error))?;
        succeeded("cuInit", init(0))?;
        let get_version = driver
            .get::<Output>(b"cuDriverGetVersion\0")
            .map_err(|error| symbol_error(&error))?;
        succeeded("cuDriverGetVersion", get_version(&raw mut version))?;
    }
    Ok((driver, version))
}

fn forced_ptx() -> bool {
    force_ptx_jit(
        std::env::var_os("CUDA_FORCE_PTX_JIT").as_deref(),
        std::env::var_os("CUDA_FORCE_JIT").as_deref(),
    )
}

fn force_ptx_jit(ptx: Option<&OsStr>, jit: Option<&OsStr>) -> bool {
    // NVIDIA specifies that CUDA_FORCE_PTX_JIT overrides CUDA_FORCE_JIT,
    // including an explicit zero that restores native-code selection.
    // Unknown values must not accidentally grant the native-code exception.
    ptx.or(jit).is_some_and(|value| value != "0")
}

fn symbol_error(error: &libloading::Error) -> String {
    format!("CUDA driver query is unavailable: {error}")
}

fn succeeded(operation: &str, code: c_int) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed with CUDA error {code}"))
    }
}

fn capabilities(driver: &libloading::Library) -> Result<Vec<(c_int, c_int)>, String> {
    // CUDA Driver API enum values from cuda.h, not cudaRuntimeGetVersion.
    const COMPUTE_CAPABILITY_MAJOR: c_int = 75;
    const COMPUTE_CAPABILITY_MINOR: c_int = 76;
    // SAFETY: these stable Driver API signatures use integer handles/attributes
    // and valid stack output pointers. All symbols remain scoped to DRIVER.
    unsafe {
        let count = driver
            .get::<Output>(b"cuDeviceGetCount\0")
            .map_err(|error| symbol_error(&error))?;
        let device = driver
            .get::<Device>(b"cuDeviceGet\0")
            .map_err(|error| symbol_error(&error))?;
        let attribute = driver
            .get::<Attribute>(b"cuDeviceGetAttribute\0")
            .map_err(|error| symbol_error(&error))?;
        let mut device_count = 0;
        succeeded("cuDeviceGetCount", count(&raw mut device_count))?;
        if device_count <= 0 {
            return Err("CUDA driver reports no visible devices".to_owned());
        }
        (0..device_count)
            .map(|ordinal| {
                let (mut handle, mut major, mut minor) = (0, 0, 0);
                succeeded("cuDeviceGet", device(&raw mut handle, ordinal))?;
                succeeded(
                    "cuDeviceGetAttribute(major)",
                    attribute(&raw mut major, COMPUTE_CAPABILITY_MAJOR, handle),
                )?;
                succeeded(
                    "cuDeviceGetAttribute(minor)",
                    attribute(&raw mut minor, COMPUTE_CAPABILITY_MINOR, handle),
                )?;
                Ok((major, minor))
            })
            .collect()
    }
}

fn check_capability(
    version: c_int,
    native_sm86: bool,
    force_ptx: bool,
    devices: impl FnOnce() -> Result<Vec<(c_int, c_int)>, String>,
) -> Result<(), String> {
    if version >= 12040 {
        return Ok(());
    }
    if version < 12000 {
        return Err(format!(
            "CUDA 12.4 fallback requires at least CUDA 12 driver capability (reported {version})"
        ));
    }
    if native_sm86 && !force_ptx {
        let devices = devices()?;
        // Same-major cubin compatibility: SM86 runs on SM86/89, NOT SM80/90.
        // ggml enumerates every visible device during registration. A mixed
        // set must not let the first compatible GPU hide a PTX-only GPU.
        if !devices.is_empty()
            && devices
                .iter()
                .all(|&(major, minor)| major == 8 && minor >= 6)
        {
            return Ok(());
        }
    }
    Err(format!(
        "CUDA 12.4 fallback may require PTX 8.4 JIT, but the driver reports {version}; \
         require CUDA driver capability >= 12040 or compatible native SM86 code on every visible GPU"
    ))
}

fn check_cuda13_capability(
    version: c_int,
    distribution_architectures: bool,
    force_ptx: bool,
    devices: impl FnOnce() -> Result<Vec<(c_int, c_int)>, String>,
) -> Result<(), String> {
    if version < 13000 {
        return Err(format!(
            "CUDA 13.1 primary requires at least CUDA 13 driver capability (reported {version})"
        ));
    }
    if !distribution_architectures {
        return if version >= 13010 {
            Ok(())
        } else {
            Err(format!(
                "CUDA 13.1 primary/custom targets may require PTX JIT; \
                 require CUDA driver capability >= 13010 (reported {version})"
            ))
        };
    }
    let devices = devices()?;
    if devices.is_empty() {
        return Err("CUDA driver reports no visible devices".to_owned());
    }
    for (major, minor) in devices {
        if major <= 0 || minor < 0 {
            return Err(format!("Invalid CUDA compute capability {major}.{minor}"));
        }
        // Same-major, same-or-newer-minor cubin compatibility. In particular,
        // SM90 cubins cannot execute on SM100; that path requires compute90 PTX.
        let native = matches!(major, 8 | 9) || (major == 7 && minor >= 5);
        if native && !force_ptx {
            continue;
        }
        if major < 9 {
            return Err(format!(
                "CUDA 13.1 primary has no usable code for SM{major}{minor} \
                 (forced PTX: {force_ptx}); its only PTX target is compute90"
            ));
        }
        if version < 13010 {
            return Err(format!(
                "CUDA 13.1 primary requires PTX JIT on SM{major}{minor} \
                 (forced PTX: {force_ptx}); require CUDA driver capability >= 13010 \
                 (reported {version})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "cuda_tests.rs"]
mod tests;
