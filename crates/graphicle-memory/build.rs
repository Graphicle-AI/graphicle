//! Build script. On Windows with the MSVC linker, `graphicle-memory` gets an
//! 8 MiB main-thread stack, as on Linux and macOS: MSVC's default is 1 MiB,
//! too little for the recursive walkers. No effect on any other target.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_CFG_TARGET_ENV").is_some_and(|env| env == "msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
