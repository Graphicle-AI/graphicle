// Adapted from Graphify 0.9.109, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_llm_parse_fragments.py
//! LLM reply parsing changes ported from Graphify 0.9.109.
use serde_json::{Value, json};

fn parse(raw: &str) -> Value {
    graphicle::dispatch(&json!({"operation": "llm", "function": "_parse_llm_json", "raw": raw}))
        .unwrap()["value"]
        .clone()
}

fn nodes(v: &Value) -> usize {
    v["nodes"].as_array().map_or(0, Vec::len)
}

#[test]
fn ambiguous_fragments_are_refused() {
    // #5192: two independent fragments, or an array of two, are not cached
    // as if the first were complete.
    let a = r#"{"nodes":[{"id":"a","label":"A"}],"edges":[]}"#;
    let b = r#"{"nodes":[{"id":"b","label":"B"}],"edges":[]}"#;
    assert_eq!(nodes(&parse(&format!("first {a} then {b}"))), 0);
    assert_eq!(nodes(&parse(&format!("[{a}, {b}]"))), 0);
    // One fragment wrapped in an array, or the same fragment twice, is one answer.
    assert_eq!(nodes(&parse(&format!("[{a}]"))), 1);
    assert_eq!(nodes(&parse(&format!("```json\n{a}\n```\nagain: {a}"))), 1);
}
