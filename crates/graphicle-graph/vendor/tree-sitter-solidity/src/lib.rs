//! Generated grammar is retained verbatim from tree-sitter-solidity 1.2.13's sdist.
//! No Python interpreter or Python extension is linked.
unsafe extern "C" {
    fn tree_sitter_solidity() -> *const ();
}
pub const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_solidity) };
