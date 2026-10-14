//! Mirrors the graph-level tests added upstream in Graphify 0.9.73:
//! `tests/test_analyze.py` (`_top_level_dir` root files, backslashes, leading
//! `./`, and the `repo` attribute in the cross-repo surprise bonus) and
//! `tests/test_cluster_exclude_hubs.py` (hub-only neighbours follow their
//! excluded hub instead of becoming singletons).
use graphicle::{
    analyze::{SurpriseContext, surprise_score, top_level_dir},
    cluster::{Communities, cluster},
    graph::Graph,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const CROSS: &str = "connects across different repos/directories";

fn attrs(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn pair_graph(
    a: (&str, &str, Option<&str>),
    b: (&str, &str, Option<&str>),
    edge_source: &str,
) -> Graph {
    let mut graph = Graph::new(false, false);
    for (id, (label, source, repo)) in [("a", a), ("b", b)] {
        let mut node = attrs(json!({"label": label, "source_file": source, "file_type": "code"}));
        if let Some(repo) = repo {
            node.insert("repo".into(), json!(repo));
        }
        graph.add_node(json!(id), node).unwrap();
    }
    graph
        .add_edge(
            json!("a"),
            json!("b"),
            None,
            attrs(json!({
                "relation": "calls",
                "confidence": "EXTRACTED",
                "weight": 1.0,
                "source_file": edge_source,
            })),
        )
        .unwrap();
    graph
}

fn score(graph: &Graph, u_source: &str, v_source: &str) -> (i64, Vec<String>) {
    let u = graph.node_index(&json!("a")).unwrap();
    let v = graph.node_index(&json!("b")).unwrap();
    let edge = graph.edges_between(&json!("a"), &json!("b"))[0];
    let communities = [Some(0), Some(0)];
    surprise_score(
        graph,
        u,
        v,
        &edge.attributes,
        SurpriseContext {
            node_community: &communities,
            u_source,
            v_source,
            degrees: None,
        },
    )
}

fn cross_reasons(src_u: &str, src_v: &str, repo_u: Option<&str>, repo_v: Option<&str>) -> bool {
    let graph = pair_graph(("A", src_u, repo_u), ("B", src_v, repo_v), src_u);
    score(&graph, src_u, src_v).1.iter().any(|r| r == CROSS)
}

#[test]
fn top_level_dir_treats_root_files_as_the_root() {
    assert_eq!(top_level_dir("service.py"), ".");
    assert_eq!(top_level_dir("pkg/service.py"), "pkg");
    assert_eq!(top_level_dir("/opt/vendor/ext.py"), "");
    assert_eq!(top_level_dir("pkg\\a.py"), "pkg");
    assert_eq!(top_level_dir("./pkg/a.py"), "pkg");
    assert_eq!(top_level_dir("././a.py"), ".");
    assert_eq!(top_level_dir(".\\pkg\\a.py"), "pkg");
    assert_eq!(top_level_dir("./"), ".");
}

#[test]
fn surprise_score_flat_directory_is_not_cross_directory() {
    let graph = pair_graph(
        ("handle", "service.py", None),
        ("Store", "store.py", None),
        "service.py",
    );
    let (score_flat, reasons_flat) = score(&graph, "service.py", "store.py");
    let (score_nested, _) = score(&graph, "pkg/service.py", "pkg/store.py");
    assert!(!reasons_flat.iter().any(|r| r == CROSS));
    assert_eq!(score_flat, score_nested);
}

#[test]
fn surprise_score_root_file_vs_subdirectory_still_crosses() {
    let graph = pair_graph(
        ("main", "main.py", None),
        ("Store", "pkg/store.py", None),
        "main.py",
    );
    assert!(
        score(&graph, "main.py", "pkg/store.py")
            .1
            .iter()
            .any(|r| r == CROSS)
    );
}

#[test]
fn surprise_score_root_file_vs_out_of_root_absolute_path_still_crosses() {
    let graph = pair_graph(
        ("main", "main.py", None),
        ("Ext", "/opt/vendor/ext.py", None),
        "main.py",
    );
    assert!(
        score(&graph, "main.py", "/opt/vendor/ext.py")
            .1
            .iter()
            .any(|r| r == CROSS)
    );
}

#[test]
fn surprise_score_merged_graph_counts_different_repos_as_crossing() {
    assert!(cross_reasons(
        "server.py",
        "client.py",
        Some("svcA"),
        Some("svcB")
    ));
    assert!(cross_reasons(
        "src/x.py",
        "src/y.py",
        Some("svcA"),
        Some("svcB")
    ));
    assert!(!cross_reasons(
        "src/x.py",
        "src/y.py",
        Some("svcA"),
        Some("svcA")
    ));
}

#[test]
fn surprise_score_normalizes_backslash_paths() {
    assert!(cross_reasons("pkg\\a.py", "lib\\b.py", None, None));
    assert!(!cross_reasons("pkg\\a.py", "pkg\\b.py", None, None));
}

#[test]
fn surprise_score_ignores_leading_dot_slash() {
    assert!(cross_reasons("./pkg/a.py", "b.py", None, None));
    assert!(!cross_reasons("./a.py", "b.py", None, None));
}

fn edge(graph: &mut Graph, u: &str, v: &str) {
    graph
        .add_edge(json!(u), json!(v), None, Map::new())
        .unwrap();
}

/// Two 4-cliques, one hub H joined to all of A and to b1, and three leaves
/// connected to H only. Degrees: H=8, A=4, b1=4, B=3, leaves=1, so
/// `exclude_hubs_percentile=90` excludes exactly H.
fn hub_with_private_leaves() -> Graph {
    let mut graph = Graph::new(false, false);
    for group in [["a1", "a2", "a3", "a4"], ["b1", "b2", "b3", "b4"]] {
        for (i, u) in group.iter().enumerate() {
            for v in &group[i + 1..] {
                edge(&mut graph, u, v);
            }
        }
    }
    for a in ["a1", "a2", "a3", "a4"] {
        edge(&mut graph, "H", a);
    }
    edge(&mut graph, "H", "b1");
    for leaf in ["l1", "l2", "l3"] {
        edge(&mut graph, "H", leaf);
    }
    graph
}

fn node_to_cid(communities: &Communities) -> BTreeMap<String, i64> {
    communities
        .iter()
        .flat_map(|(cid, nodes)| {
            nodes
                .iter()
                .map(move |n| (n.as_str().unwrap().to_owned(), *cid))
        })
        .collect()
}

fn has_community(communities: &Communities, members: &[&str]) -> bool {
    let wanted: Vec<Value> = members.iter().map(|m| json!(m)).collect();
    communities.iter().any(|(_, nodes)| *nodes == wanted)
}

#[test]
fn exclude_hubs_keeps_hub_only_neighbours_with_their_hub() {
    let graph = hub_with_private_leaves();
    let communities = cluster(&graph, 1.0, Some(90.0), "auto").unwrap();
    let assigned = node_to_cid(&communities);
    let all: Vec<String> = graph
        .nodes()
        .iter()
        .map(|n| n.id.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(assigned.len(), all.len());
    assert!(all.iter().all(|n| assigned.contains_key(n)));
    for leaf in ["l1", "l2", "l3"] {
        assert_eq!(assigned[leaf], assigned["H"], "{leaf} left its hub");
    }
    assert!(communities.iter().all(|(_, nodes)| nodes.len() != 1));
}

#[test]
fn exclude_hubs_leaves_true_isolates_alone() {
    let mut graph = hub_with_private_leaves();
    graph.add_node(json!("lonely"), Map::new()).unwrap();
    let communities = cluster(&graph, 1.0, Some(90.0), "auto").unwrap();
    assert!(has_community(&communities, &["lonely"]));
}

#[test]
fn exclude_hubs_without_hubs_is_unchanged() {
    let graph = hub_with_private_leaves();
    assert_eq!(
        cluster(&graph, 1.0, Some(100.0), "auto").unwrap(),
        cluster(&graph, 1.0, None, "auto").unwrap()
    );
}

#[test]
fn exclude_hubs_hub_only_neighbour_with_self_loop_joins_its_hub() {
    let mut graph = hub_with_private_leaves();
    edge(&mut graph, "l1", "l1");
    let communities = cluster(&graph, 1.0, Some(90.0), "auto").unwrap();
    let assigned = node_to_cid(&communities);
    assert_eq!(assigned["l1"], assigned["H"]);
}

#[test]
fn exclude_hubs_self_loop_only_node_is_left_alone() {
    let mut graph = hub_with_private_leaves();
    edge(&mut graph, "solo", "solo");
    let communities = cluster(&graph, 1.0, Some(90.0), "auto").unwrap();
    assert!(has_community(&communities, &["solo"]));
}
