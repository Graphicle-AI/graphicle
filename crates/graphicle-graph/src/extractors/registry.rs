// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/__init__.py
//! Registry from graphify/extractors/__init__.py. Entries retain the actual
//! native function pointers; the call adapter only unifies return signatures.
use super::*;
use crate::Result;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::LazyLock};
#[derive(Clone, Copy)]
pub enum Extractor {
    Infallible(fn(&Path) -> Value),
    Fallible(fn(&Path) -> Result<Value>),
    Sql(fn(&Path, Option<&[u8]>) -> Result<Value>),
}
impl Extractor {
    pub fn extract(self, path: &Path) -> Result<Value> {
        match self {
            Self::Infallible(f) => Ok(f(path)),
            Self::Fallible(f) => f(path),
            Self::Sql(f) => f(path, None),
        }
    }
}
pub static LANGUAGE_EXTRACTORS: LazyLock<BTreeMap<&'static str, Extractor>> = LazyLock::new(|| {
    use Extractor::{Fallible as F, Infallible as I, Sql};
    BTreeMap::from([
        ("apex", F(apex::extract_apex)),
        ("bash", F(bash::extract_bash)),
        ("blade", I(blade::extract_blade)),
        ("cobol", I(cobol::extract_cobol)),
        ("commonlisp", I(commonlisp::extract_commonlisp)),
        ("dart", F(dart::extract_dart)),
        ("delphi_form", I(pascal_forms::extract_delphi_form)),
        ("dm", F(dm::extract_dm)),
        ("dmf", I(dm::extract_dmf)),
        ("dmi", F(dm::extract_dmi)),
        ("dmm", I(dm::extract_dmm)),
        ("elixir", I(elixir::extract_elixir)),
        ("erlang", F(erlang::extract_erlang)),
        ("fortran", I(fortran::extract_fortran)),
        ("go", I(go::extract_go)),
        ("json", I(json_config::extract_json)),
        ("julia", I(julia::extract_julia)),
        ("lazarus_form", I(pascal_forms::extract_lazarus_form)),
        ("markdown", F(markdown::extract_markdown)),
        ("objc", F(objc::extract_objc)),
        ("pascal", F(pascal::extract)),
        ("powershell", I(powershell::extract_powershell)),
        (
            "powershell_manifest",
            I(powershell::extract_powershell_manifest),
        ),
        ("r", I(r::extract_r)),
        ("razor", I(razor::extract_razor)),
        ("rust", I(rust::extract_rust)),
        ("sln", I(sln::extract_sln)),
        ("solidity", I(solidity::extract_solidity)),
        ("sql", Sql(sql::extract_sql)),
        ("terraform", F(terraform::extract_terraform)),
        ("vbnet", I(vbnet::extract_vbnet)),
        ("verilog", F(verilog::extract_verilog)),
        ("zig", I(zig::extract_zig)),
    ])
});
/// Registry keys in upstream insertion order (`vbnet` follows `verilog`).
const ORDER: [&str; 33] = [
    "apex",
    "bash",
    "blade",
    "cobol",
    "commonlisp",
    "dart",
    "delphi_form",
    "dm",
    "dmf",
    "dmi",
    "dmm",
    "elixir",
    "erlang",
    "fortran",
    "go",
    "json",
    "julia",
    "lazarus_form",
    "markdown",
    "objc",
    "pascal",
    "powershell",
    "powershell_manifest",
    "r",
    "razor",
    "rust",
    "sln",
    "solidity",
    "sql",
    "terraform",
    "verilog",
    "vbnet",
    "zig",
];
/// Every registered extractor key, in the upstream registry's order.
pub fn registered_languages() -> Vec<&'static str> {
    debug_assert_eq!(ORDER.len(), LANGUAGE_EXTRACTORS.len());
    debug_assert!(ORDER.iter().all(|k| LANGUAGE_EXTRACTORS.contains_key(k)));
    ORDER.to_vec()
}
pub fn extract(language: &str, path: &Path) -> Result<Value> {
    LANGUAGE_EXTRACTORS
        .get(language)
        .ok_or_else(|| format!("No registered extractor: {language}"))?
        .extract(path)
}
