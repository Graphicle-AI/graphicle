//! Mirrors `tests/test_graphdb_push_indexes.py`, added upstream in Graphify
//! 0.9.73 (#3804): graph DB pushes index every MERGE label on `(n.id)` before
//! the upsert loop, and FalkorDB (no `IF NOT EXISTS`) tolerates the error an
//! already-indexed graph returns. Upstream records Cypher through fake SDK
//! clients; here the real RESP and Bolt transports talk to loopback fixtures.
use graphicle::{
    cluster::Communities,
    exporters::graphdb::{
        PushCounts, distinct_node_labels, falkordb_query_plan, push_to_falkordb, push_to_neo4j,
        query_plan,
    },
    graph::Graph,
};
use serde_json::{Map, Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

type Calls = Arc<Mutex<Vec<String>>>;

fn attrs(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

/// Upstream `_graph()`: two labels share "Code", so per-label indexing shows.
fn sample_graph() -> Graph {
    let mut graph = Graph::new(false, false);
    for (id, label, file_type, source) in [
        ("parse_config", "parse_config()", "code", "a.py"),
        ("load_settings", "load_settings()", "code", "a.py"),
        ("sftp_service", "SFTP service", "document", "b.md"),
        ("payroll_db", "Payroll DB", "database", "b.md"),
    ] {
        let node = attrs(json!({"label": label, "file_type": file_type, "source_file": source}));
        graph.add_node(json!(id), node).unwrap();
    }
    for (u, v) in [
        ("parse_config", "sftp_service"),
        ("load_settings", "payroll_db"),
    ] {
        graph
            .add_edge(
                json!(u),
                json!(v),
                None,
                attrs(json!({"relation": "reads"})),
            )
            .unwrap();
    }
    graph
}

fn no_communities() -> Communities {
    vec![]
}

fn indexes(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with("CREATE INDEX"))
        .collect()
}

// FalkorDB upserts carry a `CYPHER key=value ...` parameter header first.
fn first_merge(calls: &[String]) -> usize {
    calls
        .iter()
        .position(|c| c.contains("MERGE"))
        .expect("expected the push to still upsert nodes")
}

fn count_containing(calls: &[String], fragment: &str) -> usize {
    calls.iter().filter(|c| c.contains(fragment)).count()
}

fn serve(handler: fn(TcpStream, Calls, bool), reject_index: bool) -> (u16, Calls) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Calls::default();
    let recorded = calls.clone();
    // Calls are recorded before each reply, so they are complete once a push
    // returns; the accept loop is left to end with the test process.
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let recorded = recorded.clone();
            thread::spawn(move || handler(stream, recorded, reject_index));
        }
    });
    (port, calls)
}

fn resp_command(reader: &mut BufReader<TcpStream>) -> Option<Vec<String>> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let count = line.trim_end().strip_prefix('*')?.parse::<usize>().ok()?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let size = line.trim_end().strip_prefix('$')?.parse::<usize>().ok()?;
        let mut data = vec![0; size + 2];
        reader.read_exact(&mut data).ok()?;
        data.truncate(size);
        args.push(String::from_utf8(data).ok()?);
    }
    Some(args)
}

fn falkordb_fixture(stream: TcpStream, calls: Calls, reject_index: bool) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(args) = resp_command(&mut reader) {
        let reply: &[u8] = if args[0].eq_ignore_ascii_case("GRAPH.QUERY") {
            assert_eq!(
                (args[1].as_str(), args[3].as_str()),
                ("graphicle", "--compact")
            );
            calls.lock().unwrap().push(args[2].clone());
            if reject_index && args[2].starts_with("CREATE INDEX") {
                b"-ERR Attribute 'id' is already indexed\r\n"
            } else {
                b"*1\r\n*0\r\n"
            }
        } else {
            b"+OK\r\n"
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn falkordb_push(
    graph: &Graph,
    reject_index: bool,
) -> (graphicle::Result<PushCounts>, Vec<String>) {
    let (port, calls) = serve(falkordb_fixture, reject_index);
    let uri = format!("redis://127.0.0.1:{port}");
    let result = push_to_falkordb(graph, &uri, None, None, &no_communities(), "graphicle");
    let calls = calls.lock().unwrap().clone();
    (result, calls)
}

fn bolt_message(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut message = vec![];
    loop {
        let mut size = [0; 2];
        stream.read_exact(&mut size).ok()?;
        let size = u16::from_be_bytes(size) as usize;
        if size == 0 {
            if message.is_empty() {
                continue;
            }
            return Some(message);
        }
        let mut chunk = vec![0; size];
        stream.read_exact(&mut chunk).ok()?;
        message.extend(chunk);
    }
}

fn packstream_string(data: &[u8]) -> String {
    let (size, start) = match data[0] {
        marker @ 0x80..=0x8F => ((marker & 0x0F) as usize, 1),
        0xD0 => (data[1] as usize, 2),
        0xD1 => (u16::from_be_bytes([data[1], data[2]]) as usize, 3),
        0xD2 => (
            u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize,
            5,
        ),
        marker => panic!("unexpected PackStream marker {marker:#x}"),
    };
    String::from_utf8(data[start..start + size].to_vec()).unwrap()
}

fn packstream_text(text: &str) -> Vec<u8> {
    let mut out = if text.len() < 16 {
        vec![0x80 | text.len() as u8]
    } else {
        vec![0xD0, text.len() as u8]
    };
    out.extend(text.as_bytes());
    out
}

fn bolt_reply(stream: &mut TcpStream, tag: u8, fields: &[(&str, Vec<u8>)]) -> bool {
    let mut body = vec![0xB1, tag, 0xA0 | fields.len() as u8];
    for (key, value) in fields {
        body.extend(packstream_text(key));
        body.extend(value);
    }
    let mut frame = (body.len() as u16).to_be_bytes().to_vec();
    frame.extend(body);
    frame.extend([0, 0]);
    stream.write_all(&frame).is_ok()
}

/// Minimal Bolt 4.4 server: HELLO, RUN, PULL/DISCARD, RESET and GOODBYE.
fn neo4j_fixture(mut stream: TcpStream, calls: Calls, reject_index: bool) {
    let mut handshake = [0; 20];
    stream.read_exact(&mut handshake).unwrap();
    assert_eq!(handshake[..4], [0x60, 0x60, 0xB0, 0x17]);
    assert!(handshake[4..].chunks(4).any(|v| v == [0, 0, 4, 4]));
    stream.write_all(&[0, 0, 4, 4]).unwrap();
    let text = packstream_text;
    while let Some(message) = bolt_message(&mut stream) {
        let ok = match message[1] {
            0x01 => bolt_reply(
                &mut stream,
                0x70,
                &[
                    ("server", text("Neo4j/5.26.0")),
                    ("connection_id", text("bolt-fixture")),
                ],
            ),
            0x10 => {
                let query = packstream_string(&message[2..]);
                let rejected = reject_index && query.starts_with("CREATE INDEX");
                calls.lock().unwrap().push(query);
                if rejected {
                    bolt_reply(
                        &mut stream,
                        0x7F,
                        &[
                            ("code", text("Neo.ClientError.Schema.IndexAlreadyExists")),
                            ("message", text("staged index failure")),
                        ],
                    )
                } else {
                    bolt_reply(
                        &mut stream,
                        0x70,
                        &[
                            ("fields", vec![0x90]),
                            ("t_first", vec![1]),
                            ("qid", vec![0]),
                        ],
                    )
                }
            }
            0x2F | 0x3F => {
                let stats = [vec![0xA1], text("nodes-created"), vec![1]].concat();
                bolt_reply(
                    &mut stream,
                    0x70,
                    &[
                        ("type", text("w")),
                        ("t_last", vec![1]),
                        ("db", text("neo4j")),
                        ("stats", stats),
                    ],
                )
            }
            0x0F => bolt_reply(&mut stream, 0x70, &[]),
            0x02 => return,
            tag => {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("unexpected Bolt message {tag:#x}"));
                return;
            }
        };
        if !ok {
            return;
        }
    }
}

fn neo4j_push(graph: &Graph, reject_index: bool) -> (graphicle::Result<PushCounts>, Vec<String>) {
    let (port, calls) = serve(neo4j_fixture, reject_index);
    let uri = format!("bolt://127.0.0.1:{port}");
    let result = push_to_neo4j(graph, &uri, "neo4j", "fixture", &no_communities());
    let calls = calls.lock().unwrap().clone();
    (result, calls)
}

#[test]
fn falkordb_push_indexes_each_label_before_upserting() {
    let (result, calls) = falkordb_push(&sample_graph(), false);
    assert_eq!(result.unwrap(), PushCounts { nodes: 4, edges: 2 });
    assert_eq!(
        indexes(&calls),
        [
            "CREATE INDEX FOR (n:Code) ON (n.id)",
            "CREATE INDEX FOR (n:Database) ON (n.id)",
            "CREATE INDEX FOR (n:Document) ON (n.id)",
        ]
    );
    let last_index = calls
        .iter()
        .rposition(|c| c.starts_with("CREATE INDEX"))
        .unwrap();
    assert!(last_index < first_merge(&calls), "{calls:#?}");
}

#[test]
fn falkordb_push_indexes_before_the_upsert_loop_runs() {
    let (result, calls) = falkordb_push(&sample_graph(), false);
    result.unwrap();
    let mut seen_index = std::collections::BTreeSet::new();
    for cypher in &calls {
        if let Some(rest) = cypher.strip_prefix("CREATE INDEX FOR (n:") {
            seen_index.insert(rest.split(')').next().unwrap().to_owned());
        } else if let Some(rest) = cypher.split("MERGE (n:").nth(1) {
            let label = rest.split(' ').next().unwrap();
            assert!(
                seen_index.contains(label),
                "MERGE on unindexed label {label}: {cypher}"
            );
        }
    }
}

#[test]
fn falkordb_push_survives_an_existing_index() {
    let (result, calls) = falkordb_push(&sample_graph(), true);
    assert_eq!(result.unwrap(), PushCounts { nodes: 4, edges: 2 });
    assert_eq!(count_containing(&calls, "MERGE (n:"), 4);
    assert_eq!(count_containing(&calls, "CREATE INDEX"), 3);
}

#[test]
fn falkordb_index_statements_send_no_parameter_header() {
    let plan = falkordb_query_plan(&sample_graph(), &no_communities()).unwrap();
    assert!(plan[..3].iter().all(|s| s.params.is_null()));
    let (_, calls) = falkordb_push(&sample_graph(), false);
    assert!(
        calls[..3]
            .iter()
            .all(|c| c.starts_with("CREATE INDEX FOR (n:"))
    );
    assert!(
        calls[3..].iter().all(|c| c.starts_with("CYPHER ")),
        "{calls:#?}"
    );
}

#[test]
fn falkordb_push_rejects_a_non_string_file_type_before_any_query() {
    let mut graph = sample_graph();
    graph
        .add_node(json!("bad"), attrs(json!({"label": "bad", "file_type": 3})))
        .unwrap();
    let (result, calls) = falkordb_push(&graph, false);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("file_type must be a string")
    );
    assert!(calls.is_empty(), "{calls:#?}");
}

#[test]
fn neo4j_push_indexes_each_label_before_upserting() {
    let (result, calls) = neo4j_push(&sample_graph(), false);
    assert_eq!(result.unwrap(), PushCounts { nodes: 4, edges: 2 });
    assert_eq!(
        indexes(&calls),
        [
            "CREATE INDEX IF NOT EXISTS FOR (n:Code) ON (n.id)",
            "CREATE INDEX IF NOT EXISTS FOR (n:Database) ON (n.id)",
            "CREATE INDEX IF NOT EXISTS FOR (n:Document) ON (n.id)",
        ]
    );
    let last_index = calls
        .iter()
        .rposition(|c| c.starts_with("CREATE INDEX"))
        .unwrap();
    assert!(last_index < first_merge(&calls), "{calls:#?}");
    let plan = query_plan(&sample_graph(), &no_communities()).unwrap();
    let planned = plan.iter().map(|s| s.query.clone()).collect::<Vec<_>>();
    assert_eq!(calls, planned);
}

#[test]
fn neo4j_push_tolerates_a_failed_index_statement() {
    let (result, calls) = neo4j_push(&sample_graph(), true);
    assert_eq!(result.unwrap(), PushCounts { nodes: 4, edges: 2 });
    assert_eq!(count_containing(&calls, "CREATE INDEX"), 3);
    assert_eq!(count_containing(&calls, "MERGE (n:"), 4);
}

#[test]
fn distinct_node_labels_sanitizes_like_the_push_loop() {
    let mut graph = Graph::new(false, false);
    for (id, file_type) in [
        ("a", Some("code")),
        ("b", Some("Code")),
        ("c", Some("!!!")),
        ("d", None),
    ] {
        let mut node = Map::new();
        if let Some(file_type) = file_type {
            node.insert("file_type".into(), json!(file_type));
        }
        graph.add_node(json!(id), node).unwrap();
    }
    assert_eq!(distinct_node_labels(&graph).unwrap(), ["Code", "Entity"]);
    assert_eq!(
        distinct_node_labels(&sample_graph()).unwrap(),
        ["Code", "Database", "Document"]
    );
}
