//! Exact tree-sitter-sql 0.3.11 grammar; no Python interpreter or extension.
unsafe extern "C" {
    fn tree_sitter_sql() -> *const ();
}
pub const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_sql) };
