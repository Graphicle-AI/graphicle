unsafe extern "C" {
    fn tree_sitter_kotlin() -> *const ();
}
pub const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_kotlin) };
