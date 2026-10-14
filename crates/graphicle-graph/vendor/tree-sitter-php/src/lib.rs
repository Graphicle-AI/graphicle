unsafe extern "C" {
    fn tree_sitter_php() -> *const ();
    fn tree_sitter_php_only() -> *const ();
}
pub const LANGUAGE_PHP: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_php) };
pub const LANGUAGE_PHP_ONLY: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_php_only) };
