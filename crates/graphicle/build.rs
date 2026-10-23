//! Build script. On Windows with the MSVC linker, the `graphicle` program gets an
//! 8 MiB main-thread stack, as on Linux and macOS: MSVC's default is 1 MiB,
//! too little for the recursive walkers. Built on Windows, it also gets a
//! version resource: product name, company, description and the crate's
//! version, which Windows shows in the file's properties. No effect on any
//! other target.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
