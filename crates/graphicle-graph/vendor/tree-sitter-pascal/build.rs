fn main() {
    println!("cargo:rerun-if-changed=grammar");
    cc::Build::new()
        .include("grammar")
        .file("grammar/parser.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("graphicle_pascal");
}
