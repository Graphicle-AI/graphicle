//! Reviewed memory read from the code symbols it is linked to, and code
//! read from the memory.
//!
//! A code window selected for a question lifts the reviewed memory linked to
//! a symbol inside it, and a selected memory lifts its symbols' current
//! windows, only with `symbol_links: true`; left out, the links are counted;
//! `false` reads none. The memory's text shares no word with the question,
//! so only the link can bring it. A stale memory is never lifted or listed.
//! `get_node` lists a symbol's linked memories and documents. Answers take
//! these switches, and `notes_first`, as recall does.
use axum::Json;
use graphicle_runtime::{
    graph_tools,
    jobs::{self, Worker},
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A: &str = "import os\n\n\ndef alpha():\n    return 1\n\n\ndef beta():\n    return 2\n";
const B: &str = "from a import alpha, beta\n\n\ndef run():\n    return alpha() + beta()\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    /// A project holding `files`, synced and projected.
    fn new(files: &[(&str, &str)]) -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Links").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"links","name":"Links"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        };
        f.sync(files);
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    /// Sync `files` and run the projection and link jobs.
    fn sync(&self, files: &[(&str, &str)]) {
        let status = self
            .store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &self.args(json!({"expected_snapshot":status["repository"]["snapshot"],"manifest":manifest})),
            )
            .unwrap();
        if begin["replayed"] != true {
            let upload: Vec<Value> = files
                .iter()
                .map(|(name, text)| json!({"path":name,"content":text}))
                .collect();
            self.store
                .sync_upload(
                    &self.workspace,
                    &self.key,
                    &self.args(json!({"sync_id":begin["sync_id"],"files":upload})),
                )
                .unwrap();
            self.store
                .sync_commit(
                    &self.workspace,
                    &self.key,
                    &self.args(json!({"sync_id":begin["sync_id"]})),
                )
                .unwrap();
        }
        jobs::drain_projection(&self.store, &self.repo).unwrap();
    }
    /// A reviewed memory named for `symbols`, quoting nothing.
    fn note(&self, title: &str, body: &str, symbols: Value) -> Value {
        self.store
            .note(
                &self.workspace,
                &self.key,
                &self.args(json!({"title":title,"body":body,"kind":"decision","symbols":symbols})),
            )
            .unwrap()
    }
    fn context(&self, question: &str, switches: Value) -> Value {
        let mut args = self.args(json!({"question":question}));
        for (key, value) in switches.as_object().unwrap() {
            args[key] = value.clone();
        }
        self.store
            .project_context(&self.workspace, &self.key, &args)
            .unwrap()
    }
    fn node(&self, label: &str) -> Value {
        graph_tools::call(
            &self.store,
            &self.workspace,
            &self.key,
            &self.args(json!({"tool":"get_node","label":label})),
        )
        .unwrap()
    }
}

fn includes(packet: &Value, memory: &Value) -> bool {
    packet["memories"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == memory["id"])
}

#[test]
fn a_code_window_lifts_its_linked_memory_only_when_asked() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let memory = f.note(
        "Quokka ledger",
        "Keep the quokka ledger frozen.",
        json!([{"path":"a.py","qualified_name":"alpha"}]),
    );
    let question = "What does alpha return?";
    // Left out: the link is counted and nothing is lifted.
    let counted = f.context(question, json!({}));
    assert!(!includes(&counted, &memory), "{:#}", counted["memories"]);
    let report = &counted["symbol_links"];
    assert_eq!(report["status"], "counted", "{report:#}");
    assert_eq!(report["withheld"], 1, "{report:#}");
    assert_eq!(report["lifted_memories"], 0);
    assert_eq!(counted["mechanisms"]["symbol_links_enabled"], false);
    assert_eq!(counted["mechanisms"]["links_used"]["memory"], 0);
    // false: no link is read.
    let off = f.context(question, json!({"symbol_links":false}));
    assert!(!includes(&off, &memory));
    assert_eq!(off["symbol_links"], json!({"status":"disabled"}));
    // true: the code window lifts the memory, one hop.
    let on = f.context(question, json!({"symbol_links":true}));
    let lifted = on["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == memory["id"])
        .cloned()
        .unwrap_or_else(|| panic!("{:#}", on["symbol_links"]));
    assert_eq!(lifted["via"], "link");
    let link = &lifted["link"];
    assert_eq!(link["direction"], "code_to_memory");
    assert_eq!(link["qualified_name"], "alpha");
    assert_eq!(link["rung"], "named");
    assert!(
        link["from"].as_str().unwrap().starts_with("file:a.py"),
        "{link}"
    );
    assert!(s(&on, "markdown").contains("Quokka ledger"));
    let report = &on["symbol_links"];
    assert_eq!(report["status"], "followed", "{report:#}");
    assert_eq!(
        (&report["lifted_memories"], &report["memories_included"]),
        (&json!(1), &json!(1)),
        "{report:#}"
    );
    let mechanisms = &on["mechanisms"];
    assert_eq!(mechanisms["symbol_links_enabled"], true);
    assert_eq!(mechanisms["links_used"]["memory"], 1);
    assert_eq!(mechanisms["links_used"]["by_rung"]["named"], 1);
    // The same packet otherwise: the code evidence is unchanged.
    let ids = |packet: &Value| -> Vec<Value> {
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| json!([e["id"], e["byte_start"], e["byte_end"]]))
            .collect()
    };
    assert_eq!(ids(&on), ids(&counted));
    assert_eq!(ids(&off), ids(&counted));

    // An edit elsewhere in the file keeps the memory current and linked.
    let elsewhere = A.replace("return 2", "return 20");
    f.sync(&[("a.py", &elsewhere), ("b.py", B)]);
    assert!(includes(
        &f.context(question, json!({"symbol_links":true})),
        &memory
    ));
    // An edit inside alpha withholds it: never lifted.
    f.sync(&[
        ("a.py", &elsewhere.replace("return 1", "return 10")),
        ("b.py", B),
    ]);
    let stale = f.context(question, json!({"symbol_links":true}));
    assert!(!includes(&stale, &memory), "{:#}", stale["memories"]);
    assert_eq!(stale["mechanisms"]["memories_withheld"]["by_symbol"], 1);
    assert_eq!(stale["mechanisms"]["links_used"]["memory"], 0);
}

/// The identifier route names, in one line, the current reviewed memories
/// linked to the symbols it cites; a call that leaves memory out gets no
/// such line.
#[test]
fn the_identifier_route_names_linked_reviewed_memory() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let memory = f.note(
        "Quokka ledger",
        "Keep the quokka ledger frozen.",
        json!([{"path":"a.py","qualified_name":"alpha"}]),
    );
    let packet = f.context("where is `alpha` defined", json!({"identifier_route":true}));
    assert_eq!(packet["route"]["path"], "identifier", "{packet:#}");
    assert_eq!(
        packet["linked_memories"][0]["id"], memory["id"],
        "{packet:#}"
    );
    let markdown = s(&packet, "markdown");
    assert!(
        markdown.contains("Reviewed memory linked to these symbols: Quokka ledger (memory "),
        "{markdown}"
    );
    let off = f.context(
        "where is `alpha` defined",
        json!({"identifier_route":true,"memory_retrieval":false}),
    );
    assert_eq!(off["route"]["path"], "identifier");
    assert!(!s(&off, "markdown").contains("Quokka ledger"), "{off:#}");
    // A symbol no memory links: no line.
    let beta = f.context("who calls `beta`", json!({"identifier_route":true}));
    assert!(
        !s(&beta, "markdown").contains("Reviewed memory"),
        "{beta:#}"
    );
}

#[test]
fn a_selected_memory_lifts_the_window_of_its_symbol() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let memory = f.note(
        "Quokka ledger",
        "Keep the quokka ledger frozen.",
        json!([{"path":"a.py","qualified_name":"beta"}]),
    );
    let lifted = |packet: &Value| {
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e.get("symbol_links").is_some())
            .cloned()
    };
    // With the graph on, the property graph of sources and reviewed knowledge
    // already brings a.py (the memory's file) whole: its link is covered and
    // nothing is lifted twice.
    let covered = f.context("quokka ledger", json!({"symbol_links":true}));
    assert!(includes(&covered, &memory));
    assert!(lifted(&covered).is_none());
    let report = &covered["symbol_links"];
    assert_eq!(
        (&report["covered"], &report["lifted_windows"]),
        (&json!(2), &json!(0)),
        "{report:#}"
    );
    // With the graph off, only the link can bring the code.
    let counted = f.context("quokka ledger", json!({"graph_retrieval":false}));
    assert!(includes(&counted, &memory), "{:#}", counted["memories"]);
    assert!(lifted(&counted).is_none());
    assert_eq!(counted["evidence"], json!([]), "{:#}", counted["evidence"]);
    assert_eq!(
        counted["symbol_links"]["withheld"], 1,
        "{:#}",
        counted["symbol_links"]
    );
    let on = f.context(
        "quokka ledger",
        json!({"graph_retrieval":false,"symbol_links":true}),
    );
    let window = lifted(&on).unwrap_or_else(|| panic!("{:#}", on["symbol_links"]));
    assert_eq!(window["id"], "file:a.py");
    let facts = &window["symbol_links"];
    assert_eq!(facts["direction"], "memory_to_code");
    assert_eq!(facts["from"], format!("memory:{}", s(&memory, "id")));
    assert_eq!(facts["qualified_name"], "beta");
    let (a, b) = (
        window["byte_start"].as_u64().unwrap() as usize,
        window["byte_end"].as_u64().unwrap() as usize,
    );
    // The cited window holds beta (the packet may widen a short window).
    assert!(
        A[a..b].contains("def beta():\n    return 2\n"),
        "{window:#}"
    );
    assert_eq!(on["symbol_links"]["lifted_windows"], 1);
    assert_eq!(on["symbol_links"]["cited"], 1);
    assert_eq!(on["mechanisms"]["links_used"]["memory"], 1);
    // A memory without code links lifts nothing and reads no link.
    let g = Fixture::new(&[("a.py", A)]);
    g.store
        .note(
            &g.workspace,
            &g.key,
            &g.args(json!({"title":"Quokka ledger","body":"Keep the quokka ledger frozen."})),
        )
        .unwrap();
    let plain = g.context("quokka ledger", json!({"symbol_links":true}));
    assert_eq!(plain["symbol_links"], json!({"status":"no_links"}));
    // Its packet keeps its evidence and memories whatever the switch says.
    let default = g.context("quokka ledger", json!({}));
    assert_eq!(default["symbol_links"], json!({"status":"no_links"}));
    assert_eq!(plain["evidence"], default["evidence"]);
    assert_eq!(plain["memories"], default["memories"]);
    assert_eq!(plain["markdown"], default["markdown"]);
}

#[test]
fn get_node_lists_the_linked_memory_and_documents() {
    let readme = "# Notes\n\nCall `alpha` before anything else.\n";
    let files = [("a.py", A), ("b.py", B), ("README.md", readme)];
    let f = Fixture::new(&files);
    let memory = f.note(
        "Quokka ledger",
        "Keep the quokka ledger frozen.",
        json!([{"path":"a.py","qualified_name":"alpha"}]),
    );
    let reply = f.node("alpha");
    let node = &reply["nodes"][0];
    assert_eq!(node["label"], "alpha()", "{reply:#}");
    let linked = &node["linked"];
    let memories = linked["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 1, "{linked:#}");
    assert_eq!(memories[0]["id"], memory["id"]);
    assert_eq!(memories[0]["title"], "Quokka ledger");
    assert_eq!(memories[0]["rung"], "named");
    let documents = linked["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 1, "{linked:#}");
    assert_eq!(documents[0]["document_id"], "file:README.md");
    let text = graph_tools::citations_text(&reply);
    assert!(
        text.contains(&format!(
            "reviewed memory linked: Quokka ledger ({})",
            s(&memory, "id")
        )),
        "{text}"
    );
    assert!(
        text.contains("documents mentioning it: file:README.md"),
        "{text}"
    );
    // Nothing links to beta.
    assert_eq!(
        f.node("beta")["nodes"][0]["linked"],
        json!({"memories":[],"documents":[]})
    );
    // A stale memory is not listed.
    f.sync(&[
        ("a.py", &A.replace("return 1", "return 10")),
        ("b.py", B),
        ("README.md", readme),
    ]);
    assert_eq!(f.node("alpha")["nodes"][0]["linked"]["memories"], json!([]));
}

/// The answer model quotes the first source it was given.
async fn inference(Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let source = &input["sources"][0];
    let answer = json!({"answer":"from the first source","abstain":false,
        "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]});
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":answer.to_string()}]}]}),
    )
}

impl Fixture {
    /// Queue a `basic` answer and run it as a worker would: its retrieval
    /// packet.
    async fn basic(&self, config: &model::Config, mut args: Value) -> Value {
        args["kind"] = json!("basic");
        args["repository_id"] = json!(self.repo);
        let queued = self
            .store
            .enqueue(&self.workspace, &self.key, &args, config)
            .unwrap();
        Worker::new(self.store.clone(), config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        let done = self
            .store
            .job_status(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"job_id":queued["id"]}),
            )
            .unwrap();
        assert_eq!(done["state"], "complete", "{done}");
        done["result"]["context"].clone()
    }
}

/// The answer model of [`inference`], served on a loopback address, and
/// the configuration that names it.
async fn answer_model() -> (model::Config, tokio::task::JoinHandle<()>) {
    // IPv6 loopback first: some build machines refuse IPv4 loopback.
    let mut bound = None;
    for address in ["[::1]:0", "127.0.0.1:0"] {
        if let Ok(listener) = tokio::net::TcpListener::bind(address).await {
            bound = Some(listener);
            break;
        }
    }
    let listener = bound.expect("a loopback address to serve the answer model on");
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route("/v1/responses", axum::routing::post(inference));
        axum::serve(listener, app).await.unwrap();
    });
    let config: model::Config = serde_json::from_value(json!({"default_model":"fast","models":{
        "fast":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":4096}
    }}))
    .unwrap();
    (config, server)
}

/// Answers take `notes_first` like `context`: left out, the answer's
/// packet is 0.1.2's; `true` orders a question in prose notes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_take_the_notes_first_switch() {
    let f = Fixture::new(&[
        ("a.py", A),
        ("b.py", B),
        ("README.md", "# Notes\n\nalpha returns one.\n"),
    ]);
    let (config, server) = answer_model().await;
    let question = "What does alpha return?";
    let plain = f.basic(&config, json!({"question":question})).await;
    assert_eq!(
        plain["notes_first"],
        json!({"status":"off","defaulted":true}),
        "{}",
        plain["notes_first"]
    );
    let first = f
        .basic(&config, json!({"question":question,"notes_first":true}))
        .await;
    assert_eq!(
        first["notes_first"]["status"], "reranked",
        "{}",
        first["notes_first"]
    );
    assert_eq!(first["notes_first"]["defaulted"], false);
    assert_eq!(first["mechanisms"]["switches"]["notes_first"], true);
    server.abort();
}

/// Answers take `symbol_links` whatever `doc_links` says: with documents'
/// links off, `symbol_links: true` still lifts the reviewed memory linked to
/// the code window, reports `followed`, and the echo shows the values in
/// force. Left out, the link is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_follow_memory_links_with_document_links_off() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let memory = f.note(
        "Quokka ledger",
        "Keep the quokka ledger frozen.",
        json!([{"path":"a.py","qualified_name":"alpha"}]),
    );
    let (config, server) = answer_model().await;
    let question = "What does alpha return?";
    let counted = f
        .basic(&config, json!({"question":question,"doc_links":false}))
        .await;
    assert!(!includes(&counted, &memory), "{:#}", counted["memories"]);
    assert_eq!(
        counted["symbol_links"]["status"], "counted",
        "{:#}",
        counted["symbol_links"]
    );
    assert_eq!(counted["symbol_links"]["withheld"], 1);
    assert_eq!(counted["mechanisms"]["switches"]["symbol_links"], "counted");
    let on = f
        .basic(
            &config,
            json!({"question":question,"doc_links":false,"symbol_links":true}),
        )
        .await;
    let lifted = on["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == memory["id"])
        .cloned()
        .unwrap_or_else(|| panic!("{:#}", on["symbol_links"]));
    assert_eq!(lifted["via"], "link");
    assert_eq!(lifted["link"]["direction"], "code_to_memory");
    let report = &on["symbol_links"];
    assert_eq!(report["status"], "followed", "{report:#}");
    assert_eq!(report["lifted_memories"], 1, "{report:#}");
    let mechanisms = &on["mechanisms"];
    assert_eq!(mechanisms["symbol_links_enabled"], true);
    assert_eq!(mechanisms["links_used"]["memory"], 1);
    assert_eq!(mechanisms["switches"]["symbol_links"], true);
    assert_eq!(mechanisms["switches"]["doc_links"], false);
    assert_eq!(on["doc_links"]["status"], "disabled");
    assert_eq!(on["integration"]["symbol_links"], true);
    server.abort();
}
