//! The MCP replies an agent reads. Claude Code shows the model a reply's
//! `structuredContent` as JSON whenever it has one, instead of its text, so
//! in the essentials set (`--tools essentials`, what setup gives Claude
//! Code) `recall_project` answers with the packet's Markdown only (at most
//! 12,000 characters) and the graph tools with the engine's text and the
//! citations block only. `structured: true`, beside the published
//! contracts, asks for the 0.1.0 reply with the whole packet. Every other
//! set answers as 0.1.0 did.
use graphicle_runtime::{
    api::{self, App, ToolSet},
    jobs, model, query,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    /// One project with `files`, synced, its code graph recorded.
    fn new(files: &[(String, String)]) -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Lean replies").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"lean","name":"lean"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let args = |mut value: Value| {
            value["repository_id"] = json!(repo);
            value
        };
        let state = store
            .project_status(&workspace, &key, &args(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = store
            .sync_prepare(
                &workspace,
                &key,
                &args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest})),
            )
            .unwrap();
        let upload: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"content":text}))
            .collect();
        store
            .sync_upload(
                &workspace,
                &key,
                &args(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        store
            .sync_commit(&workspace, &key, &args(json!({"sync_id":begin["sync_id"]})))
            .unwrap();
        jobs::drain_projection(&store, &repo).unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        }
    }
    /// A server of the essentials set.
    fn app(&self) -> App {
        let mut app = App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        app.mcp.tools = ToolSet::Essentials;
        app
    }
    fn rpc(&self, app: &App, method: &str, params: Value) -> Value {
        api::mcp(
            app,
            &self.workspace,
            &self.key,
            &self.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    }
    /// A tool call's reply, which must not be an error.
    fn result(&self, app: &App, name: &str, arguments: Value) -> Value {
        let reply = self.rpc(
            app,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        );
        assert!(reply.get("error").is_none(), "{name}: {reply:#}");
        assert_eq!(reply["result"]["isError"], false, "{name}: {reply:#}");
        reply["result"].clone()
    }
}

/// `a.py`, `b.py` and ten ledger files of invoice helpers, about 48,000
/// characters that all match a question about settling invoices.
fn ledger() -> Vec<(String, String)> {
    let mut files = vec![
        ("a.py".to_string(), A.to_string()),
        ("b.py".to_string(), B.to_string()),
    ];
    for part in 0..10 {
        let mut text = String::new();
        for n in 0..40 {
            text.push_str(&format!(
                "def settle_invoice_{part}_{n}(amount):\n    \"\"\"Settle invoice {n}: round the amount to cents.\"\"\"\n    return round(amount * 100) / 100\n\n"
            ));
        }
        files.push((format!("ledger/part_{part}.py"), text));
    }
    files
}

#[test]
fn recall_project_answers_markdown_within_the_cap_unless_asked_for_the_packet() {
    let f = Fixture::new(&ledger());
    let app = f.app();
    let question = "How does settle_invoice round the amount to cents?";
    // The default: the packet's Markdown, once, as the only content.
    let lean = f.result(&app, "recall_project", json!({"question":question}));
    assert!(lean.get("structuredContent").is_none(), "{lean:#}");
    let content = lean["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "{lean:#}");
    let text = s(&content[0], "text");
    assert!(text.starts_with("Graphicle project evidence"), "{text}");
    assert!(text.chars().count() as u64 <= query::MCP_RECALL_CHARS);
    // Asked for: the 0.1.0 reply, whose Markdown is the same text and which
    // cites the same files.
    let full = f.result(
        &app,
        "recall_project",
        json!({"question":question,"structured":true}),
    );
    let packet = &full["structuredContent"];
    assert_eq!(s(packet, "markdown"), text);
    let as_text: Value = serde_json::from_str(s(&full["content"][0], "text")).unwrap();
    assert_eq!(as_text["markdown"], packet["markdown"]);
    let sources = packet["sources"].as_array().unwrap();
    assert!(!sources.is_empty(), "{packet:#}");
    for source in sources {
        assert!(
            text.contains(&format!("{}:", s(source, "path"))),
            "{} is not cited in the Markdown",
            s(source, "path")
        );
    }
    // A larger budget than the cap: the packet as asked, the Markdown capped
    // (its budget line says so).
    let asked = json!({"question":question,"max_tokens":16000,"max_chars":40000});
    let mut structured = asked.clone();
    structured["structured"] = json!(true);
    let big = f.result(&app, "recall_project", structured);
    let big = s(&big["structuredContent"], "markdown");
    assert!(
        big.chars().count() as u64 > query::MCP_RECALL_CHARS,
        "the fixture must overflow the cap: {} characters",
        big.chars().count()
    );
    let capped = f.result(&app, "recall_project", asked);
    assert!(capped.get("structuredContent").is_none(), "{capped:#}");
    let text = s(&capped["content"][0], "text");
    assert!(
        text.chars().count() as u64 <= query::MCP_RECALL_CHARS,
        "{} characters",
        text.chars().count()
    );
    assert!(
        text.contains("Budget: 16000 cl100k_base reference tokens / 12000 characters."),
        "{text}"
    );
    // `structured` is a boolean, and only these tools take it; no contract
    // publishes it.
    let bad = f.rpc(
        &app,
        "tools/call",
        json!({"name":"recall_project","arguments":{"question":question,"structured":"yes"}}),
    );
    assert!(
        s(&bad["error"], "message").contains("structured must be true or false"),
        "{bad}"
    );
    let other = f.rpc(
        &app,
        "tools/call",
        json!({"name":"read_project_source","arguments":{"path":"a.py","structured":true}}),
    );
    assert!(
        s(&other["error"], "message").contains("published contract"),
        "{other}"
    );
    let listed = f.rpc(&app, "tools/list", json!({}));
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert!(
            tool["inputSchema"]["properties"]
                .get(query::STRUCTURED)
                .is_none(),
            "{tool}"
        );
    }
}

#[test]
fn graph_tools_answer_the_engine_text_and_citations_only() {
    let f = Fixture::new(&[
        ("a.py".to_string(), A.to_string()),
        ("b.py".to_string(), B.to_string()),
    ]);
    let app = f.app();
    for (name, arguments) in [
        ("get_node", json!({"label":"beta"})),
        ("get_neighbors", json!({"label":"beta"})),
        ("query_graph", json!({"question":"who calls beta"})),
    ] {
        let lean = f.result(&app, name, arguments.clone());
        assert!(lean.get("structuredContent").is_none(), "{name}: {lean:#}");
        let mut asked = arguments;
        asked["structured"] = json!(true);
        let full = f.result(&app, name, asked);
        assert!(
            full["structuredContent"]["nodes"].is_array(),
            "{name}: {full:#}"
        );
        // Few nodes: the same text and citations as the 0.1.0 reply.
        assert_eq!(lean["content"], full["content"], "{name}");
        assert_eq!(
            lean["content"][0]["text"], full["structuredContent"]["text"],
            "{name}"
        );
        assert!(
            s(&lean["content"][1], "text").starts_with("Citations (code-graph episode "),
            "{name}: {lean:#}"
        );
    }
}

/// `--tools all`, `graph` and `memory` answer as 0.1.0 did: the whole
/// answer in `structuredContent` and again as JSON text, recall at the
/// budget asked for (no character cap) and in 0.1.0's order (test files are
/// not moved); `structured` is taken and changes nothing.
#[test]
fn every_other_tool_set_answers_as_0_1_0() {
    let mut files = ledger();
    files.push((
        "tests/test_ledger.py".to_string(),
        "from ledger.part_0 import settle_invoice_0_0\n\ndef test_settle_invoice_rounds_the_amount_to_cents():\n    # settle_invoice rounds the amount to cents\n    assert settle_invoice_0_0(1.005) == 1.0\n".to_string(),
    ));
    let f = Fixture::new(&files);
    let question = "How does settle_invoice round the amount to cents?";
    let asked = json!({"question":question,"max_tokens":16000,"max_chars":40000});
    for tools in [ToolSet::All, ToolSet::Memory] {
        let mut app = f.app();
        app.mcp.tools = tools;
        let reply = f.result(&app, "recall_project", asked.clone());
        let packet = &reply["structuredContent"];
        assert!(
            s(packet, "markdown").chars().count() as u64 > query::MCP_RECALL_CHARS,
            "{tools:?}: no cap"
        );
        let as_text: Value = serde_json::from_str(s(&reply["content"][0], "text")).unwrap();
        assert_eq!(&as_text, packet, "{tools:?}");
        assert_eq!(
            packet["test_files"],
            json!({"include_tests":true,"defaulted":false}),
            "{tools:?}"
        );
        let mut with = asked.clone();
        with["structured"] = json!(false);
        let again = f.result(&app, "recall_project", with);
        assert_eq!(again["structuredContent"]["markdown"], packet["markdown"]);
    }
    let mut app = f.app();
    app.mcp.tools = ToolSet::All;
    let graph = f.result(&app, "get_node", json!({"label":"beta"}));
    assert!(graph["structuredContent"]["nodes"].is_array(), "{graph:#}");
    let citations = s(&graph["content"][1], "text");
    assert!(!citations.contains("structured: true"), "{citations}");
    // The essentials set moves the test file behind the code.
    let essentials = f.result(
        &f.app(),
        "recall_project",
        json!({"question":question,"structured":true}),
    );
    assert_eq!(
        essentials["structuredContent"]["test_files"]["include_tests"],
        "auto"
    );
}
