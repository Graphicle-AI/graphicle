// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Graphify 0.9.76 (bb20ef7): suggested questions interleave their types
//! before the `top_n` cut, so ten ambiguous edges cannot hide every bridge,
//! inferred-relationship, isolation and low-cohesion signal.
use graphicle::analyze::{QUESTIONS_TOP_N, suggest_questions};
use graphicle::cluster::Communities;
use graphicle::graph::Graph;
use serde_json::{Map, Value, json};

fn graph(nodes: Vec<Value>, links: Vec<Value>) -> Graph {
    let data = json!({"directed": false, "multigraph": false, "graph": {},
                      "nodes": nodes, "links": links});
    Graph::from_node_link(&data, "links").unwrap()
}

fn code(id: Value, label: String, file: &str) -> Value {
    json!({"id": id, "label": label, "source_file": file, "file_type": "code"})
}

fn diversity_graph() -> (Graph, Communities) {
    let ambiguous: Vec<String> = (0..10).map(|i| format!("uncertain_{i}")).collect();
    let inferred = ["inferred_a".to_string(), "inferred_b".to_string()];
    let disconnected: Vec<String> = (0..5).map(|i| format!("disconnected_{i}")).collect();
    let mut names = vec!["hub".to_string()];
    names.extend(ambiguous.iter().cloned());
    names.extend(inferred.iter().cloned());
    names.extend(disconnected.iter().cloned());
    let nodes = names
        .iter()
        .map(|n| code(json!(n), n.clone(), "src/example.py"))
        .collect();
    let mut links = vec![];
    for node in &ambiguous {
        links.push(
            json!({"source": "hub", "target": node, "confidence": "AMBIGUOUS", "relation": "uses"}),
        );
    }
    for node in &inferred {
        links.push(
            json!({"source": "hub", "target": node, "confidence": "INFERRED", "relation": "uses"}),
        );
    }
    let ids = |xs: &[String]| xs.iter().map(|x| json!(x)).collect::<Vec<_>>();
    let mut middle = ids(&ambiguous);
    middle.extend(ids(&inferred));
    let communities = vec![
        (0, vec![json!("hub")]),
        (1, middle),
        (2, ids(&disconnected)),
    ];
    (graph(nodes, links), communities)
}

fn types(questions: &[Value]) -> Vec<&str> {
    questions
        .iter()
        .map(|q| q["type"].as_str().unwrap())
        .collect()
}

#[test]
fn suggest_questions_does_not_starve_later_categories() {
    let (g, communities) = diversity_graph();
    let labels = Map::new();
    let questions = suggest_questions(&g, &communities, &labels, QUESTIONS_TOP_N).unwrap();
    assert_eq!(questions.len(), 7);
    assert_eq!(
        types(&questions)[..5],
        [
            "ambiguous_edge",
            "bridge_node",
            "verify_inferred",
            "isolated_nodes",
            "low_cohesion"
        ]
    );
    assert_eq!(
        questions,
        suggest_questions(&g, &communities, &labels, QUESTIONS_TOP_N).unwrap()
    );
}

#[test]
fn suggest_questions_diversity_preserves_limit_and_candidates() {
    let (g, communities) = diversity_graph();
    let labels = Map::new();
    let all = suggest_questions(&g, &communities, &labels, 100).unwrap();
    for top_n in [1, 3, 7, 100] {
        let limited = suggest_questions(&g, &communities, &labels, top_n).unwrap();
        assert_eq!(
            limited,
            all[..(top_n as usize).min(all.len())],
            "top_n={top_n}"
        );
    }
    assert_eq!(
        all.iter().filter(|q| q["type"] == "ambiguous_edge").count(),
        10
    );
    // Graphify 0.9.81 (#4199) tags AMBIGUOUS entries `low_confidence`; every
    // other key is the required three.
    for q in &all {
        let mut keys: Vec<&str> = q.as_object().unwrap().keys().map(String::as_str).collect();
        keys.retain(|k| !(*k == "low_confidence" && q["type"] == "ambiguous_edge"));
        keys.sort();
        assert_eq!(keys, ["question", "type", "why"]);
    }
}

#[test]
fn suggest_questions_single_category_uses_available_slots() {
    let nodes = (0..5)
        .map(|i| code(json!(i), i.to_string(), "example.py"))
        .collect();
    let mut links = vec![];
    for u in 0..5 {
        for v in u + 1..5 {
            links.push(
                json!({"source": u, "target": v, "confidence": "AMBIGUOUS", "relation": "uses"}),
            );
        }
    }
    let g = graph(nodes, links);
    let communities = vec![(0, (0..5).map(|i| json!(i)).collect())];
    let questions = suggest_questions(&g, &communities, &Map::new(), QUESTIONS_TOP_N).unwrap();
    assert_eq!(questions.len(), 7);
    assert!(questions.iter().all(|q| q["type"] == "ambiguous_edge"));
}

#[test]
fn suggest_questions_empty_graph_keeps_no_signal() {
    let g = graph(vec![], vec![]);
    let questions = suggest_questions(&g, &vec![], &Map::new(), QUESTIONS_TOP_N).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["type"], "no_signal");
    assert!(questions[0]["question"].is_null());
}
