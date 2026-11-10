use super::*;

fn plugin(directory: &Path, backend: &str) -> PathBuf {
    let name = if cfg!(target_os = "windows") {
        format!("ggml-{backend}.dll")
    } else {
        format!("libggml-{backend}.so")
    };
    let path = directory.join(name);
    std::fs::write(&path, []).expect("plugin fixture");
    path
}

#[test]
fn selects_best_cpu_variant_without_opening_gpu_plugins() {
    let root = tempfile::tempdir().expect("bundle");
    let baseline = plugin(root.path(), "cpu");
    let generic = plugin(root.path(), "cpu-generic");
    let fast = plugin(root.path(), "cpu-fast");
    let unsupported = plugin(root.path(), "cpu-unsupported");
    plugin(root.path(), "cuda");
    plugin(root.path(), "vulkan");
    let mut opened = Vec::new();
    let selected = select_cpu_backend(
        root.path(),
        || {},
        |path| {
            opened.push(path.to_path_buf());
            Some(if path == fast {
                10
            } else {
                i32::from(path == generic)
            })
        },
    )
    .expect("CPU backend");
    assert_eq!(selected, fast);
    assert_eq!(opened.len(), 3);
    assert!(
        opened
            .iter()
            .all(|path| [fast.clone(), generic.clone(), unsupported.clone()].contains(path))
    );
    assert!(!opened.contains(&baseline));
}

#[test]
fn unsupported_or_unloadable_variants_fall_back_to_base_cpu() {
    let root = tempfile::tempdir().expect("bundle");
    let base = plugin(root.path(), "cpu");
    plugin(root.path(), "cpu-unsupported");
    assert_eq!(
        select_cpu_backend(root.path(), || {}, |_| Some(0)).expect("base"),
        base
    );
    assert_eq!(
        select_cpu_backend(root.path(), || {}, |_| None).expect("base"),
        base
    );
}

#[test]
fn missing_cpu_does_not_load_a_gpu_as_substitute() {
    let root = tempfile::tempdir().expect("bundle");
    plugin(root.path(), "cuda");
    assert!(select_cpu_backend(root.path(), || {}, |_| panic!("must not open GPU")).is_err());
}

#[test]
fn core_is_initialized_before_scoring_even_unloadable_or_unsupported_variants() {
    let root = tempfile::tempdir().expect("bundle");
    let base = plugin(root.path(), "cpu");
    let broken = plugin(root.path(), "cpu-broken");
    plugin(root.path(), "cpu-unsupported");
    plugin(root.path(), "cuda");
    let events = std::cell::RefCell::new(Vec::new());
    let selected = select_cpu_backend(
        root.path(),
        || events.borrow_mut().push("core"),
        |path| {
            assert_eq!(events.borrow().first(), Some(&"core"));
            events.borrow_mut().push("score");
            (path != broken).then_some(0)
        },
    )
    .expect("base CPU fallback");
    assert_eq!(selected, base);
    assert_eq!(*events.borrow(), ["core", "score", "score"]);
}

#[test]
fn failed_cpu_library_load_is_reported() {
    let root = tempfile::tempdir().expect("bundle");
    plugin(root.path(), "cpu");
    assert!(lock_registry().load_cpu_backend(root.path()).is_err());
}

#[test]
#[ignore = "requires dynamic-backends, ZVEC_GREP_NATIVE_LIB_DIR with real CPU plugins, and GGML_BACKEND_PATH pointing to a GPU plugin; run alone"]
fn real_cpu_plugin_ignores_gpu_override() {
    let directory = std::env::var_os("ZVEC_GREP_NATIVE_LIB_DIR").expect("native bundle");
    let gpu = std::env::var_os("GGML_BACKEND_PATH").expect("GPU override");
    assert!(
        Path::new(&gpu).is_file(),
        "GPU plugin must actually be present"
    );
    let registry = lock_registry();
    registry
        .load_cpu_backend(Path::new(&directory))
        .expect("load real CPU plugin");
    // SAFETY: this isolated test holds the registry guard throughout. A CPU
    // plugin has just been registered and its name is a static C string. Core
    // initialization must not auto-load the GPU override after this registration.
    unsafe {
        assert_eq!(llama_cpp_sys_2::ggml_backend_reg_count(), 1);
        let backend = llama_cpp_sys_2::ggml_backend_reg_get(0);
        assert_eq!(
            std::ffi::CStr::from_ptr(llama_cpp_sys_2::ggml_backend_reg_name(backend)),
            c"CPU"
        );
        llama_cpp_sys_2::llama_backend_init();
        assert_eq!(llama_cpp_sys_2::ggml_backend_reg_count(), 1);
    }
}
