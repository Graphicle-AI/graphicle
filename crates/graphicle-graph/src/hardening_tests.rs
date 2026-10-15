//! Native contracts that remain meaningful without a Python oracle.
use crate::{
    build, cache, export_obsidian, graph::Graph, install_fs, llm, llm_labels, paths, serve, wiki,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

fn graph() -> Graph {
    Graph::from_node_link(&json!({"directed":true,"nodes":[{"id":"a","label":"Alpha","community":0},{"id":"b","label":"Beta","community":0}],"links":[{"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"}]}), "links").unwrap()
}

#[test]
fn borrowed_identifier_hashes_preserve_numeric_and_tuple_aliases() {
    let mut graph = Graph::new(true, true);
    for (ids, expected) in [
        (vec![json!(true), json!(1), json!(1.0)], 0),
        (vec![json!(false), json!(0), json!(-0.0)], 1),
        (
            vec![json!(["x", [true, null]]), json!(["x", [1.0, null]])],
            2,
        ),
    ] {
        for id in ids {
            assert_eq!(graph.add_node(id.clone(), Map::new()).unwrap(), expected);
            assert_eq!(graph.node_index(&id), Some(expected));
        }
    }
    assert_eq!(graph.node_count(), 3);
    graph
        .add_edge(json!(1), json!(0), Some(json!(true)), Map::new())
        .unwrap();
    graph
        .add_edge(json!(1.0), json!(false), Some(json!(1.0)), Map::new())
        .unwrap();
    assert_eq!(graph.edge_count(), 1);
}

#[test]
fn graph_removal_preserves_order_parallel_edges_and_adjacency() {
    for directed in [false, true] {
        for multigraph in [false, true] {
            let mut graph = Graph::new(directed, multigraph);
            for id in 0..8 {
                graph.add_node(json!(id), Map::new()).unwrap();
            }
            for id in 0..8 {
                for key in 0..2 {
                    graph
                        .add_edge(json!(id), json!((id + 1) % 8), Some(json!(key)), Map::new())
                        .unwrap();
                }
            }
            graph
                .add_edge(json!(7), json!(7), None, Map::new())
                .unwrap();
            assert_eq!(
                graph
                    .remove_nodes_from(&[json!(1), json!(4), json!(99)])
                    .unwrap(),
                2
            );
            assert_eq!(
                graph
                    .nodes()
                    .iter()
                    .map(|n| n.id.clone())
                    .collect::<Vec<_>>(),
                vec![json!(0), json!(2), json!(3), json!(5), json!(6), json!(7)]
            );
            let links: Vec<_> = graph.iter_edges().collect();
            assert_eq!(links.len(), 4 * if multigraph { 2 } else { 1 } + 1);
            for (index, node) in graph.nodes().iter().enumerate() {
                assert_eq!(graph.node_index(&node.id), Some(index));
                for (source, target, edge) in graph.out_edges(index) {
                    assert_eq!(source, index);
                    assert!(target < graph.node_count());
                    assert!(
                        links
                            .iter()
                            .any(|(_, _, candidate)| std::ptr::eq(*candidate, edge))
                    );
                    if directed {
                        assert!(
                            graph
                                .in_edges(target)
                                .iter()
                                .any(|(from, _, e)| *from == source && std::ptr::eq(*e, edge))
                        );
                    }
                }
            }
            let copy = Graph::from_node_link(&graph.to_node_link("links"), "links").unwrap();
            assert_eq!(copy.to_node_link("links"), graph.to_node_link("links"));
        }
    }
}

#[test]
fn invalid_edge_does_not_partially_create_nodes() {
    let mut graph = Graph::new(true, true);
    for (target, key) in [
        (Value::Null, None),
        (json!({}), None),
        (json!("b"), Some(json!([]))),
    ] {
        let error = graph
            .add_edge(json!("a"), target, key, Map::new())
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidGraph);
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }
    assert!(graph.out_edges(usize::MAX).is_empty());
    assert!(graph.in_edges(usize::MAX).is_empty());
}

#[test]
fn invalid_graph_metadata_is_a_typed_boundary_error() {
    for value in [
        Value::Null,
        json!([]),
        json!({"graph":[],"nodes":[],"links":[]}),
    ] {
        assert_eq!(
            Graph::from_node_link(&value, "links").unwrap_err().kind(),
            crate::ErrorKind::InvalidGraph
        );
    }
}

#[test]
fn high_degree_adjacency_keeps_order_through_reordering_removal_and_node_compaction() {
    for directed in [false, true] {
        for multigraph in [false, true] {
            let mut graph = Graph::new(directed, multigraph);
            for id in 0..=97 {
                graph.add_node(json!(id), Map::new()).unwrap();
            }
            for neighbor in 1..=96 {
                graph
                    .add_edge(json!(0), json!(neighbor), Some(json!("base")), Map::new())
                    .unwrap();
                if multigraph {
                    graph
                        .add_edge(
                            json!(0),
                            json!(neighbor),
                            Some(json!("parallel")),
                            Map::new(),
                        )
                        .unwrap();
                }
                if directed {
                    graph
                        .add_edge(json!(neighbor), json!(97), None, Map::new())
                        .unwrap();
                }
            }
            let descending: Vec<_> = (1..=96).rev().collect();
            graph
                .restore_neighbor_order(&json!([[0, descending]]))
                .unwrap();
            if directed {
                graph
                    .restore_predecessor_order(&json!([[97, descending]]))
                    .unwrap();
            }
            graph
                .add_edge(
                    json!(0),
                    json!(1),
                    Some(json!("base")),
                    Map::from_iter([("updated".into(), json!(true))]),
                )
                .unwrap();
            assert_eq!(
                graph.number_of_edges(&json!(0), &json!(1)),
                if multigraph { 2 } else { 1 }
            );
            while graph.number_of_edges(&json!(0), &json!(48)) > 0 {
                graph.remove_edge(&json!(0), &json!(48), None).unwrap();
            }
            graph
                .add_edge(json!(0), json!(48), None, Map::new())
                .unwrap();
            graph.remove_nodes_from(&[json!(3), json!(50)]).unwrap();
            let neighbors: Vec<_> = graph
                .neighbor_indices(0, false)
                .map(|i| graph.nodes()[i].id.clone())
                .collect();
            let mut expected: Vec<_> = descending
                .iter()
                .filter(|&&n| ![3, 48, 50].contains(&n))
                .map(|&n| json!(n))
                .collect();
            expected.push(json!(48));
            assert_eq!(neighbors, expected);
            if directed {
                let sink = graph.node_index(&json!(97)).unwrap();
                let predecessors: Vec<_> = graph
                    .neighbor_indices(sink, true)
                    .map(|i| graph.nodes()[i].id.clone())
                    .collect();
                assert_eq!(
                    predecessors,
                    descending
                        .iter()
                        .filter(|&&n| ![3, 50].contains(&n))
                        .map(|&n| json!(n))
                        .collect::<Vec<_>>()
                );
            }
            let saved = graph.to_node_link("links");
            assert_eq!(
                Graph::from_node_link_owned(saved.clone(), "links")
                    .unwrap()
                    .to_node_link("links"),
                saved
            );
        }
    }
}

#[test]
fn owned_graph_import_moves_attribute_storage_and_matches_borrowed_errors() {
    let data = json!({"nodes":[{"id":"owned", "metadata":{"body":"a".repeat(4096)}}],"links":[]});
    let allocation = data["nodes"][0]["metadata"]["body"]
        .as_str()
        .unwrap()
        .as_ptr();
    let graph = Graph::from_node_link_owned(data, "links").unwrap();
    assert_eq!(
        graph.nodes()[0].attributes["metadata"]["body"]
            .as_str()
            .unwrap()
            .as_ptr(),
        allocation
    );

    for data in [
        Value::Null,
        json!([]),
        json!({"nodes":[null]}),
        json!({"nodes":[],"graph":false,"links":[]}),
        json!({"nodes":[{"id":null}],"links":[]}),
        json!({"nodes":[{"id":["nested",[1]]}],"links":[{"source":["nested",[1]],"target":"b"}]}),
        json!({"nodes":[],"links":[{"source":"a"}]}),
        json!({"nodes":[],"links":[{"source":"a","target":"b","key":[]}]}),
    ] {
        let borrowed = Graph::from_node_link(&data, "links").unwrap_err();
        let owned = Graph::from_node_link_owned(data, "links").unwrap_err();
        assert_eq!(owned.kind(), borrowed.kind());
        assert_eq!(owned.to_string(), borrowed.to_string());
    }
    let aliased = json!({"nodes":[{"id":"a","source":"a","target":"b"}]});
    assert_eq!(
        Graph::from_node_link_owned(aliased.clone(), "nodes")
            .unwrap()
            .to_node_link("links"),
        Graph::from_node_link(&aliased, "nodes")
            .unwrap()
            .to_node_link("links")
    );
}

#[test]
fn graph_import_preserves_attribute_order_when_removing_reserved_fields() {
    let document = json!({
        "directed": true,
        "multigraph": true,
        "nodes": [
            {"id": "a", "first": 1, "second": 2, "third": 3},
            {"id": "b"}
        ],
        "links": [
            {"source": "a", "first": 1, "target": "b", "second": 2, "key": 7, "third": 3}
        ]
    });
    for graph in [
        Graph::from_node_link(&document, "links").unwrap(),
        Graph::from_node_link_owned(document, "links").unwrap(),
    ] {
        let serialized = graph.to_node_link("links");
        let keys = |value: &Value| {
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&serialized["nodes"][0]),
            ["first", "second", "third", "id"]
        );
        assert_eq!(
            keys(&serialized["links"][0]),
            ["first", "second", "third", "source", "target", "key"]
        );
    }
}

#[test]
fn owned_dispatch_preserves_graph_operation_results() {
    let graph = graph();
    for operation in [
        "affected.hits",
        "affected.resolve",
        "affected.format",
        "affected.label",
        "cluster.cohesion",
        "cluster.scores",
        "analyze.god_nodes",
    ] {
        let mut request = json!({"operation":operation,"seed":"a","query":"Alpha","nodes":["a","b"],"communities":{"0":["a","b"]}});
        request["graph"] = graph.to_node_link(if operation.starts_with("affected.") {
            "links"
        } else {
            "edges"
        });
        assert_eq!(
            crate::dispatch(&request).unwrap(),
            crate::dispatch_owned(request).unwrap(),
            "{operation}"
        );
    }
    for request in [
        json!(null),
        json!([]),
        json!({"operation":"affected.hits","graph":{}}),
    ] {
        assert_eq!(
            crate::dispatch(&request).unwrap_err().to_string(),
            crate::dispatch_owned(request).unwrap_err().to_string()
        );
    }
}

fn stamp(path: &std::path::Path) -> std::time::SystemTime {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(946684800);
    file.set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
    file.metadata().unwrap().modified().unwrap()
}

#[test]
fn unchanged_atomic_text_preserves_metadata_and_changed_text_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("page.md");
    assert!(paths::write_text_atomic_if_changed(&path, "hello\r\n世界\n").unwrap());
    let previous = stamp(&path);
    assert!(!paths::write_text_atomic_if_changed(&path, "hello\r\n世界\n").unwrap());
    assert_eq!(path.metadata().unwrap().modified().unwrap(), previous);
    assert!(paths::write_text_atomic_if_changed(&path, "updated\n").unwrap());
    assert_eq!(fs::read_to_string(path).unwrap(), "updated\n");
}

#[test]
fn wiki_unchanged_pages_survive_and_renamed_pages_are_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let graph = graph();
    let communities = json!({"0":["a","b"]});
    let labels = Map::from_iter([("0".into(), json!("Subsystem A"))]);
    wiki::to_wiki(&graph, &communities, dir.path(), &labels, &Map::new(), &[]).unwrap();
    let path = dir.path().join("Subsystem_A.md");
    let old = stamp(&path);
    wiki::to_wiki(&graph, &communities, dir.path(), &labels, &Map::new(), &[]).unwrap();
    assert_eq!(path.metadata().unwrap().modified().unwrap(), old);
    let renamed = Map::from_iter([("0".into(), json!("Subsystem B"))]);
    wiki::to_wiki(&graph, &communities, dir.path(), &renamed, &Map::new(), &[]).unwrap();
    assert!(!path.exists());
    assert!(dir.path().join("Subsystem_B.md").exists());
    assert!(wiki::to_wiki(&graph, &json!({}), dir.path(), &renamed, &Map::new(), &[]).is_err());
    assert!(dir.path().join("index.md").exists());
}

// Upstream PR #3095 (#3032/#3033): index rows link to the slug each article was
// written under, so identically labelled communities and god nodes stay reachable.
#[test]
fn wiki_index_links_same_labelled_articles_to_their_own_files() {
    let dir = tempfile::tempdir().unwrap();
    let graph = Graph::from_node_link(&json!({"directed":false,"nodes":[
        {"id":"n1","label":"GameState","file_type":"code","source_file":"a.py"},
        {"id":"n2","label":"GameState","file_type":"code","source_file":"b.py"}],
        "links":[{"source":"n1","target":"n2","relation":"references","confidence":"INFERRED","weight":1.0}]}), "links").unwrap();
    let communities = json!({"0":["n1"],"1":["n2"]});
    let labels = Map::from_iter([("0".into(), json!("Core")), ("1".into(), json!("Core"))]);
    let gods = [
        json!({"id":"n1","label":"GameState","degree":1}),
        json!({"id":"n2","label":"GameState","degree":1}),
    ];
    let (count, _) = wiki::to_wiki(
        &graph,
        &communities,
        dir.path(),
        &labels,
        &Map::new(),
        &gods,
    )
    .unwrap();
    assert_eq!(count, 4);
    let index = fs::read_to_string(dir.path().join("index.md")).unwrap();
    for slug in ["Core", "Core_2", "GameState", "GameState_2"] {
        assert!(dir.path().join(format!("{slug}.md")).exists(), "{slug}");
        assert!(
            index.contains(&format!("]({slug}.md)")),
            "{slug} in\n{index}"
        );
    }
    assert_eq!(index.matches("](Core.md)").count(), 1);
    assert_eq!(index.matches("](GameState.md)").count(), 1);
    assert_eq!(wiki::md_link_to("a[b]", None), "a\\[b\\]");
    assert_eq!(wiki::md_link_to("a[b]", Some("A_b")), "[a\\[b\\]](A_b.md)");
}

#[test]
fn obsidian_preserves_owned_unchanged_pages_and_user_notes() {
    let dir = tempfile::tempdir().unwrap();
    let graph = graph();
    fs::write(dir.path().join("personal.md"), "private notes").unwrap();
    let communities = vec![(0, vec![json!("a"), json!("b")])];
    let options = export_obsidian::VaultOptions::default();
    let mut logs = build::Diagnostics::default();
    export_obsidian::to_obsidian(&graph, &communities, dir.path(), &options, &mut logs).unwrap();
    let paths = walk_md(dir.path());
    assert!(!paths.is_empty());
    let before: Vec<_> = paths.iter().map(|p| (p.clone(), stamp(p))).collect();
    export_obsidian::to_obsidian(&graph, &communities, dir.path(), &options, &mut logs).unwrap();
    for (path, time) in before {
        assert_eq!(path.metadata().unwrap().modified().unwrap(), time);
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("personal.md")).unwrap(),
        "private notes"
    );
}
fn walk_md(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = vec![];
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(walk_md(&path));
        } else if path.extension().is_some_and(|s| s == "md") {
            found.push(path);
        }
    }
    found
}

#[test]
fn learning_sidecar_invalidates_both_pinned_and_lru_contexts() {
    for pinned in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        fs::write(&path, graph().to_node_link("links").to_string()).unwrap();
        let cache = serve::GraphContextCache::new(1);
        let initial = cache.load(&path, pinned).unwrap();
        assert!(Arc::ptr_eq(&initial, &cache.load(&path, pinned).unwrap()));
        let sidecar = dir.path().join(crate::reflect::LEARNING_SIDECAR_NAME);
        fs::write(
            &sidecar,
            json!({"nodes":{"a":{"summary":"learned fact"}}}).to_string(),
        )
        .unwrap();
        let added = cache.load(&path, pinned).unwrap();
        assert!(!Arc::ptr_eq(&initial, &added));
        assert_eq!(
            added.graph.attributes()["_learning_overlay"]["a"]["summary"],
            "learned fact"
        );
        fs::write(
            &sidecar,
            json!({"nodes":{"a":{"summary":"corrected, longer fact"}}}).to_string(),
        )
        .unwrap();
        let changed = cache.load(&path, pinned).unwrap();
        assert!(!Arc::ptr_eq(&added, &changed));
        fs::remove_file(sidecar).unwrap();
        let deleted = cache.load(&path, pinned).unwrap();
        assert!(!Arc::ptr_eq(&changed, &deleted));
        assert_eq!(deleted.graph.attributes()["_learning_overlay"], json!({}));
    }
}

#[test]
fn mcp_identifier_aliases_resolve_the_same_node_and_neighbors() {
    let context = serve::GraphContext::new(graph()).unwrap();
    for tool in ["get_node", "get_neighbors"] {
        let expected = serve::call_graph_tool(&context, tool, &json!({"label":"a"}), None).unwrap();
        for key in ["node_id", "id"] {
            assert_eq!(
                serve::call_graph_tool(&context, tool, &json!({key:"a"}), None).unwrap(),
                expected
            );
        }
        assert!(
            serve::call_graph_tool(&context, tool, &json!({}), None)
                .unwrap()
                .contains("accepted keys")
        );
    }
    let tools = serve::tools_list();
    for tool in tools
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| matches!(tool["name"].as_str(), Some("get_node" | "get_neighbors")))
    {
        // Upstream 0.9.69 advertises `label` plus the `node_id` alias and
        // requires neither; `id` stays an accepted, unadvertised alias.
        let schema = &tool["inputSchema"];
        assert!(schema["properties"]["label"].is_object(), "{schema}");
        assert!(schema["properties"]["node_id"].is_object(), "{schema}");
        assert!(
            schema.get("required").is_none() && schema.get("anyOf").is_none(),
            "{schema}"
        );
    }
}

#[test]
fn rejected_semantic_cache_invalidation_is_file_prompt_and_mode_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = cache::ExtractionCache::default();
    let first = dir.path().join("first.md");
    let other = dir.path().join("other.md");
    fs::write(&first, "first document").unwrap();
    fs::write(&other, "other document").unwrap();
    let options = cache::CacheOptions {
        root: dir.path().to_owned(),
        kind: "semantic".into(),
        prompt: Some(cache::Prompt::Text("prompt one".into())),
        ..Default::default()
    };
    let alternate = cache::CacheOptions {
        prompt: Some(cache::Prompt::Text("prompt two".into())),
        ..options.clone()
    };
    let deep = cache::CacheOptions {
        kind: "semantic-deep".into(),
        ..options.clone()
    };
    let data = |path: &std::path::Path| json!({"nodes":[{"id":"n","source_file":path,"label":"fact"}],"edges":[],"hyperedges":[]});
    for option in [&options, &alternate, &deep] {
        cache.save(&first, &data(&first), option).unwrap();
    }
    cache.save(&other, &data(&other), &options).unwrap();
    assert!(cache.load(&first, &options).unwrap().is_some());
    cache.invalidate(&first, &options).unwrap();
    assert!(cache.load(&first, &options).unwrap().is_none());
    assert!(cache.load(&other, &options).unwrap().is_some());
    assert!(cache.load(&first, &alternate).unwrap().is_some());
    assert!(cache.load(&first, &deep).unwrap().is_some());
    cache.invalidate(&first, &options).unwrap();
}

#[test]
fn registration_refreshes_exact_h1_without_confusing_user_mentions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("CLAUDE.md");
    fs::write(
        &path,
        "# User\r\nWe discuss graphicle here.\r\n\r\n## graphicle\r\nUser subsection\r\n",
    )
    .unwrap();
    assert!(install_fs::register_block(&path, "# graphicle\nInstalled path v1\n").unwrap());
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("We discuss graphicle here.\r\n"));
    assert!(content.contains("## graphicle\r\nUser subsection"));
    assert!(!install_fs::register_block(&path, "# graphicle\nInstalled path v1\n").unwrap());
    fs::write(
        &path,
        format!("{content}\n# More user notes\nPreserve this\n"),
    )
    .unwrap();
    assert!(install_fs::register_block(&path, "# graphicle\nInstalled path v2\n").unwrap());
    let content = fs::read_to_string(&path).unwrap();
    assert!(!content.contains("Installed path v1"));
    assert!(content.contains("Installed path v2"));
    // The rewrite keeps the file's own ending (CRLF here) on every line,
    // the appended LF lines included (Graphify 0.9.88, #4360).
    assert!(content.contains("# More user notes\r\nPreserve this"));
    assert_eq!(
        content.matches('\n').count(),
        content.matches("\r\n").count()
    );
}

#[test]
fn stale_recorded_merge_root_falls_back_to_the_current_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    fs::create_dir(&out).unwrap();
    fs::write(
        out.join(".graphicle_root"),
        dir.path()
            .join("missing-old-clone")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        crate::build_normalize::infer_merge_root(&out.join("graph.json"), ".graphicle"),
        Some(crate::fsutil::plain(&dir.path().canonicalize().unwrap()))
    );
}

#[test]
fn same_relation_keeps_extracted_provenance_in_both_input_orders() {
    let strong = json!({"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED","source_file":"module.py","source_location":"L2"});
    let weak = json!({"source":"a","target":"b","relation":"calls","confidence":"INFERRED","source_file":"module.py"});
    for edges in [
        vec![strong.clone(), weak.clone()],
        vec![weak, strong.clone()],
    ] {
        let mut input = json!({"nodes":[{"id":"a","label":"alpha()","file_type":"code","source_file":"module.py"},{"id":"b","label":"beta()","file_type":"code","source_file":"module.py"}],"edges":edges});
        let graph =
            build::build_from_json(&mut input, true, None, &mut build::Diagnostics::default())
                .unwrap();
        let edges: Vec<_> = graph.iter_edges().collect();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].2.attributes["confidence"], "EXTRACTED");
        assert_eq!(edges[0].2.attributes["source_location"], "L2");
    }
}

struct LabelReplies {
    replies: Vec<String>,
    requests: Mutex<Vec<llm::Request>>,
}
impl llm::Transport for LabelReplies {
    fn send(&self, request: &llm::Request) -> llm::Result<Value> {
        let mut calls = self.requests.lock().unwrap();
        let index = calls.len();
        calls.push(request.clone());
        let text = self
            .replies
            .get(index)
            .ok_or_else(|| llm::Error::runtime("unexpected extra request"))?;
        Ok(
            json!({"choices":[{"message":{"content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}),
        )
    }
}

#[test]
fn truncated_label_response_retries_only_missing_ids_and_counts_usage() {
    let transport = Arc::new(LabelReplies {
        replies: vec![
            "{\"1\":\"Authentication\",\"2\":".into(),
            "{\"2\":\"Billing\"}".into(),
            "{\"3\":\"Storage\"}".into(),
        ],
        requests: Mutex::new(vec![]),
    });
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport.clone(),
    );
    let mut usage = llm::Usage::default();
    let result = runtime
        .label_batch(
            &[1, 2, 3],
            &[
                "1: auth_symbols".into(),
                "2: billing_symbols".into(),
                "3: storage_symbols".into(),
            ],
            llm_labels::BatchOptions {
                backend: "openai",
                model: Some("fixture-model"),
                depth: 0,
                max_depth: 3,
            },
            &mut usage,
            &mut build::Diagnostics::default(),
        )
        .unwrap();
    assert_eq!(
        result,
        json!({"1":"Authentication","2":"Billing","3":"Storage"})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!((usage.input, usage.output), (21, 6));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(!request.body.to_string().contains("auth_symbols"));
        assert_eq!(request.model, "fixture-model");
    }
}

#[test]
fn partial_labels_survive_a_failed_missing_id_retry() {
    let transport = Arc::new(LabelReplies {
        replies: vec![
            "{\"1\":\"Authentication\"}".into(),
            "not JSON".into(),
            "{\"3\":\"Reporting\"}".into(),
        ],
        requests: Mutex::new(vec![]),
    });
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport.clone(),
    );
    let mut usage = llm::Usage::default();
    let mut diagnostics = build::Diagnostics::default();
    let result = runtime
        .label_batch(
            &[1, 2, 3],
            &[
                "1: auth_symbols".into(),
                "2: billing".into(),
                "3: reports".into(),
            ],
            llm_labels::BatchOptions {
                backend: "openai",
                model: None,
                depth: 0,
                max_depth: 3,
            },
            &mut usage,
            &mut diagnostics,
        )
        .unwrap();
    assert_eq!(
        result,
        json!({"1":"Authentication", "3":"Reporting"})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!((usage.input, usage.output), (21, 6));
    assert!(
        diagnostics
            .stderr
            .iter()
            .any(|line| line.contains("unparseable"))
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1..]
            .iter()
            .all(|request| !request.body.to_string().contains("auth_symbols"))
    );
}

#[test]
fn label_retry_depth_is_bounded_and_shape_errors_are_typed() {
    let transport = Arc::new(LabelReplies {
        replies: vec!["{\"1\":\"Authentication\"}".into()],
        requests: Mutex::new(vec![]),
    });
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport.clone(),
    );
    let options = llm_labels::BatchOptions {
        backend: "openai",
        model: None,
        depth: 3,
        max_depth: 3,
    };
    let result = runtime
        .label_batch(
            &[1, 2],
            &["1: auth".into(), "2: bills".into()],
            options,
            &mut llm::Usage::default(),
            &mut build::Diagnostics::default(),
        )
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(
        runtime
            .label_batch(
                &[1],
                &[],
                options,
                &mut llm::Usage::default(),
                &mut build::Diagnostics::default()
            )
            .unwrap_err()
            .kind,
        llm::ErrorKind::Value
    );
}

/// The 0.9.73 `test_label_retry.py` fake: records the community ids named in each
/// prompt, replies `first` to the opening call, then labels every prompted id
/// unless the prompt names an `unparseable` one, which gets malformed JSON.
struct PromptLabels {
    first: Option<&'static str>,
    unparseable: Vec<i64>,
    prompts: Mutex<Vec<Vec<i64>>>,
}
impl llm::Transport for PromptLabels {
    fn send(&self, request: &llm::Request) -> llm::Result<Value> {
        let community = regex::Regex::new(r"Community (\d+):").unwrap();
        let ids: Vec<i64> = community
            .captures_iter(&request.body.to_string())
            .map(|found| found[1].parse().unwrap())
            .collect();
        let mut prompts = self.prompts.lock().unwrap();
        prompts.push(ids.clone());
        let text = match self.first {
            Some(first) if prompts.len() == 1 => first.to_owned(),
            _ if ids.iter().any(|id| self.unparseable.contains(id)) => {
                "{this is not valid json, missing quotes".to_owned()
            }
            _ => Value::Object(
                ids.iter()
                    .map(|id| (id.to_string(), json!(format!("Label {id}"))))
                    .collect(),
            )
            .to_string(),
        };
        Ok(
            json!({"choices":[{"message":{"content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}),
        )
    }
}

fn label_communities(
    transport: &Arc<PromptLabels>,
    cids: &[i64],
) -> llm::Result<Map<String, Value>> {
    let runtime = llm::Runtime::with_transport(
        llm::Configuration::builtins([("OPENAI_API_KEY".into(), "fixture".into())].into()),
        transport.clone(),
    );
    let lines: Vec<String> = cids
        .iter()
        .map(|cid| format!("Community {cid}: node_{cid}"))
        .collect();
    runtime.label_batch(
        cids,
        &lines,
        llm_labels::BatchOptions {
            backend: "openai",
            model: None,
            depth: 0,
            max_depth: 3,
        },
        &mut llm::Usage::default(),
        &mut build::Diagnostics::default(),
    )
}

#[test]
fn truncated_label_reply_keeps_salvaged_labels_when_a_missing_id_stays_unparseable() {
    let transport = Arc::new(PromptLabels {
        first: Some("{\"42\":\"Label 42\",\"99\":\"Label 99\""),
        unparseable: vec![137],
        prompts: Mutex::new(vec![]),
    });
    let labels = label_communities(&transport, &[42, 99, 137, 201]).unwrap();
    assert_eq!(
        labels.keys().cloned().collect::<BTreeSet<_>>(),
        ["201", "42", "99"].map(String::from).into()
    );
    assert_eq!(labels["42"], "Label 42");
    assert_eq!(labels["99"], "Label 99");
    assert_eq!(labels["201"], "Label 201");
    assert_eq!(
        *transport.prompts.lock().unwrap(),
        vec![vec![42, 99, 137, 201], vec![137], vec![201]]
    );
}

#[test]
fn unparseable_label_reply_bisects_and_keeps_the_halves_that_parse() {
    let transport = Arc::new(PromptLabels {
        first: Some("{this is not valid json, missing quotes"),
        unparseable: vec![137],
        prompts: Mutex::new(vec![]),
    });
    let labels = label_communities(&transport, &[42, 99, 137, 201]).unwrap();
    assert_eq!(
        labels.keys().cloned().collect::<BTreeSet<_>>(),
        ["201", "42", "99"].map(String::from).into()
    );
    assert_eq!(
        *transport.prompts.lock().unwrap(),
        vec![
            vec![42, 99, 137, 201],
            vec![42, 99],
            vec![137, 201],
            vec![137],
            vec![201]
        ]
    );
}

#[test]
fn label_batch_with_no_parseable_reply_still_raises() {
    let transport = Arc::new(PromptLabels {
        first: Some("{not json at all"),
        unparseable: vec![1, 2, 3],
        prompts: Mutex::new(vec![]),
    });
    assert_eq!(
        label_communities(&transport, &[1, 2, 3]).unwrap_err().kind,
        llm::ErrorKind::Value
    );
}

fn read_capped_files(names: &[(&str, usize)]) -> (String, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let units: Vec<_> = names
        .iter()
        .map(|(name, size)| {
            let path = directory.path().join(name);
            fs::write(&path, "a".repeat(*size)).unwrap();
            crate::file_slice::Unit::Path(path)
        })
        .collect();
    let mut diagnostics = build::Diagnostics::default();
    let text = crate::llm_source::read_files(
        &units,
        directory.path(),
        &mut crate::llm_source::TokenPacker::default(),
        &mut diagnostics,
    );
    (text, diagnostics.stderr)
}

#[test]
fn reading_a_file_over_the_character_cap_warns_with_its_relative_path() {
    let cap = crate::llm_source::FILE_CHAR_CAP;
    let (text, stderr) = read_capped_files(&[("oversized.txt", cap + 500)]);
    assert!(!text.is_empty());
    assert!(!text.contains(&"a".repeat(cap + 1)));
    assert_eq!(
        stderr,
        [format!(
            "[graphicle] WARNING: file oversized.txt exceeds {cap} characters and was truncated"
        )]
    );
}

#[test]
fn reading_a_file_under_the_character_cap_does_not_warn() {
    let (text, stderr) = read_capped_files(&[("small.txt", 1000)]);
    assert!(!text.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn reading_several_oversized_files_warns_once() {
    let cap = crate::llm_source::FILE_CHAR_CAP;
    let (text, stderr) = read_capped_files(&[
        ("large_0.txt", cap + 1000),
        ("large_1.txt", cap + 1000),
        ("large_2.txt", cap + 1000),
    ]);
    assert!(!text.is_empty());
    assert_eq!(
        stderr
            .iter()
            .filter(|line| line.contains("was truncated"))
            .count(),
        1
    );
}

#[test]
fn missing_anonymous_parser_token_reports_its_containing_line() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example.kt");
    let source = "package app\nclass Item\nclass Worker { fun run(x: Item): Item { return helper(x) }\n fun helper(x: Item): Item { return x } }\n";
    for suffix in ["", "\n// π 🧪\n"] {
        std::fs::write(&path, format!("{source}{suffix}")).unwrap();
        let parsed = crate::extract::extract_file(&path, None);
        assert_eq!(
            parsed["parse_errors"],
            json!({"first_error_line":3,"multiline_error":false})
        );
    }
}

#[test]
fn malformed_requests_do_not_unwind_and_do_not_become_successes() {
    for request in [
        Value::Null,
        json!([]),
        json!({"operation":"absent"}),
        json!({"operation":"graph","graph":[]}),
    ] {
        let result = std::panic::catch_unwind(|| crate::dispatch(&request)).unwrap();
        assert!(result.is_err());
        assert_ne!(result.unwrap_err().kind(), crate::ErrorKind::Internal);
    }
    let orders: BTreeSet<_> = (0..12)
        .map(|_| graph().to_node_link("links").to_string())
        .collect();
    assert_eq!(orders.len(), 1);
}

#[test]
fn mcp_recovers_after_malformed_and_oversized_frames() {
    let input = format!(
        "{}\nnot json\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}}\n",
        "x".repeat(256)
    );
    let server = serve::Server::new(std::path::Path::new("unused-for-ping.json"));
    let mut output = Vec::new();
    serve::stdio_with_io(&server, &mut std::io::Cursor::new(input), &mut output, 96).unwrap();
    let replies: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[2], json!({"jsonrpc":"2.0","id":1,"result":{}}));
}

#[test]
fn installer_rejects_conflicting_platforms_and_unknown_options_before_writes() {
    for arguments in [
        vec!["install", "claude", "--platform", "codex"],
        vec!["install", "--typo=one"],
        vec!["install", "--purge"],
        vec!["uninstall", "--strict"],
    ] {
        let result = crate::cli::run(&arguments.into_iter().map(str::to_owned).collect::<Vec<_>>());
        assert_ne!(result.exit_code, 0);
        assert!(result.stderr.contains("error:"));
    }
}
