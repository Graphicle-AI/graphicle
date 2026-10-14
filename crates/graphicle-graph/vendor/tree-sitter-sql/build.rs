fn main() {
    for path in [
        "grammar/parser.c",
        "grammar/scanner.c",
        "grammar/tree_sitter",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    cc::Build::new()
        .include("grammar")
        .file("grammar/parser.c")
        .file("grammar/scanner.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("graphicle_sql");
}
