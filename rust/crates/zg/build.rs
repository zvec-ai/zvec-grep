fn main() {
    zvec_rust_build::configure();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Delay direct imports so the executable can locate its sibling lib/
        // before Windows resolves any native model/storage dependencies.
        cc::Build::new()
            .file("windows_runtime.c")
            .compile("zg_windows_runtime");
        println!("cargo:rerun-if-changed=windows_runtime.c");
        println!("cargo:rustc-link-lib=delayimp");
        println!("cargo:rustc-link-arg=/INCLUDE:__pfnDliNotifyHook2");
        println!("cargo:rustc-link-arg=/DELAYLOAD:zvec_c_api.dll");
        let libraries: &[&str] = if std::env::var_os("CARGO_FEATURE_CUDA").is_some() {
            &["llama.dll", "ggml.dll", "ggml-base.dll"]
        } else {
            &["onnxruntime.dll", "DirectML.dll"]
        };
        for library in libraries {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{library}");
        }
    }
}
