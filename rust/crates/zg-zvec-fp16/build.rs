fn main() {
    println!("cargo:rerun-if-changed=../../../native");
    zvec_rust_build::configure();
}
