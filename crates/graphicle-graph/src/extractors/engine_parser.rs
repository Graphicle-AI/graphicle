// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Statically linked parsers for the generic engine; no dynamic Python imports.
use super::models::LanguageConfig;
use crate::Result;
use tree_sitter::{Language, Tree};
pub fn language(config: &LanguageConfig) -> Result<Language> {
    Ok(match config.ts_module.as_str() {
        "tree_sitter_python" => tree_sitter_python::LANGUAGE.into(),
        "tree_sitter_javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "tree_sitter_typescript" => match config.ts_language_fn.as_str() {
            "language_typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "language_tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
            _ => return Err(format!("No language function in {}", config.ts_module).into()),
        },
        "tree_sitter_java" => tree_sitter_java::LANGUAGE.into(),
        "tree_sitter_groovy" => graphicle_tree_sitter_groovy::LANGUAGE.into(),
        "tree_sitter_c" => tree_sitter_c::LANGUAGE.into(),
        "tree_sitter_cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "tree_sitter_c_sharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "tree_sitter_kotlin" => graphicle_tree_sitter_kotlin::LANGUAGE.into(),
        "tree_sitter_scala" => tree_sitter_scala::LANGUAGE.into(),
        "tree_sitter_php" => match config.ts_language_fn.as_str() {
            "language_php" => graphicle_tree_sitter_php::LANGUAGE_PHP.into(),
            "language_php_only" => graphicle_tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
            _ => return Err(format!("No language function in {}", config.ts_module).into()),
        },
        "tree_sitter_ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "tree_sitter_swift" => tree_sitter_swift::LANGUAGE.into(),
        "tree_sitter_lua" => tree_sitter_lua::LANGUAGE.into(),
        "tree_sitter_go" => tree_sitter_go::LANGUAGE.into(),
        "tree_sitter_rust" => tree_sitter_rust::LANGUAGE.into(),
        "tree_sitter_zig" => {
            crate::grammar!("grammar-zig", "tree_sitter_zig", tree_sitter_zig::LANGUAGE)
                .map_err(crate::Error::from)?
        }
        "tree_sitter_elixir" => crate::grammar!(
            "grammar-elixir",
            "tree_sitter_elixir",
            tree_sitter_elixir::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_julia" => crate::grammar!(
            "grammar-julia",
            "tree_sitter_julia",
            tree_sitter_julia::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_bash" => tree_sitter_bash::LANGUAGE.into(),
        "tree_sitter_fortran" => crate::grammar!(
            "grammar-fortran",
            "tree_sitter_fortran",
            tree_sitter_fortran::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_verilog" => crate::grammar!(
            "grammar-verilog",
            "tree_sitter_verilog",
            tree_sitter_verilog::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_powershell" => crate::grammar!(
            "grammar-powershell",
            "tree_sitter_powershell",
            tree_sitter_powershell::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_objc" => crate::grammar!(
            "grammar-objc",
            "tree_sitter_objc",
            tree_sitter_objc::LANGUAGE
        )
        .map_err(crate::Error::from)?,
        "tree_sitter_dm" => {
            crate::grammar!("grammar-dm", "tree_sitter_dm", tree_sitter_dm::LANGUAGE)
                .map_err(crate::Error::from)?
        }
        "tree_sitter_commonlisp" => crate::grammar!(
            "grammar-commonlisp",
            "tree_sitter_commonlisp",
            tree_sitter_commonlisp::LANGUAGE_COMMONLISP
        )
        .map_err(crate::Error::from)?,
        _ => return Err(format!("{} not installed", config.ts_module).into()),
    })
}
pub struct Parsed {
    pub tree: Tree,
    pub source: Vec<u8>,
}
/// The JavaScript-family grammar `config` parses with, named as
/// [`super::resolution_facts::js_parse_tag`] names it.
pub fn js_tag(config: &LanguageConfig) -> Option<&'static str> {
    match (config.ts_module.as_str(), config.ts_language_fn.as_str()) {
        ("tree_sitter_javascript", _) => Some("javascript"),
        ("tree_sitter_typescript", "language_typescript") => Some("typescript"),
        ("tree_sitter_typescript", "language_tsx") => Some("tsx"),
        _ => None,
    }
}
pub fn parse(config: &LanguageConfig, source: &[u8]) -> Result<Parsed> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language(config)?)
        .map_err(crate::Error::from)?;
    let mut source = source.to_vec();
    if !source.is_empty() && !source.ends_with(b"\n") {
        source.push(b'\n');
    }
    let tree = parser
        .parse(&source, None)
        .ok_or("Native parser did not produce a tree")?;
    Ok(Parsed { tree, source })
}
