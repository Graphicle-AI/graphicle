//! Order parity of the graph substrate with the implementation it replaced
//! (memory program, lane B2). Random graphs go through every mutation,
//! lookup, copy, node-link round trip and query-snapshot encoding, and all
//! that is observable goes into one transcript. `TRANSCRIPT` is the digest
//! the earlier implementation produced (mem-waveA, de1c945: owned-key node
//! index, an edge index on every graph, usize adjacency rows). Set
//! `GRAPH_PARITY_TRANSCRIPT=<file>` to write the transcript for a diff.
use super::Graph;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

const TRANSCRIPT: &str = "59ba82bb801290ccff413aa88873d17888d44435d68021001bd0a87c5edf632a";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn attrs(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// Identifiers that collide the way Python keys do: `1`, `1.0` and `true`
/// are one node, and so are equal tuples. A large pool also has a hub, `n0`,
/// with enough neighbours for indexed rows.
fn pick_id(rng: &mut XorShiftRng, pool: usize) -> Value {
    if pool > 10 && rng.gen_bool(0.3) {
        return json!("n0");
    }
    match rng.gen_range(0..14) {
        0 => json!(rng.gen_range(0..4)),
        1 => json!(f64::from(rng.gen_range(0..4))),
        2 => json!(rng.gen_bool(0.5)),
        3 => json!([format!("t{}", rng.gen_range(0..2)), rng.gen_range(0..2)]),
        4 => json!([
            format!("t{}", rng.gen_range(0..2)),
            f64::from(rng.gen_range(0..2))
        ]),
        5 => json!(2.5),
        _ => json!(format!("n{}", rng.gen_range(0..pool))),
    }
}

fn pick_key(rng: &mut XorShiftRng) -> Option<Value> {
    match rng.gen_range(0..12) {
        0..=3 => None,
        4 => Some(Value::Null),
        5 => Some(json!(rng.gen_range(0..3))),
        6 => Some(json!(1.0)),
        7 => Some(json!(true)),
        8 => Some(json!(["k"])),
        9 => Some(json!({"k": 1})),
        _ => Some(json!(["same", "other", "x"][rng.gen_range(0..3)])),
    }
}

fn shuffle<T>(items: &mut [T], rng: &mut XorShiftRng) {
    for i in (1..items.len()).rev() {
        items.swap(i, rng.gen_range(0..=i));
    }
}

fn record<T: std::fmt::Debug>(out: &mut String, what: &str, result: crate::Result<T>) {
    match result {
        Ok(value) => writeln!(out, "{what} ok {value:?}"),
        Err(error) => writeln!(out, "{what} err {error}"),
    }
    .unwrap();
}

fn id_text(graph: &Graph, i: usize) -> String {
    serde_json::to_string(&graph.nodes[i].id).unwrap()
}

/// Node-link document, counts and `iter_edges` order with every record.
fn light(graph: &Graph, out: &mut String) {
    let link = serde_json::to_string(&graph.to_node_link("links")).unwrap();
    let edges: Vec<_> = graph
        .iter_edges()
        .map(|(u, v, e)| {
            format!(
                "{u}-{v}:{}:{}:{}",
                e.key,
                serde_json::to_string(&e.attributes).unwrap(),
                (e.source, e.target) == (u, v)
            )
        })
        .collect();
    writeln!(
        out,
        "link {link}\ncount {} {}\niter {edges:?}",
        graph.node_count(),
        graph.edge_count()
    )
    .unwrap();
}

/// Everything observable about a graph: `light`, every row view, pairwise
/// counts, identifier lookups, `edges_within` and the query-snapshot bytes.
fn full(graph: &Graph, out: &mut String) {
    light(graph, out);
    let n = graph.node_count();
    for i in 0..n {
        let outgoing: Vec<_> = graph
            .out_edges(i)
            .into_iter()
            .map(|(_, v, e)| format!("{}:{}", id_text(graph, v), e.key))
            .collect();
        let incoming: Vec<_> = graph
            .in_edges(i)
            .into_iter()
            .map(|(u, _, e)| format!("{}:{}", id_text(graph, u), e.key))
            .collect();
        let neighbors: Vec<_> = graph.neighbor_indices(i, false).collect();
        let predecessors: Vec<_> = graph.neighbor_indices(i, true).collect();
        let between: Vec<_> = (0..n)
            .map(|j| graph.number_of_edges(&graph.nodes[i].id, &graph.nodes[j].id))
            .collect();
        let keys: Vec<_> = (0..n)
            .filter_map(|j| {
                let edges = graph.edges_between(&graph.nodes[i].id, &graph.nodes[j].id);
                (!edges.is_empty()).then(|| {
                    let keys: Vec<_> = edges.iter().map(|e| e.key.to_string()).collect();
                    format!("{j}={}", keys.join("|"))
                })
            })
            .collect();
        writeln!(
            out,
            "row {} out {outgoing:?} in {incoming:?} nb {neighbors:?} pr {predecessors:?} \
             between {between:?} keys {keys:?}",
            id_text(graph, i)
        )
        .unwrap();
    }
    let even: Vec<_> = (0..n).filter(|i| i % 2 == 0).collect();
    write!(
        out,
        "within {} lookups",
        graph.edges_within(&even, |v| v % 2 == 0)
    )
    .unwrap();
    for probe in [
        json!(0),
        json!(0.0),
        json!(1),
        json!(1.0),
        json!(true),
        json!(false),
        json!(2.5),
        json!("n1"),
        json!("n39"),
        json!(["t0", 1]),
        json!(["t0", 1.0]),
        json!(["t1", true]),
        Value::Null,
        json!({"x": 1}),
        json!([["t0"]]),
    ] {
        write!(out, " {:?}", graph.node_index(&probe)).unwrap();
    }
    let index = crate::serve_search::SearchIndex::new(graph);
    let bytes = crate::snapshot::encode(b"", graph, &index);
    writeln!(out, "\nsnapshot {}", hex(&Sha256::digest(&bytes))).unwrap();
}

/// A few edits that go through every index of `graph`.
fn edit(graph: &mut Graph, rng: &mut XorShiftRng, out: &mut String) {
    let n = graph.node_count();
    if n < 2 {
        return;
    }
    let (u, v) = (rng.gen_range(0..n), rng.gen_range(0..n));
    let (a, b) = (graph.nodes[u].id.clone(), graph.nodes[v].id.clone());
    record(
        out,
        "edit add",
        graph.add_edge(a.clone(), b.clone(), None, attrs(json!({"edit": true}))),
    );
    record(
        out,
        "edit add",
        graph.add_edge(
            b.clone(),
            a.clone(),
            Some(json!(0)),
            attrs(json!({"edit": 2})),
        ),
    );
    record(out, "edit remove", graph.remove_edge(&a, &b, None));
    record(out, "edit node", graph.add_node(json!("fresh"), Map::new()));
    record(
        out,
        "edit add",
        graph.add_edge(json!("fresh"), a, None, attrs(json!({"edit": 3}))),
    );
    full(graph, out);
}

fn scenario(rng: &mut XorShiftRng, round: usize, out: &mut String) {
    let (directed, multigraph) = (round.is_multiple_of(2), round % 4 < 2);
    // Every fifth round is large enough for indexed (hub) rows.
    let (pool, steps) = if round.is_multiple_of(5) {
        (60, 500)
    } else {
        (10, 60)
    };
    let mut graph = Graph::new(directed, multigraph);
    writeln!(
        out,
        "round {round} directed {directed} multigraph {multigraph}"
    )
    .unwrap();
    for step in 0..rng.gen_range(1..steps) {
        let n = graph.node_count();
        match rng.gen_range(0..100) {
            0..=9 => {
                let id = pick_id(rng, pool);
                let fields = attrs(json!({"label": format!("L{step}"), "w": step % 3}));
                record(out, "add_node", graph.add_node(id, fields));
            }
            10..=56 => {
                let (s, t) = (pick_id(rng, pool), pick_id(rng, pool));
                let key = pick_key(rng);
                let relation = ["calls", "imports", "contains"][step % 3];
                let fields = attrs(json!({"relation": relation, "i": step}));
                record(out, "add_edge", graph.add_edge(s, t, key, fields));
            }
            57..=68 => {
                let (s, t) = (pick_id(rng, pool), pick_id(rng, pool));
                let key = pick_key(rng);
                record(out, "remove_edge", graph.remove_edge(&s, &t, key.as_ref()));
            }
            69..=70 => {
                let ids = vec![pick_id(rng, pool), pick_id(rng, pool)];
                record(out, "remove_nodes", graph.remove_nodes_from(&ids));
            }
            71..=74 => {
                let (s, t) = (pick_id(rng, pool), pick_id(rng, pool));
                let key = pick_key(rng);
                let touched = graph
                    .edge_attributes_mut(&s, &t, key.as_ref())
                    .map(|fields| fields.insert("touched".into(), json!(step)));
                writeln!(out, "touch {touched:?}").unwrap();
            }
            75..=77 if n > 0 => {
                let i = rng.gen_range(0..n);
                let mut order: Vec<_> = graph.neighbor_indices(i, false).collect();
                shuffle(&mut order, rng);
                if rng.gen_bool(0.1) {
                    order.pop();
                }
                let ids: Vec<_> = order.iter().map(|&j| graph.nodes[j].id.clone()).collect();
                let rows = json!([[graph.nodes[i].id.clone(), ids]]);
                record(out, "neighbor order", graph.restore_neighbor_order(&rows));
            }
            78..=80 if n > 0 => {
                let i = rng.gen_range(0..n);
                let mut order = Vec::new();
                for (u, _, _) in graph.in_edges(i) {
                    if order.last() != Some(&u) {
                        order.push(u);
                    }
                }
                shuffle(&mut order, rng);
                let ids: Vec<_> = order.iter().map(|&j| graph.nodes[j].id.clone()).collect();
                let rows = json!([[graph.nodes[i].id.clone(), ids]]);
                record(
                    out,
                    "predecessor order",
                    graph.restore_predecessor_order(&rows),
                );
            }
            81 => match graph.to_undirected() {
                Ok(copy) => graph = copy,
                Err(error) => writeln!(out, "undirected err {error}").unwrap(),
            },
            82 => {
                let mapping = vec![
                    (pick_id(rng, pool), pick_id(rng, pool)),
                    (pick_id(rng, pool), pick_id(rng, pool)),
                ];
                match graph.relabel_copy(&mapping) {
                    Ok(copy) => graph = copy,
                    Err(error) => writeln!(out, "relabel err {error}").unwrap(),
                }
            }
            83..=85 => {
                let keep: Vec<_> = (0..n + 2).filter(|_| rng.gen_bool(0.8)).collect();
                match graph.induced_subgraph(&keep) {
                    Ok(copy) => graph = copy,
                    Err(error) => writeln!(out, "induced err {error}").unwrap(),
                }
            }
            86..=88 => {
                let data = graph.to_node_link("links");
                match Graph::from_node_link(&data, "links") {
                    Ok(copy) => graph = copy,
                    Err(error) => writeln!(out, "reload err {error}").unwrap(),
                }
            }
            89..=91 => {
                let index = crate::serve_search::SearchIndex::new(&graph);
                let bytes = crate::snapshot::encode(b"", &graph, &index);
                match crate::snapshot::decode(&bytes[8..]) {
                    Ok((copy, _)) => graph = copy,
                    Err(error) => writeln!(out, "snapshot err {error}").unwrap(),
                }
            }
            _ => {
                let (s, t) = (pick_id(rng, pool), pick_id(rng, pool));
                let keys: Vec<_> = graph
                    .edges_between(&s, &t)
                    .iter()
                    .map(|e| e.key.to_string())
                    .collect();
                writeln!(out, "between {keys:?} {}", graph.number_of_edges(&s, &t)).unwrap();
            }
        }
    }
    full(&graph, out);
    let n = graph.node_count();
    // Copies, and edits on them.
    light(&graph.without_node_attributes(), out);
    light(&graph.topology_with_edge_attribute("relation"), out);
    let keep: Vec<_> = (0..n).filter(|_| rng.gen_bool(0.6)).collect();
    match graph.induced_subgraph(&keep) {
        Ok(mut copy) => {
            full(&copy, out);
            edit(&mut copy, rng, out);
        }
        Err(error) => writeln!(out, "induced err {error}").unwrap(),
    }
    match graph.induced_subgraph_with(&keep, Some("relation")) {
        Ok(copy) => light(&copy, out),
        Err(error) => writeln!(out, "induced err {error}").unwrap(),
    }
    match graph.to_undirected() {
        Ok(mut copy) => {
            full(&copy, out);
            edit(&mut copy, rng, out);
        }
        Err(error) => writeln!(out, "undirected err {error}").unwrap(),
    }
    // Node-link round trips through every loader.
    let data = graph.to_node_link("links");
    let text = serde_json::to_string(&data).unwrap();
    for (name, loaded) in [
        ("borrowed", Graph::from_node_link(&data, "links")),
        ("owned", Graph::from_node_link_owned(data.clone(), "links")),
        ("arcs", Graph::from_node_link_with_arcs(&data, "links")),
        (
            "owned arcs",
            Graph::from_node_link_owned_with_arcs(data.clone(), "links"),
        ),
    ] {
        match loaded {
            Ok(loaded) => {
                writeln!(out, "{name}").unwrap();
                light(&loaded, out);
            }
            Err(error) => writeln!(out, "{name} err {error}").unwrap(),
        }
    }
    match Graph::from_node_link_json(&text, "links") {
        Some(mut loaded) => {
            full(&loaded, out);
            edit(&mut loaded, rng, out);
        }
        None => writeln!(out, "streamed declined").unwrap(),
    }
    // The query snapshot decodes to the same graph, which edits the same way.
    let index = crate::serve_search::SearchIndex::new(&graph);
    let bytes = crate::snapshot::encode(b"", &graph, &index);
    match crate::snapshot::decode(&bytes[8..]) {
        Ok((mut back, _)) => {
            full(&back, out);
            edit(&mut back, rng, out);
        }
        Err(error) => writeln!(out, "snapshot err {error}").unwrap(),
    }
    edit(&mut graph, rng, out);
}

#[test]
fn graph_behaviour_matches_the_recorded_transcript() {
    let mut rng = XorShiftRng::seed_from_u64(20261015);
    let mut out = String::new();
    for round in 0..240 {
        scenario(&mut rng, round, &mut out);
    }
    // The large rounds must reach the indexed rows (16 or more neighbours).
    let widest = out
        .lines()
        .filter_map(|line| line.split(" nb [").nth(1))
        .map(|rest| rest.split(']').next().unwrap_or("").split(',').count())
        .max()
        .unwrap_or(0);
    assert!(widest >= 24, "widest row {widest}");
    if let Some(path) = std::env::var_os("GRAPH_PARITY_TRANSCRIPT") {
        std::fs::write(path, &out).unwrap();
    }
    let digest = hex(&Sha256::digest(out.as_bytes()));
    assert_eq!(digest, TRANSCRIPT, "graph transcript changed");
}
