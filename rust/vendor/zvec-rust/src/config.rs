use std::ffi::CStr;
use std::path::{Path, PathBuf};

use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};
use crate::types::LogLevel;

/// Configuration builder for initializing the zvec library.
///
/// This struct collects configuration values without touching the C library,
/// making it safe to use even before the library is initialized.
///
/// # Example
/// ```no_run
/// use zvec_rust::ConfigBuilder;
///
/// let config = ConfigBuilder::new()
///     .memory_limit(1024 * 1024 * 1024)
///     .num_threads(4)
///     .enable_console_log(true)
///     .build();
/// ```
pub struct ConfigBuilder {
    /// Memory limit in bytes (0 = use library default).
    pub memory_limit: u64,
    /// Number of threads for query and optimize (0 = use library default).
    pub num_threads: u32,
    /// Whether to enable console logging at Info level.
    pub enable_console_log: bool,
    /// FTS brute-force-by-keys ratio (None = use library default).
    pub fts_brute_force_by_keys_ratio: Option<f32>,
    /// Jieba dict directory for the FTS `jieba` tokenizer
    /// (None = auto-discover and register the process-wide default).
    pub jieba_dict_dir: Option<String>,
}

impl ConfigBuilder {
    /// Creates a new builder with default values.
    pub fn new() -> Self {
        ConfigBuilder {
            memory_limit: 0,
            num_threads: 0,
            enable_console_log: false,
            fts_brute_force_by_keys_ratio: None,
            jieba_dict_dir: None,
        }
    }

    /// Sets the memory limit in bytes.
    pub fn memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = bytes;
        self
    }

    /// Sets the number of threads for both query and optimize.
    pub fn num_threads(mut self, count: u32) -> Self {
        self.num_threads = count;
        self
    }

    /// Enables or disables console logging at Info level.
    pub fn enable_console_log(mut self, enable: bool) -> Self {
        self.enable_console_log = enable;
        self
    }

    /// Sets the FTS brute-force-by-keys ratio.
    pub fn fts_brute_force_by_keys_ratio(mut self, ratio: f32) -> Self {
        self.fts_brute_force_by_keys_ratio = Some(ratio);
        self
    }

    /// Sets the jieba dict directory for the FTS `jieba` tokenizer.
    ///
    /// The directory must contain `jieba.dict.utf8` and `hmm_model.utf8`
    /// (shipped with the zvec prebuilt packages under `data/jieba_dict`).
    /// When unset, [`initialize`] auto-discovers the dict directory that
    /// ships with the zvec library and registers it as the process-wide
    /// default via [`set_default_jieba_dict_dir`].
    pub fn jieba_dict_dir(mut self, dir: impl Into<String>) -> Self {
        self.jieba_dict_dir = Some(dir.into());
        self
    }

    /// Finalizes the builder configuration.
    ///
    /// This is a no-op that returns `self` for API consistency. The builder
    /// is a plain-data struct — no C resources are allocated here.
    /// To apply the configuration, pass the result to [`initialize`].
    pub fn build(self) -> Self {
        self
    }
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Low-level configuration handle wrapping the C API config.
pub(crate) struct ConfigData {
    pub(crate) handle: *mut zvec_rust_sys::zvec_config_data_t,
}

impl ConfigData {
    pub(crate) fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_config_data_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create config data".into(),
            });
        }
        Ok(ConfigData { handle })
    }

    pub(crate) fn set_memory_limit(&mut self, bytes: u64) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_config_data_set_memory_limit(self.handle, bytes) })
    }

    pub(crate) fn set_query_thread_count(&mut self, count: u32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_config_data_set_query_thread_count(self.handle, count)
        })
    }

    pub(crate) fn set_optimize_thread_count(&mut self, count: u32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_config_data_set_optimize_thread_count(self.handle, count)
        })
    }

    pub(crate) fn set_fts_brute_force_by_keys_ratio(&mut self, ratio: f32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_config_data_set_fts_brute_force_by_keys_ratio(self.handle, ratio)
        })
    }

    pub(crate) fn set_jieba_dict_dir(&mut self, dir: &str) -> Result<()> {
        let c_dir = to_cstring(dir)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_config_data_set_jieba_dict_dir(self.handle, c_dir.as_ptr())
        })
    }

    pub(crate) fn set_console_log(&mut self, level: LogLevel) -> Result<()> {
        let log_config = unsafe { zvec_rust_sys::zvec_config_log_create_console(level as u32) };
        if log_config.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create console log config".into(),
            });
        }
        // Ownership of log_config transfers to config_data on success.
        // On failure, we must free it manually to avoid a leak.
        let result = check_error(unsafe {
            zvec_rust_sys::zvec_config_data_set_log_config(self.handle, log_config)
        });
        if result.is_err() {
            unsafe { zvec_rust_sys::zvec_config_log_destroy(log_config) };
        }
        result
    }
}

impl Drop for ConfigData {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // Safety: handle was created by zvec_config_data_create
            unsafe { zvec_rust_sys::zvec_config_data_destroy(self.handle) };
        }
    }
}

/// Initializes the zvec library with optional configuration.
///
/// Pass `None` to use default configuration, or provide a [`ConfigBuilder`]
/// to customize memory limits, thread counts, and logging.
///
/// # Examples
///
/// ```no_run
/// use zvec_rust::*;
///
/// // Default initialization
/// initialize(None)?;
///
/// // With builder
/// let config = ConfigBuilder::new()
///     .memory_limit(1024 * 1024 * 1024)
///     .num_threads(4)
///     .build();
/// initialize(Some(&config))?;
/// # Ok::<(), zvec_rust::Error>(())
/// ```
pub fn initialize(config: Option<&ConfigBuilder>) -> Result<()> {
    match config {
        None => {
            auto_register_default_jieba_dict_dir();
            check_error(unsafe { zvec_rust_sys::zvec_initialize(std::ptr::null()) })
        }
        Some(builder) => {
            let mut cfg = ConfigData::new()?;
            if builder.memory_limit > 0 {
                cfg.set_memory_limit(builder.memory_limit)?;
            }
            if builder.num_threads > 0 {
                cfg.set_query_thread_count(builder.num_threads)?;
                cfg.set_optimize_thread_count(builder.num_threads)?;
            }
            if builder.enable_console_log {
                cfg.set_console_log(LogLevel::Info)?;
            }
            if let Some(ratio) = builder.fts_brute_force_by_keys_ratio {
                cfg.set_fts_brute_force_by_keys_ratio(ratio)?;
            }
            if let Some(ref dir) = builder.jieba_dict_dir {
                cfg.set_jieba_dict_dir(dir)?;
            } else {
                auto_register_default_jieba_dict_dir();
            }
            check_error(unsafe { zvec_rust_sys::zvec_initialize(cfg.handle as *const _) })
        }
    }
}

/// Sets the process-wide default jieba dict directory.
///
/// The directory must contain `jieba.dict.utf8` and `hmm_model.utf8` (as
/// shipped with the zvec prebuilt packages under `data/jieba_dict`). The FTS
/// `jieba` tokenizer uses this default when neither the per-field
/// `jieba_dict_dir` extra param nor the `ZVEC_JIEBA_DICT_DIR` environment
/// variable is set. Passing an empty string clears the default.
///
/// Thread-safe; last writer wins. A subsequent [`initialize`] with a
/// non-empty [`ConfigBuilder::jieba_dict_dir`] overrides this default.
pub fn set_default_jieba_dict_dir(dir: &str) -> Result<()> {
    let c_dir = to_cstring(dir)?;
    unsafe { zvec_rust_sys::zvec_set_default_jieba_dict_dir(c_dir.as_ptr()) };
    Ok(())
}

/// Returns the process-wide default jieba dict directory, or `""` if unset.
pub fn get_default_jieba_dict_dir() -> String {
    unsafe {
        let ptr = zvec_rust_sys::zvec_get_default_jieba_dict_dir();
        if ptr.is_null() {
            return String::new();
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// Registers an auto-discovered jieba dict directory as the process-wide
/// default, unless one is already registered.
///
/// Called automatically by [`initialize`] so the `jieba` FTS tokenizer works
/// out of the box with the bundled prebuilt libraries.
fn auto_register_default_jieba_dict_dir() {
    if !get_default_jieba_dict_dir().is_empty() {
        return;
    }
    if let Some(dir) = discover_jieba_dict_dir() {
        let dir = dir.to_string_lossy().into_owned();
        let _ = set_default_jieba_dict_dir(&dir);
    }
}

/// Searches known locations for a directory containing `jieba.dict.utf8`
/// and `hmm_model.utf8`, returning the first hit.
fn discover_jieba_dict_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Path recorded at build time by zvec-rust-sys (prebuilt download
    //    cache, sibling/submodule checkout, or ZVEC_LIB_DIR).
    if let Some(build_time_dir) = zvec_rust_sys::build_time_jieba_dict_dir() {
        candidates.push(PathBuf::from(build_time_dir));
    }

    // 2. Relative to the loaded libzvec_c_api: prebuilt packages ship the
    //    dict at `<lib_dir>/data/jieba_dict`, native SDK install prefixes at
    //    `<prefix>/data/jieba_dict` next to `<prefix>/lib`.
    if let Some(lib_dir) = loaded_library_dir() {
        candidates.push(lib_dir.join("data").join("jieba_dict"));
        candidates.push(lib_dir.join("..").join("data").join("jieba_dict"));
    }

    candidates.into_iter().find(|dir| is_jieba_dict_dir(dir))
}

fn is_jieba_dict_dir(dir: &Path) -> bool {
    dir.join("jieba.dict.utf8").is_file() && dir.join("hmm_model.utf8").is_file()
}

/// Directory of the loaded `libzvec_c_api` shared library, if determinable.
#[cfg(unix)]
fn loaded_library_dir() -> Option<PathBuf> {
    #[repr(C)]
    struct DlInfo {
        dli_fname: *const std::ffi::c_char,
        dli_fbase: *mut std::ffi::c_void,
        dli_sname: *const std::ffi::c_char,
        dli_saddr: *mut std::ffi::c_void,
    }
    extern "C" {
        fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> std::ffi::c_int;
    }

    // Probe with a symbol that was definitely called already: with lazy
    // binding the GOT slot only holds the resolved in-library address after
    // the first call; an uncalled symbol could still point at the PLT stub
    // inside the executable.
    let symbol =
        zvec_rust_sys::zvec_get_default_jieba_dict_dir as *const () as *const std::ffi::c_void;

    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    let path = unsafe {
        if dladdr(symbol, &mut info) == 0 || info.dli_fname.is_null() {
            return None;
        }
        PathBuf::from(CStr::from_ptr(info.dli_fname).to_string_lossy().as_ref())
    };
    let resolved = std::fs::canonicalize(&path).unwrap_or(path);
    resolved.parent().map(PathBuf::from)
}

/// Directory of the loaded `zvec_c_api.dll`, if determinable.
#[cfg(windows)]
fn loaded_library_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    type HModule = *mut std::ffi::c_void;
    const FROM_ADDRESS: u32 = 0x0000_0004;
    const UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleExW(
            flags: u32,
            module_name: *mut std::ffi::c_void,
            module: *mut HModule,
        ) -> i32;
        fn GetModuleFileNameW(module: HModule, filename: *mut u16, size: u32) -> u32;
    }

    let symbol =
        zvec_rust_sys::zvec_get_default_jieba_dict_dir as *const () as *mut std::ffi::c_void;
    unsafe {
        let mut module: HModule = std::ptr::null_mut();
        if GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, symbol, &mut module) == 0
            || module.is_null()
        {
            return None;
        }
        let mut size = 260u32; // MAX_PATH
        loop {
            let mut buf = vec![0u16; size as usize];
            let len = GetModuleFileNameW(module, buf.as_mut_ptr(), size);
            if len == 0 {
                return None;
            }
            if len < size {
                buf.truncate(len as usize);
                return PathBuf::from(std::ffi::OsString::from_wide(&buf))
                    .parent()
                    .map(PathBuf::from);
            }
            size = size.checked_mul(2)?; // buffer too small; grow and retry
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn loaded_library_dir() -> Option<PathBuf> {
    None
}

/// Shuts down the zvec library and releases all resources.
#[doc(hidden)]
pub fn shutdown() -> Result<()> {
    check_error(unsafe { zvec_rust_sys::zvec_shutdown() })
}

/// Returns `true` if the library has been initialized.
pub fn is_initialized() -> bool {
    unsafe { zvec_rust_sys::zvec_is_initialized() }
}

/// Returns the library version string.
pub fn version() -> String {
    unsafe {
        let ptr = zvec_rust_sys::zvec_get_version();
        if ptr.is_null() {
            return String::new();
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// Checks if the current library version meets the minimum requirements.
pub fn check_version(major: i32, minor: i32, patch: i32) -> bool {
    unsafe { zvec_rust_sys::zvec_check_version(major, minor, patch) }
}

/// Returns the major version number.
pub fn version_major() -> i32 {
    unsafe { zvec_rust_sys::zvec_get_version_major() }
}

/// Returns the minor version number.
pub fn version_minor() -> i32 {
    unsafe { zvec_rust_sys::zvec_get_version_minor() }
}

/// Returns the patch version number.
pub fn version_patch() -> i32 {
    unsafe { zvec_rust_sys::zvec_get_version_patch() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builder_defaults() {
        let builder = ConfigBuilder::new();
        assert_eq!(builder.memory_limit, 0);
        assert_eq!(builder.num_threads, 0);
        assert!(!builder.enable_console_log);
        assert!(builder.jieba_dict_dir.is_none());
    }

    #[test]
    fn config_builder_chaining() {
        let builder = ConfigBuilder::new()
            .memory_limit(1024)
            .num_threads(4)
            .enable_console_log(true)
            .build();
        assert_eq!(builder.memory_limit, 1024);
        assert_eq!(builder.num_threads, 4);
        assert!(builder.enable_console_log);
    }

    #[test]
    fn config_builder_default_trait() {
        let builder = ConfigBuilder::default();
        assert_eq!(builder.memory_limit, 0);
        assert_eq!(builder.num_threads, 0);
        assert!(!builder.enable_console_log);
    }

    #[test]
    fn config_builder_memory_limit_setter() {
        let builder = ConfigBuilder::new().memory_limit(2048);
        assert_eq!(builder.memory_limit, 2048);
    }

    #[test]
    fn config_builder_num_threads_setter() {
        let builder = ConfigBuilder::new().num_threads(8);
        assert_eq!(builder.num_threads, 8);
    }

    #[test]
    fn config_builder_enable_console_log_setter() {
        let builder = ConfigBuilder::new().enable_console_log(true);
        assert!(builder.enable_console_log);
    }

    #[test]
    fn config_builder_jieba_dict_dir_setter() {
        let builder = ConfigBuilder::new().jieba_dict_dir("/tmp/jieba_dict");
        assert_eq!(builder.jieba_dict_dir.as_deref(), Some("/tmp/jieba_dict"));
    }

    #[test]
    fn default_jieba_dict_dir_roundtrip() {
        let previous = get_default_jieba_dict_dir();
        set_default_jieba_dict_dir("/tmp/zvec_rust_jieba_roundtrip").unwrap();
        assert_eq!(
            get_default_jieba_dict_dir(),
            "/tmp/zvec_rust_jieba_roundtrip"
        );
        // Empty string clears; restore the previous value afterwards so the
        // process-wide default is left as found.
        set_default_jieba_dict_dir("").unwrap();
        assert_eq!(get_default_jieba_dict_dir(), "");
        set_default_jieba_dict_dir(&previous).unwrap();
        assert_eq!(get_default_jieba_dict_dir(), previous);
    }

    #[test]
    fn discovered_jieba_dict_dir_contains_dict_files() {
        // Discovery is environment-dependent (build-time path or dylib
        // location); when it finds something, it must be a valid dict dir.
        if let Some(dir) = discover_jieba_dict_dir() {
            assert!(is_jieba_dict_dir(&dir), "invalid dict dir: {:?}", dir);
        }
    }

    #[test]
    fn config_builder_build_returns_self() {
        let builder = ConfigBuilder::new()
            .memory_limit(4096)
            .num_threads(2)
            .enable_console_log(true)
            .build();
        assert_eq!(builder.memory_limit, 4096);
        assert_eq!(builder.num_threads, 2);
        assert!(builder.enable_console_log);
    }

    #[test]
    fn config_builder_overwrite_values() {
        let builder = ConfigBuilder::new()
            .memory_limit(1024)
            .memory_limit(2048)
            .num_threads(4)
            .num_threads(8)
            .enable_console_log(false)
            .enable_console_log(true);
        assert_eq!(builder.memory_limit, 2048);
        assert_eq!(builder.num_threads, 8);
        assert!(builder.enable_console_log);
    }

    #[test]
    fn config_builder_zero_values() {
        let builder = ConfigBuilder::new().memory_limit(0).num_threads(0);
        assert_eq!(builder.memory_limit, 0);
        assert_eq!(builder.num_threads, 0);
    }

    #[test]
    fn config_builder_large_memory_limit() {
        let builder = ConfigBuilder::new().memory_limit(u64::MAX);
        assert_eq!(builder.memory_limit, u64::MAX);
    }

    #[test]
    fn config_builder_fts_ratio() {
        let builder = ConfigBuilder::new();
        assert_eq!(builder.fts_brute_force_by_keys_ratio, None);

        let builder = ConfigBuilder::new().fts_brute_force_by_keys_ratio(0.5);
        assert_eq!(builder.fts_brute_force_by_keys_ratio, Some(0.5));
    }

    #[test]
    fn loaded_library_dir_resolves_zvec_library() {
        // dladdr / GetModuleHandleEx based discovery of libzvec_c_api.
        if let Some(dir) = loaded_library_dir() {
            let dir_str = dir.to_string_lossy();
            assert!(
                dir_str.contains("zvec"),
                "unexpected dylib dir: {}",
                dir_str
            );
        }
    }
}
