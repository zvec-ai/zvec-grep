fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    let script = root.join("prepare-onnxruntime.py");
    println!("cargo:rerun-if-changed={}", script.display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("onnxruntime-distributions.json").display()
    );
    for variable in [
        "PYTHON",
        "CARGO_NET_OFFLINE",
        "ORT_DYLIB_PATH",
        "ZG_ORT_CACHE_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    // Explicit external runtimes remain available for custom execution providers
    // (including WebGPU), offline development and embedding the engine.
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    let python = std::env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let mut command = std::process::Command::new(python);
    command
        .arg(script)
        .arg("--target")
        .arg(std::env::var("TARGET").expect("Cargo TARGET"))
        .arg("--cargo-out-dir")
        .arg(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    if std::env::var_os("CARGO_FEATURE_CUDA").is_some() {
        command.arg("--cuda");
    }
    if let Some(cache) = std::env::var_os("ZG_ORT_CACHE_DIR") {
        command.arg("--cache-dir").arg(cache);
    }
    let status = command
        .status()
        .expect("preparing ONNX Runtime requires Python 3 (override with PYTHON)");
    assert!(status.success(), "preparing ONNX Runtime failed: {status}");
}
