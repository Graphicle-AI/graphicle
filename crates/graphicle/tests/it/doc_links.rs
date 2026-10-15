//! Documents linked to the code symbols they name.
//!
//! Repository pages (Markdown, reStructuredText) and connector documents
//! (Jira, Confluence, Notion, imported JSONL) name code by marking it as code;
//! the workspace's `link` job resolves each mention as the engine resolves
//! Markdown mentions and keeps the result as rows, never as graph nodes. A
//! query counts the selected windows' links and, with `doc_links: true`,
//! lifts what they reach, and `graph.doc_links` reads the rows back, both
//! only as far as the reader may read the documents and the projects they
//! name.
use axum::{
    Json, Router,
    extract::{Request, State},
    response::IntoResponse,
};
use graphicle_runtime::{
    atlassian, connectors, doc_links, documents, graph_projection,
    jobs::{self, Worker},
    model,
    store::{self, Store, s},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::TempDir;

const WIRE: &str = "class Parser:\n    def read(self, line):\n        return parse_header(line)\n\n\ndef parse_header(line):\n    name, _, value = line.partition(\":\")\n    return name.strip().lower(), value.strip()\n\n\ndef helper():\n    return 1\n";
const OTHER: &str =
    "def helper():\n    return 2\n\n\nclass Store:\n    def get(self, key):\n        return key\n";
const GUIDE: &str = "---\ntitle: `front_matter`\n---\n# Guide to `parse_header`\n\nHeaders go through `parse_header`; the reader is `Parser.read`,\nand `src/wire.py::Parser::read` cites its file. `helper` is ambiguous,\n`print` is a builtin, `missing_function` is unknown and `Store.get()` is a method.\n\n```\n`fenced_code` is not a mention\n```\n\n`a.py` names a file and `two words` are not code.\n";
/// A function whose body shares no word with the page about it.
const PARSER: &str = "def parse_header(line):\n    name, _, value = line.partition(\":\")\n    return name.strip().lower(), value.strip()\n";
const PROTOCOL: &str = "# Gateway policy\n\nThe gateway refuses chunked bodies from older clients; the check lives in `parse_header`.\n";
const RELEASE: &str = "# Release notes\n\nVersion two adds retries.\n";
const CLAMP: &str = "def clamp(amount, low, high):\n    return max(low, min(amount, high))\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Doc links").unwrap();
        Self {
            _dir: dir,
            store,
            workspace: s(&ws, "id").to_string(),
            key: s(&ws, "capability").to_string(),
        }
    }
    fn project(&self, key: &str) -> String {
        s(
            &self
                .store
                .register(
                    &self.workspace,
                    &self.key,
                    &json!({"project_key":key,"name":key}),
                )
                .unwrap(),
            "id",
        )
        .to_string()
    }
    fn sync(&self, repo: &str, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &json!({"repository_id":repo}))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest}),
            )
            .unwrap();
        let upload: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"content":text}))
            .collect();
        self.store
            .sync_upload(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"sync_id":begin["sync_id"],"files":upload}),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"sync_id":begin["sync_id"]}),
            )
            .unwrap();
    }
    /// The project's projection and the workspace `link` job after it.
    fn drain(&self, repo: &str) -> Vec<Value> {
        jobs::drain_projection(&self.store, repo).unwrap()
    }
    fn context(&self, repo: &str, mut args: Value) -> Value {
        args["repository_id"] = json!(repo);
        self.store
            .project_context(&self.workspace, &self.key, &args)
            .unwrap()
    }
    fn read(&self, token: &str, args: Value) -> graphicle_runtime::Result<Value> {
        doc_links::read(&self.store, &self.workspace, token, &args)
    }
    fn count(&self, sql: &str) -> i64 {
        self.store
            .read(|db| Ok(store::one(db, sql, [])?.unwrap()))
            .unwrap()["n"]
            .as_i64()
            .unwrap()
    }
    fn rows(&self, repo: &str) -> Vec<Value> {
        self.store
            .read(|db| {
                let mut found = store::rows(
                    db,
                    "SELECT document_id,document_sha256,mention,mark,state,scope,candidates,target_repository_id,\
                     stable_id,target_relpath,target,rung,producer FROM gf_doc_links WHERE repository_id=? \
                     ORDER BY document_id,byte_start,id",
                    [repo],
                )?;
                for row in &mut found {
                    store::decode(row, "target")?;
                }
                Ok(found)
            })
            .unwrap()
    }
    fn reader(&self) -> (String, String) {
        let reader = self
            .store
            .grant(
                &self.workspace,
                &self.key,
                &json!({"role":"reader","label":"restricted"}),
            )
            .unwrap();
        (
            s(&reader, "id").to_string(),
            s(&reader, "capability").to_string(),
        )
    }
    fn revoke(&self, repo: &str, credential: &str) {
        self.store
            .graph_access(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"credential_id":credential,"state":"revoked"}),
            )
            .unwrap();
    }
}

fn by_mention<'a>(rows: &'a [Value], mention: &str) -> Vec<&'a Value> {
    rows.iter().filter(|r| r["mention"] == mention).collect()
}

/// `(evidence id, doc_links facts)` of a packet's windows.
fn windows(packet: &Value) -> Vec<(String, Value)> {
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                s(w, "id").to_string(),
                w.get(doc_links::ARM).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

fn cites(packet: &Value, id: &str) -> bool {
    windows(packet).iter().any(|(window, _)| window == id)
}

/// Repository Markdown pages link exactly
/// where the engine's own Markdown resolution puts its `references` edges
/// in the episode, and the mentions it leaves out are recorded as
/// ambiguous or unresolved, never linked.
#[test]
fn markdown_mentions_link_where_the_engine_resolves_them() {
    let f = Fixture::new();
    let repo = f.project("guide");
    f.sync(
        &repo,
        &[
            ("src/wire.py", WIRE),
            ("src/other.py", OTHER),
            ("docs/guide.md", GUIDE),
        ],
    );
    let ran = f.drain(&repo);
    assert!(ran.iter().any(|j| j["kind"] == "link"), "{ran:?}");
    // The engine's references from the page's nodes to code, in the episode.
    let expected: BTreeSet<String> = f
        .store
        .read(|db| {
            let episode = graph_projection::current_episode(db, &repo)?;
            let handle = graph_projection::resident(db, &repo, &episode)?;
            let graph: &graphicle::graph::Graph = &handle;
            let text = |i: usize, key: &str| {
                graph.nodes()[i]
                    .attributes
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            Ok(graph
                .iter_edges()
                .filter(|(a, b, edge)| {
                    edge.attributes.get("relation").and_then(Value::as_str) == Some("references")
                        && text(*a, "source_file") == "docs/guide.md"
                        && text(*b, "file_type") == "code"
                })
                .map(|(_, b, _)| graphicle::serve_search::id(graph, b))
                .collect())
        })
        .unwrap();
    let rows = f.rows(&repo);
    let linked: BTreeSet<String> = rows
        .iter()
        .filter(|r| r["state"] == "linked")
        .map(|r| s(&r["target"], "node_id").to_string())
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(linked, expected, "{rows:#?}");
    for row in &rows {
        assert_eq!(row["document_id"], "file:docs/guide.md");
        assert_eq!(
            (row["rung"].as_str(), row["producer"].as_str()),
            (Some("mention"), Some("core"))
        );
    }
    let header = by_mention(&rows, "parse_header");
    assert_eq!(header.len(), 2, "the heading and the text: {rows:#?}");
    assert!(
        header
            .iter()
            .all(|r| r["state"] == "linked" && r["scope"] == "project")
    );
    assert_eq!(header[0]["target_relpath"], "src/wire.py");
    assert!(
        header[0]["stable_id"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    let cited = by_mention(&rows, "src/wire.py::Parser::read");
    assert_eq!(cited[0]["state"], "linked");
    assert_eq!(cited[0]["target"]["confidence"], "EXTRACTED");
    assert_eq!(
        cited[0]["stable_id"],
        by_mention(&rows, "Parser.read")[0]["stable_id"]
    );
    assert_eq!(by_mention(&rows, "Store.get()")[0]["state"], "linked");
    let helper = by_mention(&rows, "helper");
    assert_eq!(
        (
            helper[0]["state"].as_str(),
            helper[0]["candidates"].as_i64()
        ),
        (Some("ambiguous"), Some(2))
    );
    assert!(helper[0]["target_repository_id"].is_null());
    for unresolved in ["print", "missing_function"] {
        assert_eq!(by_mention(&rows, unresolved)[0]["state"], "unresolved");
    }
    for never in ["front_matter", "fenced_code", "a.py", "two words"] {
        assert!(by_mention(&rows, never).is_empty(), "{never}");
    }
    // Coverage: one page read, its mentions by outcome.
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    let files = &coverage["coverage"]["by_source_kind"]["repository_file"];
    assert_eq!(files["documents"], 1, "{coverage}");
    assert_eq!(files["mentions"], json!(rows.len()));
    assert_eq!(
        files["linked"],
        json!(rows.iter().filter(|r| r["state"] == "linked").count())
    );
    assert_eq!(
        (files["ambiguous"].as_u64(), files["unresolved"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(coverage["coverage"]["by_rung"]["mention"], files["linked"]);
    assert_eq!(coverage["coverage"]["pending_documents"], 0);
    // One source row per document read: code files are not documents.
    assert_eq!(f.count("SELECT count(*) AS n FROM gf_doc_link_sources"), 1);
    // A page synced but not yet read by the `link` job is pending; a code
    // file never is.
    f.sync(
        &repo,
        &[
            ("src/wire.py", WIRE),
            ("src/other.py", OTHER),
            ("docs/guide.md", GUIDE),
            ("docs/new.md", "# New\n\nSee `helper`.\n"),
        ],
    );
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    assert_eq!(coverage["coverage"]["pending_documents"], 1, "{coverage}");
    let new = f
        .read(
            &f.key,
            json!({"repository_id":repo,"document_id":"file:docs/new.md"}),
        )
        .unwrap();
    assert_eq!(new["pending"], true);
    let code = f
        .read(
            &f.key,
            json!({"repository_id":repo,"document_id":"file:src/wire.py"}),
        )
        .unwrap();
    assert_eq!(
        (
            code["pending"].as_bool(),
            code["mentions"].as_array().map(Vec::len)
        ),
        (Some(false), Some(0))
    );
    f.drain(&repo);
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    assert_eq!(coverage["coverage"]["pending_documents"], 0, "{coverage}");
    assert_eq!(
        coverage["coverage"]["by_source_kind"]["repository_file"]["documents"],
        2
    );
}

/// A reStructuredText page names a function with a Sphinx role, which
/// links; an ambiguous name links nothing and is recorded as ambiguous.
#[test]
fn an_rst_role_links_and_an_ambiguous_name_is_recorded() {
    let f = Fixture::new();
    let repo = f.project("sphinx");
    f.sync(
        &repo,
        &[
            ("src/wire.py", WIRE),
            ("src/other.py", OTHER),
            (
                "docs/api.rst",
                "API\n===\n\nUse :func:`parse_header` to split a header line, :func:`helper` for nothing\nand ``Parser.read()`` to read one.\n",
            ),
        ],
    );
    f.drain(&repo);
    let rows = f.rows(&repo);
    let parse = by_mention(&rows, "parse_header");
    assert_eq!(
        (parse[0]["state"].as_str(), parse[0]["mark"].as_str()),
        (Some("linked"), Some("role"))
    );
    assert_eq!(parse[0]["target"]["qualified_name"], "parse_header");
    let helper = by_mention(&rows, "helper");
    assert_eq!(
        (
            helper[0]["state"].as_str(),
            helper[0]["candidates"].as_i64(),
            helper[0]["mark"].as_str()
        ),
        (Some("ambiguous"), Some(2), Some("role"))
    );
    let read = by_mention(&rows, "Parser.read()");
    assert_eq!(
        (read[0]["state"].as_str(), read[0]["mark"].as_str()),
        (Some("linked"), Some("literal"))
    );
    let mentions = f
        .read(
            &f.key,
            json!({"repository_id":repo,"document_id":"file:docs/api.rst"}),
        )
        .unwrap();
    let states: Vec<&str> = mentions["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["linked", "ambiguous", "linked"], "{mentions}");
    assert_eq!(mentions["mentions"][0]["target"]["relpath"], "src/wire.py");
    assert_eq!(mentions["pending"], false);
}

fn gateway(f: &Fixture) -> String {
    let repo = f.project("gateway");
    f.sync(
        &repo,
        &[
            ("wire.py", PARSER),
            ("util.py", CLAMP),
            ("docs/protocol.md", PROTOCOL),
            ("docs/release.md", RELEASE),
        ],
    );
    f.drain(&repo);
    repo
}

/// End to end: a question in a page's words, which share no
/// word with the function the page names, returns the page and, with
/// `doc_links: true`, through the link, the function's window, with
/// `via: link`. With `doc_links: false` the function is absent, the code arm
/// included: its document nodes and their `references` edges take no part.
#[test]
fn a_page_question_reaches_the_function_only_through_document_links() {
    let f = Fixture::new();
    let repo = gateway(&f);
    let question = "Why does the gateway refuse chunked bodies?";
    // The link alone: the code arm off.
    let linked = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(
        cites(&linked, "file:docs/protocol.md"),
        "{:?}",
        windows(&linked)
    );
    let (_, facts) = windows(&linked)
        .into_iter()
        .find(|(id, _)| id == "file:wire.py")
        .unwrap_or_else(|| panic!("no link to the function: {:#}", linked["doc_links"]));
    assert_eq!(facts["via"], "link");
    assert_eq!(facts["direction"], "doc_to_code");
    assert_eq!(facts["mention"], "parse_header");
    assert_eq!(facts["document_id"], "file:docs/protocol.md");
    assert_eq!(facts["rung"], "mention");
    assert!(facts["stable_id"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(linked["doc_links"]["status"], "followed");
    assert_eq!(linked["doc_links"]["cited"], 1);
    let mechanisms = &linked["mechanisms"];
    assert_eq!(mechanisms["doc_links_enabled"], true);
    assert_eq!(
        mechanisms["doc_links_sources"],
        json!(["repository_file", "connector_source"])
    );
    assert_eq!(mechanisms["links_used"]["document"], 1);
    assert_eq!(mechanisms["links_used"]["by_rung"]["mention"], 1);
    assert_eq!(mechanisms["links_pending_projection"], 0);
    // Lexically the function is out of reach: off, it is absent.
    let off = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":false}),
    );
    assert!(cites(&off, "file:docs/protocol.md"));
    assert!(!cites(&off, "file:wire.py"), "{:?}", windows(&off));
    assert_eq!(off["doc_links"]["status"], "disabled");
    assert_eq!(off["mechanisms"]["doc_links_enabled"], false);
    assert_eq!(off["mechanisms"]["doc_links_sources"], json!([]));
    // `doc_links: false` with the code arm on: the arm leaves the page's
    // section and its `references` edge out, so no path remains.
    let arm_only = f.context(&repo, json!({"question":question,"doc_links":false}));
    assert!(
        !cites(&arm_only, "file:wire.py"),
        "{:#}",
        arm_only["code_graph"]
    );
    assert_eq!(arm_only["code_graph"]["document_nodes"], "excluded");
    // The defaults: the page and the function, by the code arm, in which
    // documents take part as before; the link is counted, not followed.
    let integrated = f.context(&repo, json!({"question":question}));
    assert!(
        cites(&integrated, "file:wire.py"),
        "{:?}",
        windows(&integrated)
    );
    assert!(integrated["code_graph"].get("document_nodes").is_none());
    assert_eq!(
        integrated["doc_links"]["lifted"], 0,
        "{:#}",
        integrated["doc_links"]
    );
    assert_eq!(integrated["mechanisms"]["doc_links_enabled"], false);
    assert!(
        f.store
            .project_context(
                &f.workspace,
                &f.key,
                &json!({"repository_id":repo,"question":question,"doc_links":"no"})
            )
            .is_err()
    );
}

/// Answers follow no document link unless asked. Left out, `doc_links`
/// counts the links of the selected windows and lifts nothing, so without
/// the code arm the function the page names stays out; the `link` job still
/// records the links and `graph.doc_links` lists them. `doc_links: true`
/// follows them for one call, and `--set doc_links=true` for every
/// `recall_project` call of an MCP server.
#[test]
fn answers_count_document_links_and_follow_them_only_when_asked() {
    let f = Fixture::new();
    let repo = gateway(&f);
    let question = "Why does the gateway refuse chunked bodies?";
    let default = f.context(&repo, json!({"question":question,"graph_retrieval":false}));
    assert!(
        cites(&default, "file:docs/protocol.md"),
        "{:?}",
        windows(&default)
    );
    assert!(!cites(&default, "file:wire.py"), "{:?}", windows(&default));
    assert!(windows(&default).iter().all(|(_, facts)| facts.is_null()));
    let report = &default["doc_links"];
    assert_eq!(report["status"], "counted", "{report}");
    assert!(report["links"].as_u64().unwrap() >= 1, "{report}");
    assert_eq!(report["withheld_repository"], report["links"], "{report}");
    assert_eq!(report["withheld_connector"], 0, "{report}");
    assert_eq!(report["lifted"], 0, "{report}");
    assert_eq!(report["cited"], 0, "{report}");
    let mechanisms = &default["mechanisms"];
    assert_eq!(mechanisms["doc_links_enabled"], false);
    assert_eq!(mechanisms["doc_links_sources"], json!([]));
    assert_eq!(mechanisms["links_used"]["document"], 0);
    // Recorded and listed all the same.
    let rows = f.rows(&repo);
    let link = by_mention(&rows, "parse_header")
        .into_iter()
        .find(|r| r["document_id"] == "file:docs/protocol.md")
        .unwrap_or_else(|| panic!("no row: {rows:#?}"));
    assert_eq!(link["state"], "linked");
    let listed = f
        .read(
            &f.key,
            json!({"repository_id":repo,"stable_id":link["stable_id"]}),
        )
        .unwrap();
    assert_eq!(
        listed["documents"][0]["document_id"], "file:docs/protocol.md",
        "{listed}"
    );
    // Asked for in one call.
    let on = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(cites(&on, "file:wire.py"), "{:?}", windows(&on));
    assert_eq!(on["doc_links"]["status"], "followed");
    assert_eq!(on["doc_links"]["withheld_repository"], 0);
    assert_eq!(on["mechanisms"]["doc_links_enabled"], true);
    // Fixed for an MCP server: every `recall_project` call follows them.
    let mut app =
        graphicle_runtime::api::App::new(f.store.clone(), model::Config::default(), vec![])
            .unwrap();
    app.mcp.fixed = graphicle_runtime::api::fixed_switches(&["doc_links=true".into()]).unwrap();
    let reply = graphicle_runtime::api::mcp(
        &app,
        &f.workspace,
        &f.key,
        &repo,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project",
            "arguments":{"question":question,"graph_retrieval":false,"structured":true}}}),
    )
    .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let recalled = &reply["result"]["structuredContent"];
    assert!(cites(recalled, "file:wire.py"), "{:?}", windows(recalled));
    assert_eq!(recalled["doc_links"]["status"], "followed");
    assert_eq!(
        recalled["mechanisms"]["server_fixed"],
        json!({"doc_links":true})
    );
}

/// Document links with workspace scope: a
/// workspace read keeps the document links each project's own read
/// followed (`doc_links: true`). The lifted window keeps its `doc_links`
/// facts through the fusion and the second pack, the packet's `doc_links`
/// reports the windows it holds, and `mechanisms` counts the links they used
/// and the sources followed. Left out, the links are counted and the
/// function stays out; `doc_links: false` leaves it out too, as in a project
/// read. (Links between projects are not followed in a workspace read.)
#[test]
fn a_workspace_read_keeps_each_projects_document_links() {
    let f = Fixture::new();
    let repo = gateway(&f);
    let other = f.project("notes");
    f.sync(
        &other,
        &[("notes.md", "# Notes\n\nRelease cadence is monthly.\n")],
    );
    f.drain(&other);
    let question = "Why does the gateway refuse chunked bodies?";
    let packet = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true,"scope":"workspace"}),
    );
    assert_eq!(packet["scope"], "workspace");
    assert_eq!(
        packet["workspace"]["projects_searched"], 2,
        "{:#}",
        packet["workspace"]
    );
    let (_, facts) = windows(&packet)
        .into_iter()
        .find(|(id, _)| id == "file:wire.py")
        .unwrap_or_else(|| panic!("no link to the function: {:#}", packet["doc_links"]));
    assert_eq!(facts["via"], "link");
    assert_eq!(facts["mention"], "parse_header");
    assert_eq!(packet["doc_links"]["status"], "followed");
    assert_eq!(packet["doc_links"]["cited"], 1);
    assert_eq!(packet["doc_links"]["projects"][&repo], "followed");
    // The lift bound is per project, not the sum over the projects.
    assert_eq!(packet["doc_links"]["limit"], 6, "{:#}", packet["doc_links"]);
    let mechanisms = &packet["mechanisms"];
    assert_eq!(mechanisms["doc_links_enabled"], true);
    assert_eq!(
        mechanisms["doc_links_sources"],
        json!(["repository_file", "connector_source"])
    );
    assert_eq!(mechanisms["links_used"]["document"], 1);
    assert_eq!(mechanisms["links_used"]["by_rung"]["mention"], 1);
    let counted = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"scope":"workspace"}),
    );
    assert!(!cites(&counted, "file:wire.py"), "{:?}", windows(&counted));
    assert_eq!(
        counted["doc_links"]["status"], "counted",
        "{:#}",
        counted["doc_links"]
    );
    assert_eq!(counted["doc_links"]["projects"][&repo], "counted");
    assert!(
        counted["doc_links"]["withheld_repository"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(counted["doc_links"]["cited"], 0);
    assert_eq!(counted["mechanisms"]["doc_links_enabled"], false);
    let off = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":false,"scope":"workspace"}),
    );
    assert!(cites(&off, "file:docs/protocol.md"), "{:?}", windows(&off));
    assert!(!cites(&off, "file:wire.py"), "{:?}", windows(&off));
    assert_eq!(off["doc_links"]["status"], "disabled");
    assert_eq!(off["doc_links"]["cited"], 0);
    assert_eq!(off["mechanisms"]["doc_links_enabled"], false);
    assert_eq!(off["mechanisms"]["doc_links_sources"], json!([]));
    assert_eq!(off["mechanisms"]["links_used"]["document"], 0);
}

/// The other direction: with `doc_links: true`, a code window lifts the
/// page passage that names a definition inside it. Links wait while either
/// side changed, and a rename drops them.
#[test]
fn a_code_hit_lifts_the_page_that_names_it() {
    let f = Fixture::new();
    let repo = gateway(&f);
    let question = "How is the line partitioned into name and value?";
    let linked = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(cites(&linked, "file:wire.py"), "{:?}", windows(&linked));
    let (_, facts) = windows(&linked)
        .into_iter()
        .find(|(id, _)| id == "file:docs/protocol.md")
        .unwrap_or_else(|| panic!("no lifted page: {:#}", linked["doc_links"]));
    assert_eq!(facts["direction"], "code_to_doc");
    assert_eq!(facts["mention"], "parse_header");
    assert_eq!(facts["relpath"], "wire.py");
    let off = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":false}),
    );
    assert!(cites(&off, "file:wire.py"));
    assert!(!cites(&off, "file:docs/protocol.md"), "{:?}", windows(&off));
    // An edit to the function's file leaves its links pending until the
    // `link` job runs again: they are not followed meanwhile.
    let touched = format!("{PARSER}\n# touched\n");
    let files = |wire: &str| {
        vec![
            ("wire.py".to_string(), wire.to_string()),
            ("util.py".to_string(), CLAMP.to_string()),
            ("docs/protocol.md".to_string(), PROTOCOL.to_string()),
            ("docs/release.md".to_string(), RELEASE.to_string()),
        ]
    };
    let sync = |wire: &str| {
        let owned = files(wire);
        let borrowed: Vec<(&str, &str)> = owned
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        f.sync(&repo, &borrowed);
    };
    sync(&touched);
    let pending = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(
        !cites(&pending, "file:docs/protocol.md"),
        "{:?}",
        windows(&pending)
    );
    assert!(
        pending["doc_links"]["pending"].as_u64().unwrap() > 0,
        "{:#}",
        pending["doc_links"]
    );
    assert!(
        pending["mechanisms"]["links_pending_projection"]
            .as_u64()
            .unwrap()
            > 0
    );
    f.drain(&repo);
    let again = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(
        cites(&again, "file:docs/protocol.md"),
        "{:?}",
        windows(&again)
    );
    // A rename drops the link: links are by name.
    sync(&PARSER.replace("def parse_header", "def parse_header_line"));
    f.drain(&repo);
    let rows = f.rows(&repo);
    assert_eq!(by_mention(&rows, "parse_header")[0]["state"], "unresolved");
    assert!(rows.iter().all(|r| r["state"] != "linked"), "{rows:#?}");
}

/// Across projects: a design-document project's page names a
/// function of a code project in the same workspace; its link row carries
/// both project ids. A reader who cannot read the code project sees neither
/// the row nor a count.
#[test]
fn a_page_links_into_another_project_and_hides_it_from_a_reader_without_access() {
    let f = Fixture::new();
    let code = f.project("code");
    let keps = f.project("keps");
    f.sync(&code, &[("wire.py", PARSER)]);
    f.drain(&code);
    f.sync(
        &keps,
        &[(
            "keps/kep-1.md",
            "# KEP 1\n\nThe parser entry point `parse_header` stays stable; `no_such_function` is gone.\n",
        )],
    );
    f.drain(&keps);
    let rows = f.rows(&keps);
    let link = by_mention(&rows, "parse_header")[0];
    assert_eq!(link["state"], "linked");
    assert_eq!(link["scope"], "workspace");
    assert_eq!(link["target_repository_id"], json!(code));
    assert_eq!(link["document_id"], "file:keps/kep-1.md");
    let stable = s(link, "stable_id").to_string();
    // The owner reads the link both ways.
    let back = f
        .read(&f.key, json!({"repository_id":code,"stable_id":stable}))
        .unwrap();
    assert_eq!(back["documents"][0]["repository_id"], json!(keps), "{back}");
    assert_eq!(back["documents"][0]["document_id"], "file:keps/kep-1.md");
    let forward = f
        .read(
            &f.key,
            json!({"repository_id":keps,"document_id":"file:keps/kep-1.md"}),
        )
        .unwrap();
    assert_eq!(
        forward["mentions"][0]["target"]["repository_id"],
        json!(code)
    );
    let coverage = f.read(&f.key, json!({"repository_id":keps})).unwrap();
    assert_eq!(coverage["coverage"]["total"]["linked"], 1, "{coverage}");
    assert_eq!(coverage["coverage"]["total"]["linked_other_projects"], 1);
    // A page's own project query cites nothing of the other project.
    let packet = f.context(&keps, json!({"question":"parser entry point stays stable"}));
    assert!(
        windows(&packet)
            .iter()
            .all(|(id, _)| id.starts_with("file:keps/"))
    );
    // A reader whose key cannot read the code project.
    let (credential, token) = f.reader();
    f.revoke(&code, &credential);
    let hidden = f
        .read(
            &token,
            json!({"repository_id":keps,"document_id":"file:keps/kep-1.md"}),
        )
        .unwrap();
    for mention in hidden["mentions"].as_array().unwrap() {
        assert_eq!(mention["state"], "unresolved", "{hidden}");
        assert!(mention.get("target").is_none());
    }
    let counted = f.read(&token, json!({"repository_id":keps})).unwrap();
    let total = &counted["coverage"]["total"];
    assert!(
        total.get("linked").is_none() && total.get("linked_other_projects").is_none(),
        "{counted}"
    );
    assert_eq!(total["unresolved"], 2);
    assert!(
        f.read(&token, json!({"repository_id":code,"stable_id":stable}))
            .is_err()
    );
}

/// Conversation stores stay as they were: a conversation-only store, `.txt`
/// sessions, gets no link rows, and its packets are the ones without links.
#[test]
fn conversation_stores_get_no_links_and_unchanged_evidence() {
    let f = Fixture::new();
    let repo = f.project("chat");
    f.sync(
        &repo,
        &[
            (
                "session-1.txt",
                "user: the `parse_header` call failed again\nassistant: retry with `strict=True`\n",
            ),
            (
                "session-2.txt",
                "user: dinner at eight?\nassistant: yes, after the `standup`\n",
            ),
        ],
    );
    f.drain(&repo);
    assert!(f.rows(&repo).is_empty());
    assert_eq!(f.count("SELECT count(*) AS n FROM gf_doc_link_sources"), 0);
    for question in ["what failed again", "when is dinner"] {
        let on = f.context(&repo, json!({"question":question}));
        let off = f.context(&repo, json!({"question":question,"doc_links":false}));
        assert_eq!(on["evidence"], off["evidence"]);
        assert_eq!(on["doc_links"]["status"], "no_links");
        assert_eq!(on["doc_links"]["lifted"], 0);
        assert_eq!(on["mechanisms"]["links_used"]["document"], 0);
    }
}

// --- Connector documents ---------------------------------------------------

/// The Jira document text as it was converted before documents were linked
/// to code, frozen here:
/// the converter must keep producing these bytes.
fn frozen_adf_text(value: &Value) -> String {
    fn walk(value: &Value, out: &mut String) {
        if value.is_null() {
            return;
        }
        if let Some(text) = value.as_str() {
            out.push_str(text);
            return;
        }
        if let Some(items) = value.as_array() {
            for item in items {
                walk(item, out);
            }
            return;
        }
        let kind = s(value, "type");
        match kind {
            "text" => out.push_str(s(value, "text")),
            "hardBreak" => out.push('\n'),
            "mention" => out.push_str(
                value["attrs"]["text"]
                    .as_str()
                    .or(value["attrs"]["id"].as_str())
                    .unwrap_or("[mention]"),
            ),
            "emoji" => out.push_str(
                value["attrs"]["text"]
                    .as_str()
                    .or(value["attrs"]["shortName"].as_str())
                    .unwrap_or("[emoji]"),
            ),
            "inlineCard" | "blockCard" => out.push_str(
                value["attrs"]["url"]
                    .as_str()
                    .unwrap_or("[linked content not fetched]"),
            ),
            "media" | "mediaSingle" => out.push_str("[attachment content not fetched]"),
            "listItem" => out.push_str("\n- "),
            "tableCell" | "tableHeader" => out.push_str(" | "),
            _ => {}
        }
        if let Some(content) = value.get("content") {
            walk(content, out);
        }
        if [
            "paragraph",
            "heading",
            "codeBlock",
            "blockquote",
            "tableRow",
        ]
        .contains(&kind)
        {
            out.push('\n');
        }
    }
    let mut text = String::new();
    walk(value, &mut text);
    text
}

/// Notion rich text as converted before documents were linked to code,
/// frozen here.
fn frozen_rich(value: &Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            v["plain_text"]
                .as_str()
                .or(v["text"]["content"].as_str())
                .unwrap_or("")
        })
        .collect()
}

fn code_marked_adf(marked: bool) -> Value {
    let mut code = json!({"type":"text","text":"parse_header"});
    if marked {
        code["marks"] = json!([{"type":"strong"},{"type":"code"}]);
    }
    json!({"type":"doc","content":[
        {"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Chunked bodies"}]},
        {"type":"paragraph","content":[
            {"type":"mention","attrs":{"text":"@Mira"}},
            {"type":"text","text":" says chunked bodies are refused in "},
            code,
            {"type":"text","text":" and "},
            {"type":"text","text":"Store.get()","marks":[{"type":"code"}]},
            {"type":"hardBreak"},
            {"type":"emoji","attrs":{"shortName":":ok:"}},
            {"type":"inlineCard","attrs":{"url":"https://example.test/x"}}]},
        {"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"not ","marks":[{"type":"em"}]},
            {"type":"text","text":"two words","marks":[{"type":"code"}]}]}]}]},
        {"type":"codeBlock","content":[{"type":"text","text":"parse_header(line)"}]},
        {"type":"table","content":[{"type":"tableRow","content":[
            {"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]})
}

/// Inline code is recorded beside the converted text, never in
/// it. Jira's ADF and Notion's rich text convert to exactly the bytes they
/// did before documents were linked to code, and Confluence's `<code>`
/// elements, which the HTML
/// conversion keeps no mark of, are found in its text.
#[test]
fn converters_keep_their_text_and_record_inline_code_beside_it() {
    let adf = code_marked_adf(true);
    let (text, spans) = atlassian::adf_text_spans(&adf).unwrap();
    assert_eq!(text, frozen_adf_text(&adf));
    assert_eq!(atlassian::adf_text(&adf).unwrap(), text);
    assert!(!text.contains('`'));
    let marked: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
    assert_eq!(marked, ["parse_header", "Store.get()", "two words"]);
    // Notion: the page text from its title and blocks.
    let title = json!([{"plain_text":"Gateway "},{"plain_text":"parse_header","annotations":{"code":true}}]);
    let block = json!([{"plain_text":"Refused in "},{"text":{"content":"Store.get"},"annotations":{"code":true}},{"plain_text":"."}]);
    let (title_text, title_spans) = connectors::rich_spans(&title);
    assert_eq!(title_text, frozen_rich(&title));
    let (block_text, block_spans) = connectors::rich_spans(&block);
    assert_eq!(block_text, frozen_rich(&block));
    let blocks = vec![
        (
            format!("  {block_text}"),
            block_spans.iter().map(|(a, b)| (a + 2, b + 2)).collect(),
        ),
        ("plain".to_string(), Vec::new()),
    ];
    let (page, page_spans) = connectors::notion_text(&(title_text.clone(), title_spans), &blocks);
    let frozen = format!(
        "# {title_text}\n\n{}",
        blocks
            .iter()
            .map(|(b, _)| b.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    assert_eq!(page, frozen);
    let marked: Vec<&str> = page_spans.iter().map(|(a, b)| &page[*a..*b]).collect();
    assert_eq!(marked, ["parse_header", "Store.get"]);
    // Confluence (the design left it unverified): the HTML conversion
    // writes `<code>` as backticks, as it always has, so the stored text
    // marks code itself and records no spans.
    let html = "<h1>Catering</h1><p>Use <code>parse_header</code> or <code class=\"x\">Store&#46;get</code>, not <code>two words</code> or <code>&lt;tag&gt;</code>.</p><p>Again <code>parse_header</code>.</p>";
    let parsed = documents::extract("page.html", html.as_bytes()).unwrap();
    let text = s(&parsed, "text");
    assert!(text.contains("Use `parse_header` or `Store.get`"), "{text}");
    let found: Vec<&str> = doc_links::connector(text, &[])
        .iter()
        .map(|m| &text[m.byte_start..m.byte_end])
        .collect();
    assert_eq!(
        found,
        ["parse_header", "Store.get", "parse_header"],
        "{text}"
    );
}

#[derive(Clone)]
struct Remote {
    marked: Arc<AtomicBool>,
}

async fn remote(State(state): State<Remote>, request: Request) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let value = match path.as_str() {
        "/rest/api/3/search/jql" => {
            json!({"issues":[
                {"id":"1","key":"GW-1","fields":{"summary":"Refuse chunked bodies","created":"2026-09-01T10:00:00Z",
                    "updated":"2026-09-02T10:00:00Z","reporter":{"accountId":"a1","displayName":"Mira"},
                    "status":{"name":"Done"},"description":code_marked_adf(state.marked.load(Ordering::SeqCst))}},
                {"id":"2","key":"GW-2","fields":{"summary":"Plain words","created":"2026-09-01T10:00:00Z",
                    "updated":"2026-09-02T10:00:00Z","reporter":{"accountId":"a1","displayName":"Mira"},
                    "status":{"name":"Open"},"description":{"type":"doc","content":[{"type":"paragraph","content":[
                        {"type":"text","text":"Someone should look at parse_header again."}]}]}}}],"isLast":true})
        }
        "/rest/api/3/issue/GW-1/comment" | "/rest/api/3/issue/GW-2/comment" => {
            json!({"startAt":0,"maxResults":100,"total":0,"comments":[]})
        }
        "/wiki/api/v2/spaces" => json!({"results":[{"id":"10","key":"TEAM","name":"Team"}]}),
        "/wiki/api/v2/spaces/10/pages" => json!({"results":[{"id":"20"}],"_links":{}}),
        "/wiki/api/v2/pages/20" => {
            json!({"id":"20","title":"Gateway","status":"current","createdAt":"2026-09-01T10:00:00Z",
                "version":{"createdAt":"2026-09-03T10:00:00Z","authorId":"a2"},
                "body":{"storage":{"value":"<h1>Gateway</h1><p>Headers are split by <code>parse_header</code>.</p>"}},
                "_links":{"webui":"/spaces/TEAM/pages/20"}})
        }
        "/pages/notion-1" => {
            json!({"id":"notion-1","url":"https://notion.test/notion-1","created_time":"2026-09-01T10:00:00Z",
                "last_edited_time":"2026-09-02T10:00:00Z",
                "properties":{"Name":{"type":"title","title":[{"plain_text":"Gateway notes"}]}}})
        }
        "/blocks/notion-1/children" => {
            json!({"results":[{"id":"b1","type":"paragraph","has_children":false,"paragraph":{"rich_text":[
                {"plain_text":"Header parsing lives in "},
                {"plain_text":"parse_header","annotations":{"code":true}},
                {"plain_text":"."}]}}],"has_more":false})
        }
        _ => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error":"unknown fixture path"})),
            )
                .into_response();
        }
    };
    Json(value).into_response()
}

/// Run one sync of `connector` as a worker does; it must complete.
async fn sync_connector(f: &Fixture, repo: &str, cfg: &model::Config, connector: String) {
    let job = f
        .store
        .enqueue(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"kind":"connector","connector_id":connector,"max_requests":16}),
            cfg,
        )
        .unwrap();
    Worker::new(f.store.clone(), cfg.clone())
        .unwrap()
        .once()
        .await
        .unwrap();
    let status = f
        .store
        .job_status(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"job_id":job["id"]}),
        )
        .unwrap();
    assert_eq!(status["state"], "complete", "{status}");
}

/// End to end over the three converters: a Jira issue whose ADF
/// marks `parse_header` as code, a Confluence page with `<code>` and a
/// Notion page with a `code` annotation each yield a link row to that
/// function; plain words yield none. A re-sync of the unchanged issue keeps
/// its sha256 and the memory citing it; a re-sync without the mark drops
/// the link. A reader the connectors are not shared with sees no link, no
/// lift and no count; forgetting a page removes its rows.
#[tokio::test]
async fn connector_code_marks_link_without_changing_text_or_staling_memories() {
    let f = Fixture::new();
    let repo = f.project("gateway");
    f.sync(&repo, &[("wire.py", PARSER), ("util.py", CLAMP)]);
    f.drain(&repo);
    let state = Remote {
        marked: Arc::new(AtomicBool::new(true)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(remote).with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cfg = model::Config::default();
    cfg.connector_endpoints
        .insert("notion".into(), base.clone());
    let secret = |name: &str, value: &str| {
        s(
            &f.store
                .put_credential(&f.workspace, &f.key, &json!({"name":name,"value":value}))
                .unwrap(),
            "credential_ref",
        )
        .to_string()
    };
    let basic = secret(
        "fixture-basic",
        &json!({"username":"fixture@example.test","password":"fixture"}).to_string(),
    );
    let bearer = secret("fixture-bearer", "fixture-token");
    let private = json!({"workspace_readers":false,"credentials":[]});
    let mut connectors = Vec::new();
    for (kind, credential, config) in [
        (
            "jira",
            &basic,
            json!({"base_url":base,"site_url":base,"auth_type":"basic","jql":"project = GW","acl":private}),
        ),
        (
            "confluence",
            &basic,
            json!({"base_url":base,"site_url":base,"auth_type":"basic","space_key":"TEAM","acl":private}),
        ),
        (
            "notion",
            &bearer,
            json!({"page_ids":["notion-1"],"acl":private}),
        ),
    ] {
        let connector = f
            .store
            .connector_add(
                &f.workspace,
                &f.key,
                &json!({"repository_id":repo,"kind":kind,"name":kind,"credential_ref":credential,"config":config}),
            )
            .unwrap();
        connectors.push(s(&connector, "id").to_string());
    }
    let sync = |connector: &str| sync_connector(&f, &repo, &cfg, connector.to_string());
    for connector in &connectors {
        sync(connector).await;
    }
    // Each sync queued the workspace's `link` job; the drain runs it.
    let ran = f.drain(&repo);
    assert!(ran.iter().any(|j| j["kind"] == "link"), "{ran:?}");
    let rows = f.rows(&repo);
    let jira = format!("connector:{}:GW-1", connectors[0]);
    let plain = format!("connector:{}:GW-2", connectors[0]);
    let confluence = format!("connector:{}:20", connectors[1]);
    let notion = format!("connector:{}:notion-1", connectors[2]);
    // Jira and Notion record their marks as spans; Confluence's text keeps
    // `<code>` as backticks.
    for (document, mark) in [
        (&jira, "code_mark"),
        (&confluence, "code_span"),
        (&notion, "code_mark"),
    ] {
        let link = rows
            .iter()
            .find(|r| r["document_id"] == json!(document) && r["mention"] == "parse_header")
            .unwrap_or_else(|| panic!("{document}: {rows:#?}"));
        assert_eq!(
            (link["state"].as_str(), link["mark"].as_str()),
            (Some("linked"), Some(mark)),
            "{document}"
        );
        assert_eq!(link["target_relpath"], "wire.py");
    }
    assert!(
        rows.iter().all(|r| r["document_id"] != json!(plain)),
        "plain words never link"
    );
    // The issue's text has no backticks: it is the frozen conversion's.
    let stored = |id: &str| {
        f.store
            .read(|db| {
                Ok(store::one(
                    db,
                    "SELECT content,sha256,metadata FROM am_connector_docs WHERE connector_id=? AND id=?",
                    rusqlite::params![connectors[0], id],
                )?
                .unwrap())
            })
            .unwrap()
    };
    let issue = stored("GW-1");
    assert!(s(&issue, "content").ends_with(&frozen_adf_text(&code_marked_adf(true))));
    assert!(!s(&issue, "content").contains('`'));
    // A reviewed memory citing the issue.
    let proposal = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.propose",
            &json!({"repository_id":repo,"kind":"decision","title":"Chunked bodies","body":"Refuse them.",
                "idempotency_key":"chunked","evidence":[{"source_id":jira,"quote":"chunked bodies are refused in parse_header"}]}),
        )
        .unwrap();
    let memory = |state: &str| {
        let listed = f
            .store
            .project_read(
                &f.workspace,
                &f.key,
                "memories",
                &json!({"repository_id":repo,"include_retired":true}),
            )
            .unwrap();
        let found = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == proposal["memory_id"])
            .unwrap()
            .clone();
        assert_eq!(found["state"], state, "{found}");
        found
    };
    let draft = memory("draft");
    f.store
        .review_memory(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"memory_id":draft["id"],"expected_revision":draft["revision"],
                "decision":"approve","maintainer":"Fixture reviewer","reason":"Checked the issue"}),
        )
        .unwrap();
    // Re-sync the unchanged issue: the same bytes, the same sha256, and the
    // memory still current.
    sync(&connectors[0]).await;
    assert_eq!(stored("GW-1")["sha256"], issue["sha256"]);
    assert_eq!(memory("reviewed")["stale"], false);
    // The issue links the function: with `doc_links: true` the function's
    // window lifts the issue; by default links are only counted.
    f.drain(&repo);
    let question = "How is the line partitioned into name and value?";
    let lifted = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(
        windows(&lifted)
            .iter()
            .any(|(id, facts)| id.starts_with("connector:") && facts["direction"] == "code_to_doc"),
        "{:#}",
        lifted["doc_links"]
    );
    assert_eq!(
        lifted["mechanisms"]["doc_links_sources"],
        json!(["repository_file", "connector_source"])
    );
    let default = f.context(&repo, json!({"question":question,"graph_retrieval":false}));
    assert!(
        windows(&default).iter().all(|(_, facts)| facts.is_null()),
        "{:?}",
        windows(&default)
    );
    assert!(default["doc_links"]["withheld_connector"].as_u64().unwrap() >= 1);
    // A reader the connectors are not shared with: no link, lift or count.
    let (_, token) = f.reader();
    let as_reader = f
        .store
        .project_context(
            &f.workspace,
            &token,
            &json!({"repository_id":repo,"question":question,"graph_retrieval":false,"doc_links":true}),
        )
        .unwrap();
    assert!(
        windows(&as_reader)
            .iter()
            .all(|(id, _)| !id.starts_with("connector:"))
    );
    assert_eq!(as_reader["doc_links"]["withheld_connector"], 0);
    assert_eq!(
        as_reader["doc_links"]["lifted"], 0,
        "{:#}",
        as_reader["doc_links"]
    );
    let stable = f
        .rows(&repo)
        .iter()
        .find(|r| r["state"] == "linked")
        .map(|r| s(r, "stable_id").to_string())
        .unwrap();
    let back = f
        .read(&token, json!({"repository_id":repo,"stable_id":stable}))
        .unwrap();
    assert_eq!(back["documents"], json!([]), "{back}");
    let coverage = f.read(&token, json!({"repository_id":repo})).unwrap();
    assert!(
        coverage["coverage"]["by_source_kind"]
            .get("connector_source")
            .is_none(),
        "{coverage}"
    );
    assert!(
        f.read(&token, json!({"repository_id":repo,"document_id":jira}))
            .is_err()
    );
    let owner = f
        .read(&f.key, json!({"repository_id":repo,"stable_id":stable}))
        .unwrap();
    assert_eq!(owner["documents"].as_array().unwrap().len(), 3, "{owner}");
    // A re-sync without the mark: the issue's link goes, and its other span
    // stays beside the text.
    state.marked.store(false, Ordering::SeqCst);
    sync(&connectors[0]).await;
    f.drain(&repo);
    let rows = f.rows(&repo);
    assert!(
        rows.iter()
            .all(|r| r["document_id"] != json!(jira) || r["mention"] != "parse_header"),
        "{rows:#?}"
    );
    assert!(
        s(&stored("GW-1"), "metadata").contains("code_spans"),
        "Store.get() is still marked"
    );
    // Forgetting the Confluence page removes its rows, with a clean receipt.
    let forgot = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "source.forget",
            &json!({"repository_id":repo,"connector_id":connectors[1],"document_ids":["20"]}),
        )
        .unwrap();
    let receipt = &forgot["receipt"];
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert!(
        receipt["removed"]["rows"]["doc_links"].as_u64().unwrap() >= 1,
        "{receipt:#}"
    );
    assert_eq!(receipt["remaining"]["rows"]["doc_links"], 0);
    assert_eq!(receipt["remaining"]["rows"]["doc_link_sources"], 0);
    assert!(
        f.rows(&repo)
            .iter()
            .all(|r| r["document_id"] != json!(confluence))
    );
    server.abort();
}

/// Imported JSONL documents may carry their own inline-code spans, checked
/// against their text; an import queues the workspace's `link` job.
#[test]
fn imports_take_code_spans_and_queue_the_link_job() {
    let f = Fixture::new();
    let repo = f.project("imports");
    f.sync(&repo, &[("wire.py", PARSER)]);
    f.drain(&repo);
    let connector = f
        .store
        .connector_add(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"kind":"jsonl","name":"Tickets","config":{"acl":{"workspace_readers":true,"credentials":[]}}}),
        )
        .unwrap();
    let text = "Refused in parse_header since two.";
    let import = |spans: Value| {
        f.store.import_documents(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"connector_id":connector["id"],
                "documents":[{"id":"T-1","title":"T-1","content":text,"code_spans":spans}]}),
        )
    };
    for bad in [json!([[11, 99]]), json!([[3, 3]]), json!("x")] {
        assert!(import(bad.clone()).is_err(), "{bad}");
    }
    import(json!([[11, 23]])).unwrap();
    assert_eq!(
        f.count("SELECT count(*) AS n FROM am_jobs WHERE kind='link' AND state='queued'"),
        1
    );
    f.drain(&repo);
    let rows = f.rows(&repo);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(
        (
            rows[0]["mention"].as_str(),
            rows[0]["mark"].as_str(),
            rows[0]["state"].as_str()
        ),
        (Some("parse_header"), Some("code_mark"), Some("linked"))
    );
    let question = "How is the line partitioned into name and value?";
    let lifted = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(
        windows(&lifted).iter().any(|(id, _)| id.ends_with(":T-1")),
        "{:#}",
        lifted["doc_links"]
    );
}

// --- Edge cases -------------------------------------------------------------

impl Fixture {
    /// `document_id → updated_at` of every document's source row: a row
    /// rewritten by a `link` job gets a new stamp.
    fn stamps(&self) -> BTreeMap<String, String> {
        self.store
            .read(|db| {
                Ok(store::rows(
                    db,
                    "SELECT document_id,updated_at FROM gf_doc_link_sources",
                    [],
                )?
                .into_iter()
                .map(|r| {
                    (
                        s(&r, "document_id").to_string(),
                        s(&r, "updated_at").to_string(),
                    )
                })
                .collect())
            })
            .unwrap()
    }
    fn queued_links(&self) -> i64 {
        self.count("SELECT count(*) AS n FROM am_jobs WHERE kind='link' AND state='queued'")
    }
    fn jsonl(&self, repo: &str, name: &str) -> String {
        let connector = self
            .store
            .connector_add(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"kind":"jsonl","name":name,
                    "config":{"acl":{"workspace_readers":true,"credentials":[]}}}),
            )
            .unwrap();
        s(&connector, "id").to_string()
    }
    fn import(&self, repo: &str, connector: &str, documents: Value) {
        self.store
            .import_documents(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"connector_id":connector,"documents":documents}),
            )
            .unwrap();
    }
}

/// The function's module imports a helper module the page never names.
const CALLER: &str = "from util import clamp\n\n\ndef parse_header(line):\n    name, _, value = line.partition(\":\")\n    return name.strip().lower(), clamp(len(value), 0, 64)\n";

/// With `doc_links: false`, `graph_local` takes no path from a
/// document to code either, whether the query asks for it
/// (`graph_retrieval: true`) or a current reviewed memory brings it in. Its
/// seed search follows the page's static edge to the function's module, and
/// its walk took the module's import one hop further: both leave documents'
/// edges to code out with the switch off.
#[test]
fn graph_local_takes_no_path_from_a_document_to_code_with_the_switch_off() {
    let f = Fixture::new();
    let repo = f.project("gateway");
    f.sync(
        &repo,
        &[
            ("wire.py", CALLER),
            ("util.py", CLAMP),
            ("docs/protocol.md", PROTOCOL),
            ("docs/release.md", RELEASE),
        ],
    );
    f.drain(&repo);
    let question = "Why does the gateway refuse chunked bodies?";
    let code = |packet: &Value| {
        windows(packet)
            .into_iter()
            .filter(|(id, _)| id.ends_with(".py"))
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    };
    let asked = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":true,"doc_links":false}),
    );
    let mechanisms = &asked["mechanisms"];
    assert_eq!(mechanisms["graph_local_status"], "searched", "{mechanisms}");
    assert_eq!(mechanisms["graph_candidate_passages"], 0, "{mechanisms}");
    assert!(cites(&asked, "file:docs/protocol.md"));
    assert_eq!(code(&asked), Vec::<String>::new());
    // On, the walk reaches the import.
    let on = f.context(&repo, json!({"question":question,"graph_retrieval":true}));
    assert!(
        on["mechanisms"]["graph_candidate_passages"]
            .as_u64()
            .unwrap()
            >= 1,
        "{}",
        on["mechanisms"]
    );
    assert!(cites(&on, "file:util.py"), "{:?}", code(&on));
    // A current reviewed memory brings `graph_local` in by itself.
    f.store
        .note(
            &f.workspace,
            &f.key,
            &json!({"repository_id":repo,"title":"Release cadence","body":"Releases ship monthly.",
                "dependencies":["docs/release.md"]}),
        )
        .unwrap();
    let remembered = f.context(&repo, json!({"question":question,"doc_links":false}));
    let mechanisms = &remembered["mechanisms"];
    assert_eq!(mechanisms["graph_local_status"], "searched", "{mechanisms}");
    assert_eq!(code(&remembered), Vec::<String>::new());
}

/// An import that lands while the workspace's `link` job runs
/// queues another job, which links what the running one may not have read;
/// `graph.link` runs the document step on request.
#[test]
fn an_import_while_the_link_job_runs_queues_another_and_graph_link_relinks() {
    let f = Fixture::new();
    let repo = f.project("tickets");
    f.sync(&repo, &[("wire.py", PARSER)]);
    f.drain(&repo);
    let connector = f.jsonl(&repo, "Tickets");
    let ticket = |id: &str| json!([{"id":id,"title":id,"content":"Refused in `parse_header`."}]);
    f.import(&repo, &connector, ticket("T-1"));
    // A worker takes the job the import queued; a second import lands.
    let running = f
        .store
        .lease_job_of_kind(&repo, "link")
        .unwrap()
        .expect("the import queued a link job");
    f.import(&repo, &connector, ticket("T-2"));
    assert_eq!(f.queued_links(), 1, "a job must follow the running one");
    let result = jobs::run_link(&f.store, &running);
    f.store.finish_job(&running, result).unwrap();
    f.drain(&repo);
    let rows = f.rows(&repo);
    for id in ["T-1", "T-2"] {
        assert!(
            rows.iter()
                .any(|r| s(r, "document_id").ends_with(&format!(":{id}")) && r["state"] == "linked"),
            "{id}: {rows:#?}"
        );
    }
    // `graph.link` relinks on request.
    f.store
        .transaction(|db| {
            db.execute("DELETE FROM gf_doc_links", [])?;
            db.execute("DELETE FROM gf_doc_link_sources", [])?;
            Ok(())
        })
        .unwrap();
    let linked = f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert_eq!(linked["documents"]["relinked"], 2, "{linked}");
    assert_eq!(
        f.rows(&repo)
            .iter()
            .filter(|r| r["state"] == "linked")
            .count(),
        2
    );
}

/// A projection, or a new project, recomputes only the
/// documents it can change. A page is read again when its own bytes change,
/// when a name it looks up changes, when a name it found nowhere appears in
/// another project, or when the file a link's window was read from changes;
/// an unrelated edit elsewhere, a new project and another page's edit leave
/// it as it was.
#[test]
fn a_projection_relinks_only_the_documents_it_can_change() {
    let f = Fixture::new();
    let code = f.project("code");
    let docs = f.project("docs");
    f.sync(&code, &[("wire.py", PARSER), ("util.py", CLAMP)]);
    f.drain(&code);
    let kep = "# KEP 1\n\nThe entry point `parse_header` stays; `retry_budget` is planned.\n";
    let notes = "# Notes\n\nNothing about code here.\n";
    let sync_docs = |notes: &str| {
        f.sync(&docs, &[("keps/kep-1.md", kep), ("notes.md", notes)]);
        f.drain(&docs);
    };
    sync_docs(notes);
    let first = f.stamps();
    assert_eq!(first.len(), 2, "{first:?}");
    let kep_id = "file:keps/kep-1.md";
    let changed = |before: &BTreeMap<String, String>| -> BTreeSet<String> {
        let now = f.stamps();
        now.iter()
            .filter(|(id, stamp)| before.get(*id) != Some(*stamp))
            .map(|(id, _)| id.clone())
            .collect()
    };
    // An edit of code no page names: nothing is read again.
    f.sync(
        &code,
        &[
            ("wire.py", PARSER),
            (
                "util.py",
                "def clamp(amount, low, high):\n    return min(max(amount, low), high)\n",
            ),
        ],
    );
    f.drain(&code);
    assert_eq!(changed(&first), BTreeSet::new());
    // A project registered, with no episode yet: nothing.
    let lib = f.project("lib");
    f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert_eq!(changed(&first), BTreeSet::new());
    // Its first episode defines a name the KEP found nowhere: the KEP only.
    f.sync(
        &lib,
        &[(
            "budget.py",
            "def retry_budget(attempts):\n    return attempts * 2\n",
        )],
    );
    f.drain(&lib);
    assert_eq!(changed(&first), BTreeSet::from([kep_id.to_string()]));
    let rows = f.rows(&docs);
    let budget = by_mention(&rows, "retry_budget")[0];
    assert_eq!(
        (
            budget["state"].as_str(),
            budget["target_repository_id"].as_str()
        ),
        (Some("linked"), Some(lib.as_str()))
    );
    // The file the KEP's link was read from changes, below the function.
    let second = f.stamps();
    let kept = format!("{PARSER}\n\n# Kept for the gateway.\n");
    f.sync(&code, &[("wire.py", kept.as_str()), ("util.py", CLAMP)]);
    f.drain(&code);
    assert_eq!(changed(&second), BTreeSet::from([kep_id.to_string()]));
    // Another page's edit leaves the KEP alone.
    let third = f.stamps();
    sync_docs("# Notes\n\nStill nothing about code.\n");
    assert_eq!(
        changed(&third),
        BTreeSet::from(["file:notes.md".to_string()])
    );
    // The function is renamed: the KEP's link goes.
    let fourth = f.stamps();
    let renamed = PARSER.replace("parse_header", "parse_header_line");
    f.sync(&code, &[("wire.py", renamed.as_str()), ("util.py", CLAMP)]);
    f.drain(&code);
    assert_eq!(changed(&fourth), BTreeSet::from([kep_id.to_string()]));
    assert_eq!(
        by_mention(&f.rows(&docs), "parse_header")[0]["state"],
        "unresolved"
    );
}

/// Findings 3 and 8: the SDK's conversation sessions (`graphicle_sdk.
/// memory`, metadata `schema` `graphicle.memory-session.v1`) are
/// conversation memory: importing them queues no `link`
/// job, and they are never read for links. Another document of the same
/// connector is.
#[test]
fn sdk_sessions_are_never_linked_and_queue_no_link_job() {
    let f = Fixture::new();
    let repo = f.project("memory-api:user_id:alice");
    f.sync(&repo, &[("wire.py", PARSER)]);
    f.drain(&repo);
    let connector = f.jsonl(&repo, "memory-api sessions");
    f.import(
        &repo,
        &connector,
        json!([{"id":"s-1","title":"Conversation","content":"user: why does `parse_header` fail?\n\nassistant: it splits on the colon.",
            "metadata":{"schema":doc_links::SDK_SESSION_SCHEMA,"user_id":"alice"}}]),
    );
    assert_eq!(f.queued_links(), 0);
    f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert!(f.rows(&repo).is_empty(), "{:#?}", f.rows(&repo));
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    assert_eq!(coverage["coverage"]["pending_documents"], 0, "{coverage}");
    let packet = f.context(
        &repo,
        json!({"question":"why does it fail","doc_links":true}),
    );
    assert_eq!(
        packet["doc_links"]["pending_documents"], 0,
        "{}",
        packet["doc_links"]
    );
    f.import(
        &repo,
        &connector,
        json!([{"id":"t-1","title":"Ticket","content":"See `parse_header`."}]),
    );
    assert_eq!(f.queued_links(), 1);
    f.drain(&repo);
    let rows = f.rows(&repo);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert!(s(&rows[0], "document_id").ends_with(":t-1"));
}

/// A store whose link tables are new, as after an upgrade,
/// queues the workspace's `link` job when it is opened, and the next drain
/// links its documents; rows of other rules do the same; a current store
/// queues nothing.
#[test]
fn an_upgraded_store_links_its_documents_at_the_next_drain() {
    let f = Fixture::new();
    let repo = gateway(&f);
    assert!(!f.rows(&repo).is_empty());
    f.store
        .transaction(|db| {
            db.execute_batch(
                "DROP TABLE gf_doc_links; DROP TABLE gf_doc_link_sources;
                 DROP TABLE gf_doc_link_checkpoints; DROP TABLE gf_doc_link_state;",
            )?;
            Ok(())
        })
        .unwrap();
    let reopened = Store::open(f._dir.path()).unwrap();
    assert_eq!(f.queued_links(), 1);
    jobs::drain_projection(&reopened, &repo).unwrap();
    assert!(!f.rows(&repo).is_empty());
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    assert_eq!(coverage["coverage"]["pending_documents"], 0, "{coverage}");
    // Rows of other rules.
    f.store
        .transaction(|db| {
            db.execute("UPDATE gf_doc_link_state SET rules='doc-links/0'", [])?;
            Ok(())
        })
        .unwrap();
    Store::open(f._dir.path()).unwrap();
    assert_eq!(f.queued_links(), 1);
    jobs::drain_projection(&reopened, &repo).unwrap();
    // A current store queues nothing.
    Store::open(f._dir.path()).unwrap();
    assert_eq!(f.queued_links(), 0);
}

/// Rows another producer wrote stay when the
/// `link` job rewrites a document's rows, and this program reads them only
/// while an extension of that name is installed (none is, in Core);
/// coverage counts linked mentions by their own rung.
#[test]
fn another_producers_rows_stay_and_are_not_read_without_it() {
    let f = Fixture::new();
    let repo = gateway(&f);
    f.store
        .transaction(|db| {
            db.execute(
                "INSERT INTO gf_doc_links(repository_id,document_id,document_sha256,source_kind,byte_start,byte_end,\
                 mention,mark,state,scope,candidates,target_repository_id,stable_id,target_relpath,target_sha256,\
                 target_start,target_end,target,rung,producer,created_at) \
                 SELECT repository_id,document_id,document_sha256,source_kind,byte_start,byte_end,mention,'lexical',\
                 state,scope,candidates,target_repository_id,stable_id,target_relpath,target_sha256,target_start,\
                 target_end,target,'lexical','an-extension',created_at FROM gf_doc_links WHERE repository_id=? AND state='linked'",
                [&repo],
            )?;
            Ok(())
        })
        .unwrap();
    let extension =
        || f.count("SELECT count(*) AS n FROM gf_doc_links WHERE producer='an-extension'");
    assert_eq!(extension(), 1);
    // The page changes: the job rewrites its own rows only.
    let edited = format!("{PROTOCOL}\nSee also the release notes.\n");
    f.sync(
        &repo,
        &[
            ("wire.py", PARSER),
            ("util.py", CLAMP),
            ("docs/protocol.md", edited.as_str()),
            ("docs/release.md", RELEASE),
        ],
    );
    f.drain(&repo);
    assert_eq!(extension(), 1);
    // Not read: one mention on the page, linked at rung `mention`.
    let page = f
        .read(
            &f.key,
            json!({"repository_id":repo,"document_id":"file:docs/protocol.md"}),
        )
        .unwrap();
    assert_eq!(page["mentions"].as_array().unwrap().len(), 1, "{page}");
    let coverage = f.read(&f.key, json!({"repository_id":repo})).unwrap();
    assert_eq!(
        coverage["coverage"]["by_rung"],
        json!({"mention":1}),
        "{coverage}"
    );
    let stable = s(&f.rows(&repo)[0], "stable_id").to_string();
    let back = f
        .read(&f.key, json!({"repository_id":repo,"stable_id":stable}))
        .unwrap();
    assert_eq!(back["documents"].as_array().unwrap().len(), 1, "{back}");
    assert_eq!(back["documents"][0]["producer"], "core");
}

/// A project whose episode cannot be read stops no one else.
/// The `link` job's import step still fails on it, but its document step
/// runs: a page naming the project's code is left as it was and read again
/// by the next run, a page that does not need it links, and the step
/// reports both.
#[test]
fn a_project_whose_episode_cannot_be_read_leaves_the_others_linking() {
    let f = Fixture::new();
    let code = f.project("code");
    let broken = f.project("broken");
    let docs = f.project("docs");
    f.sync(&code, &[("wire.py", PARSER)]);
    f.drain(&code);
    f.sync(
        &broken,
        &[("legacy.py", "def legacy_entry():\n    return 0\n")],
    );
    f.drain(&broken);
    let pages = |suffix: &str| {
        let a = format!("# A\n\nUses `parse_header`.{suffix}\n");
        let b = format!("# B\n\nUses `legacy_entry`.{suffix}\n");
        f.sync(&docs, &[("a.md", a.as_str()), ("b.md", b.as_str())]);
        f.drain(&docs);
    };
    pages("");
    // The broken project's episode can no longer be opened.
    let episode = f
        .store
        .read(|db| graph_projection::current_episode(db, &broken))
        .unwrap();
    f.store
        .transaction(|db| {
            db.execute(
                "UPDATE gf_episodes SET attributes='not json' WHERE repository_id=?",
                [&broken],
            )?;
            Ok(())
        })
        .unwrap();
    graphicle::session::registry().close(&graph_projection::graph_ref(&broken, &episode));
    pages(" Again.");
    let rows = f.rows(&docs);
    let a = by_mention(&rows, "parse_header")[0];
    assert_eq!(a["state"], "linked");
    let sha = graphicle_memory::text::sha256("# A\n\nUses `parse_header`. Again.\n".as_bytes());
    assert_eq!(a["document_sha256"], json!(sha));
    // The other page keeps its rows, now pending, and waits for the next run.
    let b = f
        .read(
            &f.key,
            json!({"repository_id":docs,"document_id":"file:b.md"}),
        )
        .unwrap();
    assert_eq!(b["pending"], true, "{b}");
    let owner: String = f
        .store
        .read(|db| {
            Ok(db.query_row(
                "SELECT capability_hash FROM workspaces WHERE id=?",
                [&f.workspace],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    let again = doc_links::relink(&f.store, &f.workspace, &owner, |_| Ok(())).unwrap();
    assert_eq!(again["failed"], 1, "{again}");
    assert_eq!(again["unavailable_projects"], json!([broken]));
    assert_eq!(again["relinked"], 0);
}

/// End to end: a ticket that shares no word with
/// the question reaches the packet only through its link to the function
/// the question finds. Lexical retrieval ranks it nowhere near the packet;
/// it is absent with `doc_links: false` and by default (links are then
/// only counted), and present with `doc_links: true`, joined to the
/// function's window by a link fact.
#[test]
fn a_ticket_out_of_lexical_reach_comes_only_through_its_link() {
    let f = Fixture::new();
    let repo = f.project("tickets");
    f.sync(&repo, &[("wire.py", PARSER), ("util.py", CLAMP)]);
    f.drain(&repo);
    let connector = f.jsonl(&repo, "Jira export");
    let text = "GW-7: refuse transfer encodings early at our edge; owner parse_header.";
    let at = text.find("parse_header").unwrap();
    f.import(
        &repo,
        &connector,
        json!([{"id":"GW-7","title":"GW-7","content":text,"code_spans":[[at, at + 12]]}]),
    );
    f.drain(&repo);
    let ticket = format!("connector:{connector}:GW-7");
    let question = "How is the line partitioned into name and value?";
    let lexical = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":false,"debug_candidates":400}),
    );
    let rank = lexical["candidates_debug"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["id"] == json!(ticket));
    let limit = lexical["depth"]["limit"].as_u64().unwrap() as usize;
    assert!(rank.is_none_or(|rank| rank >= limit), "BM25 rank {rank:?}");
    assert!(!cites(&lexical, &ticket));
    let default = f.context(&repo, json!({"question":question,"graph_retrieval":false}));
    assert!(!cites(&default, &ticket));
    assert!(default["doc_links"]["withheld_connector"].as_u64().unwrap() >= 1);
    let linked = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true}),
    );
    assert!(cites(&linked, "file:wire.py"));
    let (_, facts) = windows(&linked)
        .into_iter()
        .find(|(id, _)| *id == ticket)
        .unwrap_or_else(|| panic!("no ticket: {:#}", linked["doc_links"]));
    assert_eq!(facts["via"], "link");
    assert_eq!(facts["direction"], "code_to_doc");
    assert_eq!(facts["mention"], "parse_header");
    assert_eq!(facts["mark"], "code_mark");
    assert!(s(&facts, "from").starts_with("file:wire.py"), "{facts}");
}

/// How a mention resolved across projects reads as unresolved
/// for a reader who may not read every project of the workspace, whatever
/// the projects they may not read define: the outcome says nothing about
/// them.
#[test]
fn outcomes_across_projects_say_nothing_of_projects_a_reader_cannot_read() {
    for hidden_defines in [false, true] {
        let f = Fixture::new();
        let docs = f.project("docs");
        let shown = f.project("shown");
        let hidden = f.project("hidden");
        f.sync(&shown, &[("wire.py", PARSER)]);
        f.drain(&shown);
        f.sync(
            &hidden,
            &[("lib.py", if hidden_defines { PARSER } else { CLAMP })],
        );
        f.drain(&hidden);
        f.sync(
            &docs,
            &[("kep.md", "# KEP\n\nThe parser `parse_header` stays.\n")],
        );
        f.drain(&docs);
        let page = json!({"repository_id":docs,"document_id":"file:kep.md"});
        let owner = f.read(&f.key, page.clone()).unwrap();
        assert_eq!(
            owner["mentions"][0]["state"],
            if hidden_defines {
                "ambiguous"
            } else {
                "linked"
            },
            "{owner}"
        );
        let (credential, token) = f.reader();
        f.revoke(&hidden, &credential);
        let reader = f.read(&token, page).unwrap();
        assert_eq!(reader["mentions"][0]["state"], "unresolved", "{reader}");
        assert!(reader["mentions"][0].get("candidates").is_none());
        assert!(reader["mentions"][0].get("target").is_none());
        let coverage = f.read(&token, json!({"repository_id":docs})).unwrap();
        assert_eq!(coverage["coverage"]["total"]["unresolved"], 1, "{coverage}");
        let stable = f
            .store
            .read(|db| {
                let episode = graph_projection::current_episode(db, &shown)?;
                let handle = graph_projection::resident(db, &shown, &episode)?;
                let graph: &graphicle::graph::Graph = &handle;
                Ok(graph
                    .nodes()
                    .iter()
                    .find(|n| {
                        n.attributes.get("label").and_then(Value::as_str) == Some("parse_header()")
                    })
                    .and_then(|n| n.attributes.get("stable_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string())
            })
            .unwrap();
        let back = f
            .read(&token, json!({"repository_id":shown,"stable_id":stable}))
            .unwrap();
        assert_eq!(back["documents"], json!([]), "{back}");
    }
}

/// The packet's counts, with `doc_links: true`. Lifted windows come after
/// the other candidates, and `lifted` counts those the candidate bound kept. A page
/// the `link` job has not read at its bytes yet counts in documents, apart
/// from its links for the earlier bytes, which count as links pending.
#[test]
fn lifted_windows_are_counted_after_the_candidate_bound() {
    let f = Fixture::new();
    let repo = f.project("crowded");
    let mut files: Vec<(String, String)> = (0..45)
        .map(|i| {
            (
                format!("notes/n{i:02}.md"),
                format!("# Note {i}\n\nGateway note {i}.\n"),
            )
        })
        .collect();
    files.push(("wire.py".into(), PARSER.into()));
    files.push(("docs/protocol.md".into(), PROTOCOL.into()));
    let sync = |files: &[(String, String)]| {
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        f.sync(&repo, &borrowed);
    };
    sync(&files);
    f.drain(&repo);
    let question = "Why does the gateway refuse chunked bodies?";
    let crowded = f.context(
        &repo,
        json!({"question":question,"graph_retrieval":false,"doc_links":true,"limit":40}),
    );
    let report = &crowded["doc_links"];
    assert!(report["links"].as_u64().unwrap() >= 1, "{report}");
    assert_eq!(report["lifted"], 0, "{report}");
    assert_eq!(report["over_candidate_limit"], 1, "{report}");
    assert!(!cites(&crowded, "file:wire.py"));
    let follow = json!({"question":question,"graph_retrieval":false,"doc_links":true});
    let roomy = f.context(&repo, follow.clone());
    assert_eq!(roomy["doc_links"]["lifted"], 1, "{}", roomy["doc_links"]);
    assert_eq!(roomy["doc_links"]["over_candidate_limit"], 0);
    assert_eq!(roomy["mechanisms"]["links_pending_projection"], 0);
    assert_eq!(roomy["mechanisms"]["doc_links_pending_documents"], 0);
    // The page changes and the `link` job has not run: the page waits, and
    // so does its link for the earlier bytes.
    let last = files.len() - 1;
    files[last].1 = format!("{PROTOCOL}\nOlder clients only.\n");
    sync(&files);
    let waiting = f.context(&repo, follow.clone());
    let report = &waiting["doc_links"];
    assert_eq!(report["pending_documents"], 1, "{report}");
    assert_eq!(report["pending"], 1, "{report}");
    assert_eq!(waiting["mechanisms"]["doc_links_pending_documents"], 1);
    assert_eq!(waiting["mechanisms"]["links_pending_projection"], 1);
    f.drain(&repo);
    let drained = f.context(&repo, follow);
    assert_eq!(drained["mechanisms"]["doc_links_pending_documents"], 0);
    assert_eq!(drained["mechanisms"]["links_pending_projection"], 0);
}

#[derive(Clone, Default)]
struct Seen(Arc<std::sync::Mutex<Vec<Value>>>);

/// The answer model quotes the first source it was given.
async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    seen.0.lock().unwrap().push(input.clone());
    let source = &input["sources"][0];
    let answer = json!({"answer":"from the first source","abstain":false,
        "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]});
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":answer.to_string()}]}]}),
    )
}

struct Provider {
    config: model::Config,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider() -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", axum::routing::post(inference))
        .with_state(Seen::default());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = serde_json::from_value(json!({"default_model":"fast","models":{
        "fast":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":4096}
    }}))
    .unwrap();
    Provider { config, task }
}

impl Fixture {
    /// Queue a `basic` answer and run it as a worker would: its retrieval
    /// packet.
    async fn basic(&self, repo: &str, m: &Provider, mut args: Value) -> Value {
        args["kind"] = json!("basic");
        args["repository_id"] = json!(repo);
        let queued = self
            .store
            .enqueue(&self.workspace, &self.key, &args, &m.config)
            .unwrap();
        Worker::new(self.store.clone(), m.config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        let done = self
            .store
            .job_status(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"job_id":queued["id"]}),
            )
            .unwrap();
        assert_eq!(done["state"], "complete", "{done}");
        done["result"]["context"].clone()
    }
}

/// `basic` answers and
/// MCP `recall_project` take the switch, on, off and left out (left out, a
/// link is counted, not followed: the function comes through the code arm);
/// a `basic` answer that falls back to its sources reads no link unless
/// asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_answers_and_mcp_recall_take_the_switch() {
    let f = Fixture::new();
    let repo = gateway(&f);
    let question = "Why does the gateway refuse chunked bodies?";
    // MCP `recall_project`: the switch beside the published contract.
    let app = graphicle_runtime::api::App::new(f.store.clone(), model::Config::default(), vec![])
        .unwrap();
    let call = |arguments: Value| {
        graphicle_runtime::api::mcp(
            &app,
            &f.workspace,
            &f.key,
            &repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"recall_project","arguments":arguments}}),
        )
        .unwrap()
    };
    let recall = |mut arguments: Value| {
        arguments["structured"] = json!(true);
        let reply = call(arguments);
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        reply["result"]["structuredContent"].clone()
    };
    assert!(cites(&recall(json!({"question":question})), "file:wire.py"));
    let off = recall(json!({"question":question,"doc_links":false}));
    assert!(!cites(&off, "file:wire.py"), "{:?}", windows(&off));
    assert_eq!(off["mechanisms"]["doc_links_enabled"], false);
    let on = recall(json!({"question":question,"doc_links":true,"graph_retrieval":false}));
    assert!(cites(&on, "file:wire.py"));
    assert_eq!(on["doc_links"]["status"], "followed");
    let bad = call(json!({"question":question,"doc_links":"no"}));
    assert!(bad["error"]["message"].is_string(), "{bad}");
    // `basic`, on a project with code symbols.
    let m = provider().await;
    let default = f.basic(&repo, &m, json!({"question":question})).await;
    assert_eq!(default["integration"]["doc_links"], false);
    assert_eq!(default["mechanisms"]["doc_links_enabled"], false);
    assert_eq!(default["integration"]["requested"], json!([]));
    assert!(cites(&default, "file:wire.py"), "{:?}", windows(&default));
    let off = f
        .basic(&repo, &m, json!({"question":question,"doc_links":false}))
        .await;
    assert!(!cites(&off, "file:wire.py"), "{:?}", windows(&off));
    assert_eq!(off["integration"]["doc_links"], false);
    assert_eq!(off["integration"]["requested"], json!(["doc_links"]));
    assert_eq!(off["mechanisms"]["doc_links_enabled"], false);
    let on = f
        .basic(
            &repo,
            &m,
            json!({"question":question,"doc_links":true,"graph_retrieval":false}),
        )
        .await;
    assert!(cites(&on, "file:wire.py"), "{:?}", windows(&on));
    assert_eq!(on["doc_links"]["status"], "followed");
    // A code graph that cannot be read: the answer falls back to its sources
    // and follows no link, unless asked to.
    let episode = f
        .store
        .read(|db| graph_projection::current_episode(db, &repo))
        .unwrap();
    f.store
        .transaction(|db| {
            db.execute(
                "UPDATE gf_episodes SET attributes='not json' WHERE repository_id=?",
                [&repo],
            )?;
            Ok(())
        })
        .unwrap();
    graphicle::session::registry().close(&graph_projection::graph_ref(&repo, &episode));
    // (`evidence_limit` keeps the job from replaying the first answer.)
    let fallback = f
        .basic(&repo, &m, json!({"question":question,"evidence_limit":9}))
        .await;
    assert_eq!(fallback["integration"]["default"], "source_only");
    assert_eq!(fallback["integration"]["doc_links"], false);
    assert_eq!(fallback["mechanisms"]["doc_links_enabled"], false);
    assert!(
        !cites(&fallback, "file:wire.py"),
        "{:?}",
        windows(&fallback)
    );
    let asked = f
        .basic(&repo, &m, json!({"question":question,"doc_links":true}))
        .await;
    assert!(cites(&asked, "file:wire.py"), "{:?}", windows(&asked));
}
