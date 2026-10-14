// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py, graphify/extractors/objc.py
//! C++ declarator helper from engine.py, also used by the Objective-C grammar.
//! This helper does not complete the generic extraction engine.
use super::base::read_text;
use tree_sitter::Node;
pub fn cpp_declarator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "identifier" {
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
                        "identifier" | "pointer_declarator" | "reference_declarator"
                    )
                })
        });
        if let Some(inner) = inner {
            return cpp_declarator_name(inner, source);
        }
    }
    None
}
