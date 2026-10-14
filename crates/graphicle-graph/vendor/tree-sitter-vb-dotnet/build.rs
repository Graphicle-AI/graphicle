fn main() {
    for path in ["grammar/parser.c", "grammar/tree_sitter"] {
        println!("cargo:rerun-if-changed={path}");
    }
    cc::Build::new()
        .include("grammar")
        .file("grammar/parser.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .compile("graphicle_vb_dotnet");
}
