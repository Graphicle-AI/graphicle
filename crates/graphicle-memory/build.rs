//! Build script. It hashes the crate's sources for `build_code`
//! (`GRAPHICLE_MEMORY_CODE`). On Windows with the MSVC linker,
//! `graphicle-memory` gets an 8 MiB main-thread stack, as on Linux and macOS:
//! MSVC's default is 1 MiB, too little for the recursive walkers.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rustc-env=GRAPHICLE_MEMORY_CODE={}", source_code());
    if std::env::var_os("CARGO_CFG_TARGET_ENV").is_some_and(|env| env == "msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}

/// A hash of every file under `src/` (path and bytes) and the package
/// version, as the engine hashes its own sources: data a program derives
/// with this crate's code is keyed by it.
fn source_code() -> String {
    use std::path::{Path, PathBuf};
    fn collect(path: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(path).expect("read source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                collect(&path, files);
            } else {
                files.push(path);
            }
        }
    }
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(
        std::env::var("CARGO_PKG_VERSION")
            .unwrap_or_default()
            .as_bytes(),
    );
    for path in files {
        feed(
            path.strip_prefix(&root)
                .expect("source below the manifest")
                .to_string_lossy()
                .replace('\\', "/")
                .as_bytes(),
        );
        feed(&[0]);
        feed(&std::fs::read(&path).expect("read source file"));
        feed(&[0]);
    }
    format!("{hash:016x}")
}
