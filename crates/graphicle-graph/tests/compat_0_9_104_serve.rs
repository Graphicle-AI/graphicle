// Adapted from Graphify 0.9.104, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_serve.py
//! Query changes ported from Graphify 0.9.104. Cases follow the upstream
//! tests named in each comment.
use serde_json::{Value, json};

fn serve(action: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("serve");
    request["action"] = json!(action);
    graphicle::dispatch(&request).unwrap()
}

fn graph(directed: bool, nodes: &[(&str, &str, &str)], links: &[(&str, &str)]) -> Value {
    json!({
        "directed": directed, "multigraph": false, "graph": {},
        "nodes": nodes.iter().map(|(id, label, sf)| json!({
            "id": id, "label": label, "source_file": sf,
        })).collect::<Vec<_>>(),
        "links": links.iter().map(|(a, b)| json!({"source": a, "target": b})).collect::<Vec<_>>(),
    })
}

fn seeds(g: &Value, qs: &Value, order: bool) -> Value {
    let mut req = json!({"graph": g, "scored": qs["ranked"], "best_seed_by_term": qs["best_seed_by_term"]});
    if order {
        req["term_order"] = qs["term_order"].clone();
    }
    serve("pick_seeds", req)
}

#[test]
fn pick_seeds_keeps_same_label_node_that_is_only_match_for_a_term() {
    // test_pick_seeds_keeps_same_label_node_that_is_only_match_for_a_term (#5027).
    let g = graph(
        false,
        &[
            ("hooks_pre", "PreToolUse", "plugins/core/lib/core_hooks/handler/pre_tool_use.rb"),
            ("hooks_run", ".run", "plugins/core/lib/core_hooks/handler/pre_tool_use.rb"),
            ("alpha_pre", "PreToolUse", "plugins/core-alpha/lib/alpha/pre_tool_use.rb"),
        ],
        &[("hooks_pre", "hooks_run")],
    );
    let qs = serve(
        "score_query",
        json!({"graph": g, "terms": ["core", "alpha", "PreToolUse", "hook", "handler"], "collect_per_term_seeds": true}),
    );
    assert_eq!(qs["best_seed_by_term"]["alpha"], "alpha_pre", "{qs}");
    let s = seeds(&g, &qs, true);
    assert!(
        s.as_array().unwrap().contains(&json!("alpha_pre")),
        "only match for 'alpha' was dropped by the label cap: {s}"
    );
}

#[test]
fn pick_seeds_per_term_still_caps_same_label_winner_of_a_label_term() {
    // test_pick_seeds_per_term_still_caps_same_label_winner_of_a_label_term (#1766, #5027).
    let g = graph(
        true,
        &[
            ("get0", "GET", "r0.py"),
            ("get1", "GET", "r1.py"),
            ("get2", "GET", "r2.py"),
            ("x", "x_helper", "x.py"),
        ],
        &[("get2", "x")],
    );
    let qs = serve(
        "score_query",
        json!({"graph": g, "terms": ["get"], "collect_per_term_seeds": true}),
    );
    assert_eq!(qs["best_seed_by_term"]["get"], "get2", "{qs}");
    assert_eq!(seeds(&g, &qs, false), json!(["get0"]));
}
