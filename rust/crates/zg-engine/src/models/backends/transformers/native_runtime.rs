use std::path::PathBuf;
use std::{
    env,
    sync::{Mutex, OnceLock, PoisonError},
};

use ort::environment::{Environment, GlobalThreadPoolOptions};

use crate::models::runtime::ort_config;
use crate::models::spi::ModelError;

// ORT permits one environment per process. Keep it across model eviction/reload,
// then release it before the C++ globals initialized by loading the library.
static RUNTIME: OnceLock<Result<PathBuf, String>> = OnceLock::new();
static ENVIRONMENT: Mutex<Option<Environment>> = Mutex::new(None);
#[cfg(feature = "cuda")]
static CUDA_SHUTDOWN_REGISTERED: Mutex<bool> = Mutex::new(false);

extern "C" fn release_environment() {
    // Do not log or panic from a C exit callback. Sessions keep their own clones.
    let environment = ENVIRONMENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    drop(environment);
}

pub(super) fn initialize() -> Result<Environment, ModelError> {
    initialize_runtime()?;
    ENVIRONMENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .ok_or_else(|| ModelError::internal("ONNX Runtime has shut down"))
}

fn initialize_runtime() -> Result<&'static PathBuf, ModelError> {
    RUNTIME.get_or_init(|| {
        let path = runtime_path(
            env::var_os("ORT_DYLIB_PATH").map(PathBuf::from),
            env::var_os("ZVEC_GREP_NATIVE_LIB_DIR").map(PathBuf::from),
            env::current_exe().ok(),
        )?;
        let path = resolve_library_path(path);
        #[cfg(target_os = "windows")]
        preload_directml(&path)?;
        // Load the explicitly selected library before calling any ORT APIs,
        // including construction of the threading options themselves.
        let builder = ort::init_from(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let threads = GlobalThreadPoolOptions::default()
            .with_intra_threads(ort_config::thread_budget())
            .and_then(|options| options.with_inter_threads(1))
            .map_err(|error| format!("Unable to configure global ORT thread pool: {error}"))?;
        let environment = builder
            .with_global_thread_pool(threads)
            .build()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        *ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(environment);
        // This covers the core library. CUDA is loaded lazily and needs a later
        // callback, registered after native provider registration is attempted.
        if !shutdown_hooks::add_shutdown_hook(release_environment) {
            release_environment();
            return Err("Unable to register ONNX Runtime shutdown".to_owned());
        }
        tracing::debug!(path = %path.display(), "Packaged ONNX Runtime initialized");
        Ok(path)
    }).as_ref().map_err(|error| {
        ModelError::internal("Unable to load packaged ONNX Runtime; keep it under ../lib relative to zg or set ORT_DYLIB_PATH")
            .with_cause(error)
    })
}

#[cfg(feature = "cuda")]
pub(super) fn retain_cuda_for_shutdown() -> Result<(), ModelError> {
    let runtime = initialize_runtime()?;
    let mut loaded = CUDA_SHUTDOWN_REGISTERED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if *loaded {
        return Ok(());
    }
    let filename = if cfg!(target_os = "windows") {
        "onnxruntime_providers_cuda.dll"
    } else {
        "libonnxruntime_providers_cuda.so"
    };
    let provider = runtime.with_file_name(filename);
    // ORT has now initialized its shared bridge and attempted to load CUDA.
    // Retain the provider, including after a partially failed registration.
    // Loading it registers its C++ global destructors. Register our
    // callback afterwards so ReleaseEnv/Shutdown runs first (atexit is LIFO),
    // including when provider registration or model loading subsequently fails.
    // Keep one loader reference per process, like the core ORT/DirectML handles.
    // Failed loads remain retryable and explicit CPU never loads this library.
    ort::util::preload_dylib(&provider).map_err(|error| {
        ModelError::internal("Unable to load ONNX CUDA provider")
            .with_cause(format!("{}: {error}", provider.display()))
    })?;
    if !shutdown_hooks::add_shutdown_hook(release_environment) {
        release_environment();
        return Err(ModelError::internal(
            "Unable to register ONNX CUDA shutdown",
        ));
    }
    *loaded = true;
    Ok(())
}

fn resolve_library_path(path: PathBuf) -> PathBuf {
    // Match ort::init_from's executable-relative lookup before selecting any
    // sibling provider. Retain its cwd/system-loader fallback for overrides.
    if path.is_relative() {
        env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.join(&path)))
            .filter(|candidate| candidate.exists())
            .unwrap_or(path)
    } else {
        path
    }
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
    // macOS current_exe() may retain npm's global bin/zg symlink. Resolve
    // it before locating the package's sibling lib directory.
    let executable = executable
        .ok_or_else(|| "Unable to locate the packaged ONNX Runtime".to_owned())?
        .canonicalize()
        .map_err(|error| format!("Unable to resolve the packaged executable: {error}"))?;
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
