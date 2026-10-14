fn main() {
    println!("cargo:rerun-if-changed=src");
    cc::Build::new()
        .include("src")
        .file("src/parser.c")
        .file("src/scanner.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("graphicle_kotlin");
}
