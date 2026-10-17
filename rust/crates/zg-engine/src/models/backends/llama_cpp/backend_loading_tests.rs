use super::*;

#[test]
fn working_primary_does_not_load_fallback() {
    let root = tempfile::tempdir().expect("temporary bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback directory");
    let mut loaded = Vec::new();
    let selected = load_with_fallback(root.path(), |path| {
        loaded.push(path.to_path_buf());
        true
    });
    assert_eq!(selected, Some("13.1"));
    assert_eq!(loaded, [root.path()]);
}

#[test]
fn failed_primary_uses_cuda12() {
    let root = tempfile::tempdir().expect("temporary bundle");
    let fallback = root.path().join("cuda12");
    std::fs::create_dir(&fallback).expect("fallback directory");
    let mut loaded = Vec::new();
    let selected = load_with_fallback(root.path(), |path| {
        loaded.push(path.to_path_buf());
        path == fallback
    });
    assert_eq!(selected, Some("12.4"));
    assert_eq!(loaded, [root.path(), &fallback]);
}

#[test]
fn no_cuda_keeps_the_cpu_backend_loaded() {
    let root = tempfile::tempdir().expect("temporary bundle");
    std::fs::create_dir(root.path().join("cuda12")).expect("fallback directory");
    let mut attempts = 0;
    assert_eq!(
        load_with_fallback(root.path(), |_| {
            attempts += 1;
            false
        }),
        None
    );
    assert_eq!(attempts, 2);
}

#[test]
fn missing_fallback_is_not_loaded() {
    let root = tempfile::tempdir().expect("temporary bundle");
    let mut attempts = 0;
    assert_eq!(
        load_with_fallback(root.path(), |_| {
            attempts += 1;
            false
        }),
        None
    );
    assert_eq!(attempts, 1);
}
