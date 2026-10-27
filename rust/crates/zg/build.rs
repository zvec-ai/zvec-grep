#[path = "llama_runtime_build.rs"]
mod llama_runtime;

fn main() {
    zvec_rust_build::configure();
    if std::env::var_os("CARGO_FEATURE_CUDA").is_some() {
        // zg owns the executable layout for both SDK and source builds. The
        // source wrapper's *.so staging omits versioned SONAME targets.
        let target = std::env::var("TARGET").expect("Cargo TARGET");
        let out = std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR");
        let paths = match (
            std::env::var_os("DEP_LLAMA_RUNTIME_DIR"),
            std::env::var_os("DEP_LLAMA_RUNTIME_MANIFEST"),
        ) {
            (Some(root), Some(manifest)) => llama_runtime::stage(
                std::path::Path::new(&root),
                std::path::Path::new(&manifest),
                &target,
                std::path::Path::new(&out),
            ),
            (None, None) => {
                let backends = std::env::var_os("DEP_LLAMA_BACKENDS_DIR")
                    .expect("source CUDA build must expose DEP_LLAMA_BACKENDS_DIR");
                llama_runtime::stage_source(
                    std::path::Path::new(&backends),
                    &target,
                    std::path::Path::new(&out),
                )
            }
            _ => panic!("incomplete llama.cpp SDK runtime metadata"),
        }
        .expect("stage zg's llama.cpp runtime libraries");
        for path in paths {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Delay direct imports so the executable can locate its sibling lib/
        // before Windows resolves any native model/storage dependencies.
        cc::Build::new()
            .file("windows_runtime.c")
            .compile("zg_windows_runtime");
        println!("cargo:rerun-if-changed=windows_runtime.c");
        println!("cargo:rustc-link-lib=delayimp");
        println!("cargo:rustc-link-arg=/INCLUDE:__pfnDliNotifyHook2");
        println!("cargo:rustc-link-arg=/INCLUDE:zg_runtime_initializer");
        println!("cargo:rustc-link-arg=/DELAYLOAD:zvec_c_api.dll");
        let libraries: &[&str] = if std::env::var_os("CARGO_FEATURE_CUDA").is_some() {
            &["llama.dll", "ggml.dll", "ggml-base.dll"]
        } else {
            &[]
        };
        for library in libraries {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{library}");
        }
    }
}
