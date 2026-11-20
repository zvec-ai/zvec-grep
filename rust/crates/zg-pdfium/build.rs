use std::{env, path::Path, process::Command};

fn main() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/prepare-pdfium.py");
    println!("cargo:rerun-if-changed={}", script.display());
    for name in ["PYTHON", "DEP_PDFIUM_LIB_PATH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let python = env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let status = Command::new(python)
        .arg(script)
        .arg("--target")
        .arg(env::var("TARGET").expect("Cargo TARGET"))
        .arg("--source-lib")
        .arg(env::var_os("DEP_PDFIUM_LIB_PATH").expect("LiteParse PDFium SDK"))
        .arg("--cargo-out-dir")
        .arg(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"))
        .status()
        .expect("staging LiteParse PDFium requires Python 3");
    assert!(
        status.success(),
        "staging LiteParse PDFium failed: {status}"
    );
}
