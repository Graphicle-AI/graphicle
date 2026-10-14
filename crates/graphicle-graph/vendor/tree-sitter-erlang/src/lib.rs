//! Generated grammar is retained verbatim from the tree-sitter-language-pack 0.11.0 sdist.
//! No Python interpreter or Python extension is linked.
unsafe extern "C" {
    fn tree_sitter_erlang() -> *const ();
}
pub const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_erlang) };
