//! Gates for selectively adopting retrieval mechanisms. A disabled mechanism
//! must contribute no candidates, or its measured benefit is confounded.
use graphicle_memory::text;
use graphicle_runtime::query::{self, Snapshot};
use serde_json::{Value, json};

fn fixture(edges: bool) -> Snapshot {
    let document = |path: &str, content: &str| {
        json!({"id":format!("file:{path}"),"path":path,"title":path,
            "content":content,"sha256":text::sha256(content.as_bytes()),
            "source_kind":"repository_file","recorded_at":null})
    };
    Snapshot {
        repository: json!({"id":"selective-integration","name":"Mechanism isolation"}),
        documents: vec![
            document("entry.rs", "Heron requests enter here."),
            document("worker.rs", "The rotating duty is handled by Mira."),
        ],
        memories: vec![],
        graph: json!({"edges":if edges {
            vec![json!({"source":"entry.rs","target":"worker.rs","kind":"calls"})]
        } else {vec![]}}),
        state: "fixture-v1".into(),
    }
}

fn context(snap: &Snapshot, enabled: bool, rankings: Value) -> Value {
    query::context(
        snap,
        &json!({"question":"heron","graph_retrieval":enabled,
            "max_tokens":1000,"limit":2,"expand_context":false}),
        rankings,
    )
    .unwrap()
}

fn contains_worker(packet: &Value) -> bool {
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == "file:worker.rs")
}

fn worker_passage(snap: &Snapshot) -> Value {
    let (rows, _) = query::candidates(snap).unwrap();
    rows.into_iter()
        .find(|r| r["source"]["id"] == "file:worker.rs")
        .unwrap()["identity"]
        .clone()
}

#[test]
fn disabling_graph_removes_static_neighbor_evidence() {
    let snap = fixture(true);
    assert!(contains_worker(&context(&snap, true, json!({}))));
    let disabled = context(&snap, false, json!({}));
    assert!(
        !contains_worker(&disabled),
        "Graph-off must remove a source reachable only through a static edge: {disabled}"
    );
}

#[test]
fn disabling_graph_ignores_a_precomputed_graph_arm() {
    let snap = fixture(false);
    let ranks = json!({"graph_local":[worker_passage(&snap)]});
    assert!(contains_worker(&context(&snap, true, ranks.clone())));
    let disabled = context(&snap, false, ranks);
    assert!(
        !contains_worker(&disabled),
        "A precomputed graph arm cannot bypass the mechanism switch: {disabled}"
    );
}

#[test]
fn disabling_graph_preserves_independent_semantic_evidence() {
    let snap = fixture(true);
    let disabled = context(&snap, false, json!({"semantic":[worker_passage(&snap)]}));
    assert!(contains_worker(&disabled));
    for e in disabled["evidence"].as_array().unwrap() {
        let doc = snap.documents.iter().find(|d| d["id"] == e["id"]).unwrap();
        assert_eq!(e["sha256"], doc["sha256"]);
        assert_eq!(
            e["text"].as_str().unwrap(),
            &doc["content"].as_str().unwrap()[e["byte_start"].as_u64().unwrap() as usize
                ..e["byte_end"].as_u64().unwrap() as usize]
        );
    }
}

/// The default: a dense arm counts `SEMANTIC_WEIGHT` (0.5) in the fusion
/// unless the caller gives `arm_weights` (reported on the packet).
#[test]
fn the_dense_arm_counts_half_unless_weights_are_given() {
    let snap = fixture(false);
    let first = |weights: Option<Value>, rankings: Value| {
        let mut args =
            json!({"question":"heron","max_tokens":1000,"limit":1,"expand_context":false});
        if let Some(weights) = weights {
            args["arm_weights"] = weights;
        }
        let packet = query::context(&snap, &args, rankings).unwrap();
        (
            packet["evidence"][0]["id"].clone(),
            packet["arm_weights"].clone(),
        )
    };
    // Lexical ranks only entry.rs; the dense arm ranks only worker.rs.
    let dense = json!({"semantic":[worker_passage(&snap)]});
    assert_eq!(query::SEMANTIC_WEIGHT, 0.5);
    assert_eq!(
        first(None, dense.clone()),
        (json!("file:entry.rs"), json!({"semantic": 0.5}))
    );
    assert_eq!(
        first(Some(json!({"semantic": 2.0})), dense.clone()),
        (json!("file:worker.rs"), json!({"semantic": 2.0}))
    );
    assert_eq!(first(None, json!({})), (json!("file:entry.rs"), json!({})));
}

/// `include_tests`: `false` moves test files behind the other files, `"auto"`
/// (the default) does so unless the question names a test file or a test
/// identifier, and `true` keeps the fused order. The packet's `test_files`
/// reports what applied; the move keeps every window.
#[test]
fn test_files_follow_the_code_unless_the_question_names_tests() {
    let document = |path: &str, content: &str| {
        json!({"id":format!("file:{path}"),"path":path,"title":path,
            "content":content,"sha256":text::sha256(content.as_bytes()),
            "source_kind":"repository_file","recorded_at":null})
    };
    let snap = Snapshot {
        repository: json!({"id":"test-files","name":"Test files"}),
        documents: vec![
            document(
                "tests/test_parser.py",
                "from parser import parse_header\n\n\
                 def test_parse_header_splits_a_raw_header():\n    \
                 # parse_header splits a raw header at its colon\n    \
                 raw = 'a:b'\n    \
                 assert parse_header(raw) == raw.split(':')\n    \
                 assert parse_header('raw:header') == ['raw', 'header']\n",
            ),
            document(
                "parser.py",
                "def parse_header(value):\n    return value.split(':')\n",
            ),
        ],
        memories: vec![],
        graph: json!({"edges":[]}),
        state: "fixture-v1".into(),
    };
    let files = |question: &str, include_tests: Option<Value>| {
        let mut args =
            json!({"question":question,"max_tokens":2000,"limit":4,"expand_context":false});
        if let Some(value) = include_tests {
            args["include_tests"] = value;
        }
        let packet = query::context(&snap, &args, json!({})).unwrap();
        let ids: Vec<String> = packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|window| window["id"].as_str().unwrap().to_string())
            .collect();
        (ids, packet["test_files"].clone())
    };
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    let question = "how does parse_header split a raw header";
    let (fused, report) = files(question, Some(json!(true)));
    assert_eq!(report, json!({"include_tests":true,"defaulted":false}));
    assert_eq!(
        fused.first().map(String::as_str),
        Some("file:tests/test_parser.py"),
        "the fixture ranks the test first: {fused:?}"
    );
    let (moved, report) = files(question, Some(json!(false)));
    assert_eq!(
        moved,
        ["file:parser.py", "file:tests/test_parser.py"],
        "{report}"
    );
    assert_eq!(
        report,
        json!({"include_tests":false,"defaulted":false,"moved":1})
    );
    assert_eq!(sorted(moved.clone()), sorted(fused.clone()));
    let (auto, report) = files(question, Some(json!("auto")));
    assert_eq!(auto, moved);
    assert_eq!(report["question_names_tests"], false, "{report}");
    // Left out, include_tests is "auto".
    let (default, report) = files(question, None);
    assert_eq!(default, moved);
    assert_eq!(
        report,
        json!({"include_tests":"auto","defaulted":true,"question_names_tests":false,"moved":1})
    );
    // The word alone does not name tests, so the move applies.
    let worded = "which tests check that parse_header splits a raw header";
    let (code_first, report) = files(worded, None);
    assert_eq!(report["question_names_tests"], false, "{report}");
    assert_eq!(
        code_first.first().map(String::as_str),
        Some("file:parser.py"),
        "{code_first:?}"
    );
    // A question that names a test keeps the tests where the ranking put
    // them.
    let asked = "what does test_parse_header_splits_a_raw_header check";
    let (kept, report) = files(asked, None);
    assert_eq!(report["question_names_tests"], true, "{report}");
    assert!(report.get("moved").is_none(), "{report}");
    assert_eq!(kept, files(asked, Some(json!(true))).0);
    assert_eq!(
        kept.first().map(String::as_str),
        Some("file:tests/test_parser.py"),
        "{kept:?}"
    );
    assert!(
        query::context(
            &snap,
            &json!({"question":question,"include_tests":"sometimes"}),
            json!({})
        )
        .is_err()
    );
}
