// Adapted from Graphify 0.9.108, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_serve.py
//! Query changes ported from Graphify 0.9.108. Cases follow the upstream
//! tests named in each comment.
use serde_json::{Value, json};

fn serve(action: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("serve");
    request["action"] = json!(action);
    graphicle::dispatch(&request).unwrap()
}

#[test]
fn context_filter_reaches_heritage_edges_without_a_context() {
    // test_serve.py (#5178)
    let g = json!({"directed": true, "multigraph": false, "graph": {}, "nodes": [
        {"id": "square", "label": "Square"}, {"id": "base", "label": "Base"},
        {"id": "ishape", "label": "IShape"}, {"id": "ts", "label": "TsSquare"},
    ], "links": [
        {"source": "square", "target": "base", "relation": "inherits"},
        {"source": "square", "target": "ishape", "relation": "implements"},
        {"source": "ts", "target": "base", "relation": "inherits", "context": "type"},
    ]});
    let edges = |filters: Value| {
        serve("filter_contexts", json!({"graph": g, "filters": filters}))["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| format!("{}->{}", l["source"].as_str().unwrap(), l["target"].as_str().unwrap()))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let inherits = edges(json!(["inheritance"]));
    assert!(inherits.contains("square->base") && inherits.contains("ts->base"), "{inherits:?}");
    assert!(!inherits.contains("square->ishape"), "{inherits:?}");
    let implements = edges(json!(["interface"]));
    assert_eq!(implements.into_iter().collect::<Vec<_>>(), ["square->ishape"]);
    // `type` keeps meaning the TypeScript context only.
    assert_eq!(edges(json!(["type"])).into_iter().collect::<Vec<_>>(), ["ts->base"]);
}
