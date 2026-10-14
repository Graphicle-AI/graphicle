//! tree-sitter-python 0.25.0's generated grammar, retained verbatim from the
//! published crate except for the grammar pack's fix to the external scanner's
//! serializer (`serialize-buffer-overflow.patch`; provenance in SOURCE.json).
//! No Python interpreter or Python extension is linked.
unsafe extern "C" {
    fn tree_sitter_python() -> *const ();
}

/// The tree-sitter language function for this grammar.
pub const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_python) };
