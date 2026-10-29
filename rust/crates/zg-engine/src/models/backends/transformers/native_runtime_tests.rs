use super::*;

// ONNX IR 8 / opset 13: Identity(x: float[1]) -> y: float[1]. A self-contained
// 70-byte protobuf fixture keeps lifecycle tests offline and independent of HF.
const IDENTITY_MODEL: &[u8] = b"\x08\x08\x3a\x3e\x0a\x10\x0a\x01\x78\x12\x01\x79\x22\x08\x49\x64\x65\x6e\x74\x69\x74\x79\x12\x08\x69\x64\x65\x6e\x74\x69\x74\x79\x5a\x0f\x0a\x01\x78\x12\x0a\x0a\x08\x08\x01\x12\x04\x0a\x02\x08\x01\x62\x0f\x0a\x01\x79\x12\x0a\x0a\x08\x08\x01\x12\x04\x0a\x02\x08\x01\x42\x02\x10\x0d";

#[test]
fn sessions_can_reload_and_process_exits_cleanly() {
    const CHILD: &str = "ZG_TEST_ORT_LIFECYCLE_CHILD";
    if env::var_os(CHILD).is_some() {
        for _ in 0..2 {
            let environment = initialize().expect("load official ORT");
            let invalid = ort::session::Session::builder(&environment)
                .expect("builder")
                .commit_from_memory(b"invalid model");
            assert!(invalid.is_err());
            let mut session = ort::session::Session::builder(&environment)
                .expect("builder")
                .with_intra_threads(1)
                .expect("single thread")
                .commit_from_memory(IDENTITY_MODEL)
                .expect("load identity graph after failed load");
            let input =
                ort::value::Tensor::from_array(([1_usize], vec![42_f32])).expect("input tensor");
            let outputs = session
                .run(ort::inputs![input])
                .expect("identity inference");
            assert_eq!(
                outputs[0].try_extract_tensor::<f32>().expect("output").1,
                &[42.0]
            );
        }
        println!("ORT lifecycle passed");
        return;
    }
    // rc.13 + ORT 1.21 can report a passing test, then SIGSEGV while exiting.
    // Assert the child process's status, not just the inference result.
    let output = std::process::Command::new(env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "models::backends::transformers::native_runtime::tests::sessions_can_reload_and_process_exits_cleanly",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("run lifecycle child");
    assert!(
        output.status.success(),
        "ORT shutdown failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ORT lifecycle passed"));
}

#[test]
fn explicit_library_has_priority_over_bundle_and_executable() {
    let path = PathBuf::from("custom/runtime");
    assert_eq!(
        runtime_path(
            Some(path.clone()),
            Some("bundle".into()),
            Some("bin/zg".into())
        )
        .expect("explicit runtime"),
        path
    );
}

#[test]
fn uses_bundle_override_or_executable_directory_without_cwd_lookup() {
    let override_path =
        runtime_path(None, Some("bundle".into()), Some("bin/zg".into())).expect("bundle runtime");
    assert_eq!(override_path.parent(), Some(std::path::Path::new("bundle")));
    let adjacent = runtime_path(None, None, Some("bin/zg".into())).expect("adjacent runtime");
    assert_eq!(adjacent.parent(), Some(std::path::Path::new("bin")));
    assert_eq!(adjacent.file_name(), override_path.file_name());
}

#[test]
fn packaged_executable_uses_sibling_lib_directory() {
    let root = tempfile::tempdir().expect("temporary package");
    let bin = root.path().join("bin");
    let lib = root.path().join("lib");
    std::fs::create_dir_all(&bin).expect("bin directory");
    std::fs::create_dir_all(&lib).expect("lib directory");
    let expected = runtime_path(None, Some(lib.clone()), None).expect("library filename");
    std::fs::write(&expected, b"runtime").expect("packaged library");
    let runtime = runtime_path(None, None, Some(bin.join("zg"))).expect("packaged runtime");
    assert_eq!(runtime, expected);
}

#[test]
fn unrelated_sibling_directory_does_not_hide_the_adjacent_runtime() {
    let root = tempfile::tempdir().expect("temporary Cargo profile");
    let deps = root.path().join("deps");
    let lib = root.path().join("lib");
    std::fs::create_dir_all(&deps).expect("deps directory");
    std::fs::create_dir_all(&lib).expect("unrelated lib directory");
    let expected = runtime_path(None, Some(deps.clone()), None).expect("library filename");
    std::fs::write(&expected, b"runtime").expect("adjacent library");
    let executable = deps.join("model_test");
    assert_eq!(
        runtime_path(None, None, Some(executable.clone())).expect("adjacent runtime"),
        expected
    );
    // A same-named directory is not a loadable library either.
    std::fs::create_dir(lib.join(expected.file_name().expect("filename")))
        .expect("unrelated directory");
    assert_eq!(
        runtime_path(None, None, Some(executable)).expect("runtime"),
        expected
    );
}

#[test]
fn unknown_executable_requires_an_explicit_runtime_location() {
    assert!(runtime_path(None, None, None).is_err());
    assert!(runtime_path(None, Some("bundle".into()), None).is_ok());
}
