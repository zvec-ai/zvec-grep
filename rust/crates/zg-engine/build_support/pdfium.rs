//! Compile `PDFium` once into a cached static SDK; no `PDFium` runtime library.
use std::{env, path::Path, process::Command};

pub fn prepare() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    for file in [
        "prepare-pdfium.py",
        "pdfium-build.json",
        "licenses/gcc-runtime.txt",
    ] {
        println!("cargo:rerun-if-changed={}", scripts.join(file).display());
    }
    for name in [
        "PYTHON",
        "ZG_PDFIUM_CACHE_DIR",
        "CARGO_NET_OFFLINE",
        "MACOSX_DEPLOYMENT_TARGET",
        "DEVELOPER_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let python = env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let mut command = Command::new(python);
    command
        .arg(scripts.join("prepare-pdfium.py"))
        .arg("--target")
        .arg(env::var("TARGET").expect("Cargo TARGET"))
        .arg("--cargo-out-dir")
        .arg(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    if let Some(cache) = env::var_os("ZG_PDFIUM_CACHE_DIR") {
        command.arg("--cache-dir").arg(cache);
    }
    let status = command
        .status()
        .expect("preparing static PDFium requires Python 3 (override with PYTHON)");
    assert!(status.success(), "preparing static PDFium failed: {status}");
}
