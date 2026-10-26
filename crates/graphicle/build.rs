//! Build script. It hashes the crate's sources and its Cargo.lock for the key
//! of the saved graphs and query files (`GRAPHICLE_RUNTIME_CODE`,
//! `src/query_graph.rs`, `src/query_file.rs`). On Windows with the MSVC
//! linker, the `graphicle` program gets an 8 MiB main-thread stack, as on
//! Linux and macOS: MSVC's default is 1 MiB, too little for the recursive
//! walkers. Built on Windows, it also gets a version resource: product name,
//! company, description and the crate's version, which Windows shows in the
//! file's properties.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Data this program derives from code graphs (`query_graph`,
    // `query_file`) is keyed by a hash of its sources and of the dependency
    // versions it is built with: `GRAPHICLE_RUNTIME_CODE`.
    println!("cargo:rerun-if-changed=src");
    if std::path::Path::new("Cargo.lock").is_file() {
        println!("cargo:rerun-if-changed=Cargo.lock");
    }
    println!("cargo:rustc-env=GRAPHICLE_RUNTIME_CODE={}", source_code());
    if std::env::var_os("CARGO_CFG_TARGET_ENV").is_some_and(|env| env == "msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        #[cfg(windows)]
        version_resource("Graphicle", "graphicle.exe");
    }
}

/// Compiles the version resource with the Windows SDK's rc.exe (RC_PATH names
/// another) and links it. FileVersion and ProductVersion are the crate's
/// version. Without a resource compiler the program still builds, without
/// the resource, and Cargo shows a warning.
#[cfg(windows)]
fn version_resource(description: &str, file: &str) {
    if let Err(error) = winresource::WindowsResource::new()
        .set("ProductName", "Graphicle")
        .set("CompanyName", "KarLex AI, Inc.")
        .set("FileDescription", description)
        .set("OriginalFilename", file)
        .compile()
    {
        println!(
            "cargo:warning={file} is built without its version resource: {error} (it needs the Windows SDK rc.exe, or RC_PATH)"
        );
    }
}

/// A hash of every file under `src/` (path and bytes), the package version
/// and the crate's Cargo.lock when it has one, as the engine hashes its own
/// sources: data a program derives with this crate's code is keyed by it.
/// The lock file pins every dependency of the program, among them the ones
/// that shape a search index's strings (the tokenizers and the Unicode
/// normalization).
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
    if let Ok(lock) = std::fs::read(root.join("Cargo.lock")) {
        feed(b"Cargo.lock");
        feed(&[0]);
        feed(&lock);
        feed(&[0]);
    }
    format!("{hash:016x}")
}
