fn main() {
    for dialect in ["php", "php_only"] {
        println!("cargo:rerun-if-changed={dialect}/src");
        cc::Build::new()
            .include(format!("{dialect}/src"))
            .file(format!("{dialect}/src/parser.c"))
            .file(format!("{dialect}/src/scanner.c"))
            .flag_if_supported("-std=c11")
            .flag_if_supported("-Wno-unused-parameter")
            .compile(&format!("graphicle_{dialect}"));
    }
    println!("cargo:rerun-if-changed=common");
}
