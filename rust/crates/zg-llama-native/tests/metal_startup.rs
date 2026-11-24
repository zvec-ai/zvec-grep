#[cfg(target_os = "macos")]
fn main() {
    use std::{env, process::Command, thread};
    use zg_llama_native::try_set_metal_residency_default;

    const SETTING: &str = "GGML_METAL_NO_RESIDENCY";
    const KEEP: &str = "ZVEC_GREP_METAL_KEEP_RESIDENCY";
    const CASE: &str = "ZG_TEST_METAL_STARTUP_CASE";
    match env::var(CASE).as_deref() {
        Ok("default") => {
            assert!(try_set_metal_residency_default());
            assert_eq!(env::var(SETTING).as_deref(), Ok("1"));
        }
        Ok("explicit") => {
            assert!(try_set_metal_residency_default());
            assert_eq!(env::var(SETTING).as_deref(), Ok("0"));
        }
        Ok("keep") => {
            assert!(try_set_metal_residency_default());
            assert!(env::var_os(SETTING).is_none());
        }
        Ok("threaded") => {
            let applied = thread::spawn(try_set_metal_residency_default)
                .join()
                .expect("worker");
            assert!(!applied);
            assert!(env::var_os(SETTING).is_none());
        }
        _ => {
            for case in ["default", "explicit", "keep", "threaded"] {
                let mut child = Command::new(env::current_exe().expect("test executable"));
                child.env_remove(SETTING).env_remove(KEEP).env(CASE, case);
                if case == "explicit" {
                    child.env(SETTING, "0");
                } else if case == "keep" {
                    child.env(KEEP, "1");
                }
                assert!(
                    child.status().expect("start test process").success(),
                    "{case}"
                );
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
