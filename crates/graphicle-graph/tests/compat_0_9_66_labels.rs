//! Mirrors `tests/test_labeling.py::test_generate_community_labels_warns_on_partial_success`
//! added upstream in Graphify 0.9.66 (#3671). Partial-label salvage and the
//! retry of missing ids were already present in the Rust port
//! (`src/hardening_tests.rs`); this pins the "labeled N of M" warning.
use graphicle::{build, graph::Graph, llm};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Replies {
    replies: Vec<String>,
    requests: Mutex<Vec<llm::Request>>,
}

impl llm::Transport for Replies {
    fn send(&self, request: &llm::Request) -> llm::Result<Value> {
        let mut calls = self.requests.lock().unwrap();
        let index = calls.len();
        calls.push(request.clone());
        let text = self
            .replies
            .get(index)
            .ok_or_else(|| llm::Error::runtime("unexpected extra request"))?;
        Ok(json!({
            "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2}
        }))
    }
}

fn graph_and_communities() -> (Graph, graphicle::cluster::Communities) {
    let mut graph = Graph::new(false, false);
    let mut communities = vec![];
    for (community, names) in [
        (0, ["login", "session"]),
        (1, ["invoice", "payment"]),
        (2, ["bucket", "blob"]),
    ] {
        let mut members = vec![];
        for name in names {
            graph
                .add_node(
                    json!(name),
                    json!({"label": name, "file_type": "code"})
                        .as_object()
                        .unwrap()
                        .clone(),
                )
                .unwrap();
            members.push(json!(name));
        }
        communities.push((community, members));
    }
    (graph, communities)
}

#[test]
fn generate_community_labels_warns_on_partial_success() {
    // The first reply names only community 0; every retry for the missing ids
    // fails to parse, so 1 and 2 keep their structural fallback names.
    let transport = Arc::new(Replies {
        replies: vec![
            "{\"0\":\"Authentication\"}".into(),
            "not JSON".into(),
            "not JSON".into(),
            "not JSON".into(),
            "not JSON".into(),
        ],
        requests: Mutex::new(vec![]),
    });
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport,
    );
    let (graph, communities) = graph_and_communities();
    let options = llm::LabelOptions {
        backend: Some("openai".into()),
        model: Some("fixture-model".into()),
        ..Default::default()
    };
    let mut usage = llm::Usage::default();
    let mut diagnostics = build::Diagnostics::default();
    let (labels, source) =
        runtime.generate_labels(&graph, &communities, &options, &mut usage, &mut diagnostics);
    assert_eq!(source, "llm");
    assert_eq!(labels.get("0"), Some(&json!("Authentication")));
    assert_eq!(labels.get("1"), Some(&json!("Community 1")));
    assert_eq!(labels.get("2"), Some(&json!("Community 2")));
    let warning = diagnostics
        .stderr
        .iter()
        .find(|line| line.contains("kept structural fallback names"))
        .unwrap_or_else(|| panic!("missing partial-success warning: {:?}", diagnostics.stderr));
    assert!(warning.contains("labeled 1 of 3 communities"), "{warning}");
    assert!(
        warning.contains("2 kept structural fallback names"),
        "{warning}"
    );
}

#[test]
fn generate_community_labels_does_not_warn_on_full_success() {
    let transport = Arc::new(Replies {
        replies: vec!["{\"0\":\"Auth\",\"1\":\"Billing\",\"2\":\"Storage\"}".into()],
        requests: Mutex::new(vec![]),
    });
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport,
    );
    let (graph, communities) = graph_and_communities();
    let options = llm::LabelOptions {
        backend: Some("openai".into()),
        model: Some("fixture-model".into()),
        ..Default::default()
    };
    let mut usage = llm::Usage::default();
    let mut diagnostics = build::Diagnostics::default();
    let (labels, source) =
        runtime.generate_labels(&graph, &communities, &options, &mut usage, &mut diagnostics);
    assert_eq!(source, "llm");
    assert_eq!(labels.get("2"), Some(&json!("Storage")));
    assert!(
        !diagnostics
            .stderr
            .iter()
            .any(|line| line.contains("kept structural fallback names")),
        "{:?}",
        diagnostics.stderr
    );
}
