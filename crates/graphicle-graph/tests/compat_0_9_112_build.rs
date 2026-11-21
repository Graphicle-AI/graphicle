// Adapted from Graphify 0.9.112, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_dedup.py
//! Changes ported from Graphify 0.9.112. Cases follow the upstream tests
//! named in each comment.

#[test]
fn a_prefix_derived_word_is_not_a_variant_of_its_base() {
    // test_dedup.py (Graphify 0.9.112)
    let same_word_variant = |x: &str, y: &str| {
        graphicle::dispatch(&serde_json::json!({"operation": "dedup", "function": "_same_word_variant", "x": x, "y": y}))
            .unwrap()
            == serde_json::json!(true)
    };
    for (a, b) in [("synchronous", "asynchronous"), ("cyclic", "acyclic"), ("load", "reload")] {
        assert!(!same_word_variant(a, b), "{a}/{b}");
        assert!(!same_word_variant(b, a), "{b}/{a}");
    }
    assert!(same_word_variant("handler", "handlers"));
}
