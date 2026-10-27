use super::*;

#[test]
fn cuda124_or_newer_needs_no_native_code_exception() {
    for version in [12040, 12080, 13010] {
        assert!(check_capability(version, false, true, || panic!("no device query")).is_ok());
    }
}

#[test]
fn old_cuda12_retains_compatible_native_code() {
    for version in [12000, 12020, 12030] {
        for devices in [vec![(8, 6)], vec![(8, 9)], vec![(8, 6), (8, 9)]] {
            assert!(check_capability(version, true, false, || Ok(devices)).is_ok());
        }
    }
}

#[test]
fn old_driver_rejects_ptx_only_and_mixed_devices() {
    for devices in [
        vec![(5, 0)],
        vec![(6, 1)],
        vec![(7, 0)],
        vec![(7, 5)],
        vec![(8, 0)],
        vec![(9, 0)],
        vec![(8, 6), (7, 5)],
        vec![(7, 5), (8, 6)],
        vec![],
    ] {
        assert!(check_capability(12030, true, false, || Ok(devices)).is_err());
    }
}

#[test]
fn unknown_or_pre_cuda12_driver_cannot_use_the_exception() {
    for version in [0, -1, 11080] {
        assert!(check_capability(version, true, false, || panic!("no query")).is_err());
    }
}

#[test]
fn forced_ptx_and_custom_plugins_cannot_use_the_native_exception() {
    for (native_sm86, force_ptx) in [(false, false), (true, true)] {
        assert!(check_capability(12030, native_sm86, force_ptx, || panic!("no query")).is_err());
    }
}

#[test]
fn forced_ptx_environment_obeys_cuda_precedence() {
    let yes = Some(OsStr::new("1"));
    let no = Some(OsStr::new("0"));
    assert!(!force_ptx_jit(None, None));
    assert!(force_ptx_jit(yes, None));
    assert!(force_ptx_jit(None, yes));
    assert!(force_ptx_jit(yes, no));
    assert!(!force_ptx_jit(no, yes));
    assert!(force_ptx_jit(Some(OsStr::new("01")), None));
    assert!(force_ptx_jit(Some(OsStr::new("unknown")), None));
}

#[test]
fn failed_device_query_is_not_treated_as_native_compatibility() {
    assert_eq!(
        check_capability(12030, true, false, || Err("probe failed".to_owned())),
        Err("probe failed".to_owned())
    );
    assert!(succeeded("cuInit", 0).is_ok());
    assert_eq!(
        succeeded("cuInit", 100),
        Err("cuInit failed with CUDA error 100".to_owned())
    );
}

#[test]
fn cuda13_baseline_allows_every_distributed_native_architecture() {
    for devices in [
        vec![(7, 5)],
        vec![(8, 0)],
        vec![(8, 6)],
        vec![(8, 9)],
        vec![(9, 0)],
        vec![(7, 5), (8, 0), (8, 6), (8, 9), (9, 0)],
    ] {
        assert!(check_cuda13_capability(13000, true, false, || Ok(devices)).is_ok());
    }
}

#[test]
fn cuda13_ptx_only_and_mixed_devices_require_131_driver_capability() {
    for devices in [
        vec![(10, 0)],
        vec![(12, 0)],
        vec![(12, 1)],
        vec![(8, 6), (10, 0)],
        vec![(10, 0), (8, 6)],
    ] {
        for version in [13000, 13009] {
            assert!(check_cuda13_capability(version, true, false, || Ok(devices.clone())).is_err());
        }
        for version in [13010, 14000] {
            assert!(check_cuda13_capability(version, true, false, || Ok(devices.clone())).is_ok());
        }
    }
}

#[test]
fn cuda13_forced_ptx_must_have_a_compatible_target_and_driver() {
    for device in [(7, 5), (8, 0), (8, 6), (8, 9)] {
        // compute90 PTX cannot run on pre-Hopper GPUs, even with a new driver.
        assert!(check_cuda13_capability(13010, true, true, || Ok(vec![device])).is_err());
    }
    for device in [(9, 0), (10, 0), (12, 0)] {
        assert!(check_cuda13_capability(13000, true, true, || Ok(vec![device])).is_err());
        assert!(check_cuda13_capability(13010, true, true, || Ok(vec![device])).is_ok());
    }
    assert!(check_cuda13_capability(13010, true, true, || Ok(vec![(9, 0), (8, 6)])).is_err());
}

#[test]
fn cuda13_rejects_devices_without_primary_code_even_on_a_new_driver() {
    for device in [(5, 0), (6, 1), (7, 0), (7, 4), (0, 0), (9, -1)] {
        assert!(check_cuda13_capability(13010, true, false, || Ok(vec![device])).is_err());
    }
}

#[test]
fn cuda13_unknown_targets_have_no_native_code_exception() {
    for force_ptx in [false, true] {
        assert!(
            check_cuda13_capability(13000, false, force_ptx, || panic!("unknown targets")).is_err()
        );
        assert!(
            check_cuda13_capability(13010, false, force_ptx, || panic!("unknown targets")).is_ok()
        );
    }
}

#[test]
fn cuda13_unknown_or_old_driver_and_failed_device_queries_fail_closed() {
    for version in [-1, 0, 12040, 12090] {
        assert!(check_cuda13_capability(version, true, false, || panic!("no query")).is_err());
    }
    for version in [13000, 13010] {
        assert_eq!(
            check_cuda13_capability(version, true, false, || Err("probe failed".to_owned())),
            Err("probe failed".to_owned())
        );
        assert!(check_cuda13_capability(version, true, false, || Ok(vec![])).is_err());
    }
}
