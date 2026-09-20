use std::env;

fn main() {
    // The jieba dict auto-discovery uses dladdr() to locate the loaded
    // libzvec_c_api at runtime. On Linux dladdr lives in libdl (merged into
    // libc on glibc >= 2.34, but -ldl remains valid there and on musl).
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=dylib=dl");
    }

    // Forward the native library directory resolved by `zvec-rust-sys` one hop
    // further down the graph. `zvec-rust-sys` (links = "zvec_c_api") exposes it
    // to this build script as `DEP_ZVEC_C_API_LIB_DIR`; by re-publishing it as
    // our own `links = "zvec_rust"` metadata, crates that depend directly on
    // `zvec-rust` receive it as `DEP_ZVEC_RUST_LIB_DIR`. This lets a downstream
    // binary set the runtime rpath to the shared library it will ship (see the
    // `zvec-rust-build` helper crate).
    if let Ok(lib_dir) = env::var("DEP_ZVEC_C_API_LIB_DIR") {
        println!("cargo:lib_dir={lib_dir}");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
