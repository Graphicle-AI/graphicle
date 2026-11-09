// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py, graphify/extractors/objc.py
//! C++ declarator helper from engine.py, also used by the Objective-C grammar.
//! This helper does not complete the generic extraction engine.
use super::base::read_text;
use tree_sitter::Node;
pub fn cpp_declarator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    // A class field's declarator too (Graphify 0.9.101, #3215).
    if matches!(node.kind(), "identifier" | "field_identifier") {
        return Some(read_text(node, source));
    }
    if matches!(
        node.kind(),
        "pointer_declarator" | "reference_declarator" | "init_declarator"
    ) {
        let inner = node.child_by_field_name("declarator").or_else(|| {
            (0..node.child_count())
                .filter_map(|i| node.child(i))
                .find(|n| {
                    matches!(
                        n.kind(),
                        "identifier"
                            | "field_identifier"
                            | "pointer_declarator"
                            | "reference_declarator"
                    )
                })
        });
        if let Some(inner) = inner {
            return cpp_declarator_name(inner, source);
        }
    }
    None
}
