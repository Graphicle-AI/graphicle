// The published crate's build (bindings/rust/build.rs), reading grammar/.
fn main() {
    let src_dir = std::path::Path::new("grammar");
    let mut c_config = cc::Build::new();
    c_config
        .std("c11")
        .include(src_dir)
        .flag_if_supported("-Wno-unused-value");
    #[cfg(target_env = "msvc")]
    c_config.flag("-utf-8");
    for name in ["parser.c", "scanner.c"] {
        let path = src_dir.join(name);
        c_config.file(&path);
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed=grammar/tree_sitter");
    c_config.compile("tree-sitter-python");
}
