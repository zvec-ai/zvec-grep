use super::*;

#[test]
fn only_distribution_architectures_receive_the_native_code_exception() {
    assert!(distribution_fallback(None, None));
    assert!(distribution_fallback(
        Some("50-virtual;61-virtual;70-virtual;75-virtual;80-virtual;86-real"),
        None
    ));
    assert!(!distribution_fallback(Some("86-virtual"), None));
    assert!(!distribution_fallback(Some("native"), None));
    assert!(!distribution_fallback(None, Some("/custom/sdk")));
}

#[test]
fn only_known_primary_targets_receive_the_native_code_exception() {
    let targets = "75-real;80-real;86-real;89-real;90-real;90-virtual";
    assert!(distribution_primary(None, None, Some("prebuilt")));
    assert!(distribution_primary(Some(targets), None, Some("source")));
    for mode in [None, Some("auto"), Some("source"), Some("external")] {
        assert!(!distribution_primary(None, None, mode));
    }
    for targets in ["native", "75-virtual;80-virtual;86-real;90-virtual"] {
        assert!(!distribution_primary(Some(targets), None, Some("source")));
    }
    assert!(!distribution_primary(
        Some(targets),
        Some("/custom/sdk"),
        Some("external")
    ));
}

#[test]
fn cpu_initialization_does_not_register_gpu_plugins() {
    let state = BackendInitialization::new();
    assert_eq!(
        state.initialize(false, || Ok(7), |_| panic!("CPU must not load GPU")),
        Ok(&7)
    );
    assert!(state.gpu.get().is_none());
}

#[test]
fn cpu_then_gpu_registers_plugins_without_reinitializing_core() {
    let state = BackendInitialization::new();
    state
        .initialize(false, || Ok(7), |_| panic!("CPU must not load GPU"))
        .expect("CPU");
    let mut registrations = 0;
    state
        .initialize(
            true,
            || panic!("core already initialized"),
            |core| {
                assert_eq!(*core, 7);
                registrations += 1;
            },
        )
        .expect("GPU");
    state
        .initialize(
            true,
            || panic!("core already initialized"),
            |_| panic!("GPU already registered"),
        )
        .expect("GPU again");
    assert_eq!(registrations, 1);
}

#[test]
fn missing_gpu_plugins_do_not_poison_the_cpu_core() {
    let state = BackendInitialization::new();
    state
        .initialize(true, || Ok(7), |_| { /* no GPU plugins found */ })
        .expect("core");
    assert_eq!(
        state.initialize(
            false,
            || panic!("core already initialized"),
            |_| panic!("CPU")
        ),
        Ok(&7)
    );
}

#[test]
fn failed_core_never_registers_gpu_plugins() {
    let state = BackendInitialization::<()>::new();
    assert!(
        state
            .initialize(
                true,
                || Err("CPU plugin unavailable".to_owned()),
                |()| panic!("no core")
            )
            .is_err()
    );
}

#[test]
fn concurrent_requests_initialize_core_and_gpu_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let state = BackendInitialization::new();
    let core_calls = AtomicUsize::new(0);
    let gpu_calls = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for wants_gpu in [false, true, false, true] {
            let state = &state;
            let core_calls = &core_calls;
            let gpu_calls = &gpu_calls;
            scope.spawn(move || {
                state
                    .initialize(
                        wants_gpu,
                        || {
                            core_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                        |()| {
                            gpu_calls.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .expect("initialized");
            });
        }
    });
    assert_eq!(core_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gpu_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn working_primary_does_not_load_fallback() {
    let root = tempfile::tempdir().expect("temporary bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback directory");
    let mut loaded = Vec::new();
    let selected = load_with_fallback(
        root.path(),
        false,
        |plugin| {
            assert_eq!(plugin, CudaPlugin::Primary);
            Ok(())
        },
        |path| {
            loaded.push(path.to_path_buf());
            true
        },
    );
    assert_eq!(selected, Some("13.1"));
    assert_eq!(loaded, [root.path()]);
}

#[test]
fn failed_primary_uses_cuda12() {
    let root = tempfile::tempdir().expect("temporary bundle");
    let fallback = root.path().join("cuda12");
    std::fs::create_dir(&fallback).expect("fallback directory");
    let mut loaded = Vec::new();
    let selected = load_with_fallback(
        root.path(),
        false,
        |_| Ok(()),
        |path| {
            loaded.push(path.to_path_buf());
            path == fallback
        },
    );
    assert_eq!(selected, Some("12.4"));
    assert_eq!(loaded, [root.path(), &fallback]);
}

#[test]
fn no_cuda_keeps_the_cpu_backend_loaded() {
    let root = tempfile::tempdir().expect("temporary bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback directory");
    let mut attempts = 0;
    assert_eq!(
        load_with_fallback(
            root.path(),
            false,
            |_| Ok(()),
            |_| {
                attempts += 1;
                false
            }
        ),
        None
    );
    assert_eq!(attempts, 2);
}

#[test]
fn missing_fallback_is_not_loaded() {
    let root = tempfile::tempdir().expect("temporary bundle");
    let mut attempts = 0;
    assert_eq!(
        load_with_fallback(
            root.path(),
            false,
            |plugin| {
                assert_eq!(plugin, CudaPlugin::Primary);
                Ok(())
            },
            |_| {
                attempts += 1;
                false
            }
        ),
        None
    );
    assert_eq!(attempts, 1);
}

#[test]
fn rejected_fallback_is_never_opened_and_cpu_remains_usable() {
    let root = tempfile::tempdir().expect("bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback");
    let state = BackendInitialization::new();
    let mut loaded = Vec::new();
    state
        .initialize(
            true,
            || Ok(7),
            |_| {
                let selected = load_with_fallback(
                    root.path(),
                    false,
                    |plugin| match plugin {
                        CudaPlugin::Primary => Ok(()),
                        CudaPlugin::Fallback => Err("PTX requires a newer driver".to_owned()),
                        CudaPlugin::Override => panic!("no override"),
                    },
                    |path| {
                        loaded.push(path.to_path_buf());
                        false
                    },
                );
                assert_eq!(selected, None);
            },
        )
        .expect("CPU core survives GPU rejection");
    assert_eq!(loaded, [root.path()]);
    assert_eq!(
        state.initialize(false, || panic!("reuse CPU"), |_| panic!("no GPU")),
        Ok(&7)
    );
}

#[test]
fn custom_plugin_cannot_bypass_preflight_through_the_primary_loader() {
    let root = tempfile::tempdir().expect("bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback");
    assert_eq!(
        load_with_fallback(
            root.path(),
            true,
            |plugin| {
                assert_eq!(plugin, CudaPlugin::Override);
                Err("custom architecture unknown on older driver".to_owned())
            },
            |_| panic!("must not load override")
        ),
        None
    );
}

#[test]
fn custom_plugin_with_compatible_driver_retains_upstream_loader_behavior() {
    let root = tempfile::tempdir().expect("bundle");
    let mut checks = Vec::new();
    assert_eq!(
        load_with_fallback(
            root.path(),
            true,
            |plugin| {
                checks.push(plugin);
                Ok(())
            },
            |path| {
                assert_eq!(path, root.path());
                true
            }
        ),
        Some("13.1")
    );
    assert_eq!(checks, [CudaPlugin::Override, CudaPlugin::Primary]);
}

#[test]
fn rejected_primary_is_never_opened_and_fallback_is_checked_before_loading() {
    use std::cell::RefCell;
    let root = tempfile::tempdir().expect("bundle");
    let fallback = root.path().join("cuda12");
    std::fs::create_dir(&fallback).expect("fallback");
    let events = RefCell::new(Vec::new());
    assert_eq!(
        load_with_fallback(
            root.path(),
            false,
            |plugin| match plugin {
                CudaPlugin::Primary => {
                    events.borrow_mut().push("check primary");
                    Err("SM100 requires newer PTX support".to_owned())
                }
                CudaPlugin::Fallback => {
                    events.borrow_mut().push("check fallback");
                    Ok(())
                }
                CudaPlugin::Override => panic!("no override"),
            },
            |path| {
                assert_eq!(path, fallback);
                events.borrow_mut().push("load fallback");
                true
            },
        ),
        Some("12.4")
    );
    assert_eq!(
        events.into_inner(),
        ["check primary", "check fallback", "load fallback"]
    );
}

#[test]
fn rejected_plugins_never_open_and_preserve_cpu_initialization() {
    let root = tempfile::tempdir().expect("bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback");
    let state = BackendInitialization::new();
    let mut checked = Vec::new();
    state
        .initialize(
            true,
            || Ok(7),
            |_| {
                assert_eq!(
                    load_with_fallback(
                        root.path(),
                        false,
                        |plugin| {
                            checked.push(plugin);
                            Err("incompatible driver".to_owned())
                        },
                        |_| panic!("rejected plugins must not be opened"),
                    ),
                    None
                );
            },
        )
        .expect("CPU core survives preflight rejection");
    assert_eq!(checked, [CudaPlugin::Primary, CudaPlugin::Fallback]);
    assert_eq!(
        state.initialize(false, || panic!("reuse CPU"), |_| panic!("no GPU")),
        Ok(&7)
    );
}
