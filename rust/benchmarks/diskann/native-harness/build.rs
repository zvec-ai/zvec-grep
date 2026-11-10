fn main() {
    // This local benchmark must load the explicitly built C++ branch library.
    // Keep an absolute rpath and verify the resolved path again with dladdr.
    let dir = std::env::var("ZVEC_LIB_DIR").expect("set the local branch library directory");
    assert!(std::path::Path::new(&dir).is_absolute());
    assert_eq!(std::env::var("CARGO_CFG_TARGET_OS").unwrap(), "macos");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    println!("cargo:rerun-if-env-changed=ZVEC_LIB_DIR");
    println!("cargo:rerun-if-changed={dir}/libzvec_c_api.dylib");
}
