//! One MCP server and one setup command.
//!
//! `graphicle mcp` (and the HTTP route) serves the graph engine's navigation
//! tools under Graphify's names next to the service's tools: answered from
//! the project's recorded code-graph episode with the engine's own text, and
//! with `stable_id`, `revision`, the episode and checked `file:` byte ranges
//! in `structuredContent`. The service's statistics tool is
//! `graph_history_stats`; the pull-request tools take `github_repo` and are
//! published only with `--pull-requests`; `retrieve_context` never is. The
//! ablation switches are fixed by the server (`--set`), never listed; a tool
//! set (`--tools graph|memory`) serves one half of the server. The engine's
//! resources answer on the clustered episode.
//! `connect --graph` sets an agent up (learning tools, guard hooks), as
//! `graphicle setup` does, and `disconnect` (the easy flow's, from the
//! record `connect` keeps) removes only what `connect` wrote, restoring each
//! file it changed from its backup (every client, `--name`, `--config-path`).
//!
//! Also compiled with an edition's tools installed: they join the same list,
//! and the program the tests run is that edition's `graphicle`.
use graphicle_runtime::platform::CanonicalExt;
use graphicle_runtime::{
    api::{self, App, McpOptions, ToolSet},
    extension::McpTool,
    graph_projection, graph_tools, jobs, model, query,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

/// The service's code-graph history tools: with the engine's tools they make
/// the graph half of the server (`--tools graph`).
const GRAPH_HISTORY: [&str; 4] = [
    "graph_episodes",
    "graph_as_of",
    "graph_link",
    "graph_history_stats",
];
/// An edition's code-graph tools, also in the graph half.
const EDITION_GRAPH_TOOLS: [&str; 2] = ["change_impact", "overlay_status"];

const A: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const A_EDITED: &str = "def alpha():\n    return 10\n\ndef beta():\n    return 2\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("One server").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let mut f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: String::new(),
        };
        f.repo = f.project("one", files);
        f
    }
    /// Register a project under `key`, sync `files` into it and record its
    /// code graph.
    fn project(&self, key: &str, files: &[(&str, &str)]) -> String {
        let repo = s(
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
        .to_string();
        self.sync(&repo, files);
        jobs::drain_projection(&self.store, &repo).unwrap();
        repo
    }
    fn sync(&self, repo: &str, files: &[(&str, &str)]) {
        let args = |mut value: Value| {
            value["repository_id"] = json!(repo);
            value
        };
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &args(json!({})))
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
                &args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest})),
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
                &args(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn app(&self) -> App {
        App::new(self.store.clone(), model::Config::default(), vec![]).unwrap()
    }
    fn current(&self) -> String {
        self.store
            .read(|db| graph_projection::current_episode(db, &self.repo))
            .unwrap()
    }
}

/// One JSON-RPC request to `app` as `token`, for the fixture's project.
fn rpc(f: &Fixture, app: &App, token: &str, method: &str, params: Value) -> Value {
    api::mcp(
        app,
        &f.workspace,
        token,
        &f.repo,
        &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
    )
    .unwrap()
}

/// A call's arguments as these tests send them: recall_project and the
/// engine's tools answer with their whole packet in `structuredContent` only
/// when asked (`structured: true`), and the tests read it.
fn structured(name: &str, mut arguments: Value) -> Value {
    if name == "recall_project"
        || graph_tools::NAVIGATION.contains(&name)
        || graph_tools::PULL_REQUESTS.contains(&name)
    {
        arguments["structured"] = json!(true);
    }
    arguments
}

/// A tool call's result (`isError` false), or its message when it failed.
fn call(f: &Fixture, app: &App, name: &str, arguments: Value) -> Result<Value, String> {
    let reply = rpc(
        f,
        app,
        &f.key,
        "tools/call",
        json!({"name":name,"arguments":structured(name, arguments)}),
    );
    if let Some(error) = reply.get("error") {
        return Err(s(error, "message").to_string());
    }
    let result = &reply["result"];
    if result["isError"] == true {
        return Err(s(&result["content"][0], "text").to_string());
    }
    Ok(result.clone())
}

fn names(listed: &Value) -> Vec<String> {
    listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| s(tool, "name").to_string())
        .collect()
}

/// Tool names are unique in every flag
/// combination; the default server lists the service's tools, then the
/// engine's navigation tools under Graphify's names (the service's own
/// statistics tool renamed `graph_history_stats`); the pull-request tools
/// appear only when asked for, with `github_repo`; `retrieve_context` never.
/// The two tool sets partition the server: every tool
/// that reads the code graph, the graph-history tools and an edition's
/// code-graph tools included, is in the graph half, and every other tool in
/// the memory half.
#[test]
fn every_flag_combination_lists_each_tool_once() {
    let _f = Fixture::new(&[("a.py", A)]);
    for learning in [false, true] {
        for work_state in [false, true] {
            for pull_requests in [false, true] {
                let list = |tools: ToolSet| {
                    names(&api::mcp_tools_for(
                        learning,
                        work_state,
                        &McpOptions {
                            tools,
                            pull_requests,
                            ..Default::default()
                        },
                    ))
                };
                let (all, graph, memory) = (
                    list(ToolSet::All),
                    list(ToolSet::Graph),
                    list(ToolSet::Memory),
                );
                for listed in [&all, &graph, &memory] {
                    let unique: BTreeSet<&String> = listed.iter().collect();
                    assert_eq!(unique.len(), listed.len(), "{listed:?}");
                    assert!(!listed.contains(&"retrieve_context".to_string()));
                }
                // The halves partition the whole server, each in its order.
                let in_graph: Vec<String> = all
                    .iter()
                    .filter(|name| graph.contains(name))
                    .cloned()
                    .collect();
                let rest: Vec<String> = all
                    .iter()
                    .filter(|name| !graph.contains(name))
                    .cloned()
                    .collect();
                assert_eq!(in_graph, graph);
                assert_eq!(rest, memory);
                // The essentials: the five tools, and the tools of the opt-ins
                // asked for, in the whole server's order.
                let essentials: Vec<String> = all
                    .iter()
                    .filter(|name| {
                        api::ESSENTIAL_TOOLS.contains(&name.as_str())
                            || (learning && *name == "propose_memory")
                            || (work_state && name.starts_with("work_state_"))
                            || (pull_requests
                                && graph_tools::PULL_REQUESTS.contains(&name.as_str()))
                    })
                    .cloned()
                    .collect();
                assert_eq!(list(ToolSet::Essentials), essentials);
                for name in graph_tools::NAVIGATION.iter().chain(&GRAPH_HISTORY) {
                    assert!(graph.contains(&name.to_string()), "{name}: {graph:?}");
                }
                for name in EDITION_GRAPH_TOOLS {
                    let listed = all.contains(&name.to_string());
                    assert_eq!(graph.contains(&name.to_string()), listed, "{name}");
                }
                assert_eq!(graph.contains(&"list_prs".to_string()), pull_requests);
                assert_eq!(all.contains(&"list_prs".to_string()), pull_requests);
                for name in [
                    "recall_project",
                    "read_project_source",
                    "repository_status",
                    "project_memory",
                    "search_project_memory",
                    "session_timeline",
                ] {
                    assert!(memory.contains(&name.to_string()), "{name}: {memory:?}");
                }
                assert_eq!(memory.contains(&"propose_memory".to_string()), learning);
                assert_eq!(memory.contains(&"work_state_write".to_string()), work_state);
            }
        }
    }
    let listed = names(&api::mcp_tools(false));
    assert!(listed.contains(&"graph_history_stats".to_string()));
    // `graph_stats` is the engine's (Graphify's), once.
    assert_eq!(listed.iter().filter(|n| *n == "graph_stats").count(), 1);
    // A project connection's core tools: `list_projects` is a workspace
    // connection's, the other 23 as the one server lists them, `pack` last.
    let core: Vec<String> = api::core_mcp_tools(false, false)
        .into_iter()
        .filter_map(McpTool::confined)
        .map(|tool| tool.name().to_string())
        .collect();
    assert_eq!(core.len(), 23, "{core:?}");
    assert_eq!(core[15..22], graph_tools::NAVIGATION.map(String::from));
    assert_eq!(core[22], "pack");
    assert_eq!(listed[..23], core[..], "the core tools lead the list");
    assert!(!listed.contains(&"list_projects".to_string()));
    let pull = api::mcp_tools_for(
        false,
        false,
        &McpOptions {
            pull_requests: true,
            ..Default::default()
        },
    );
    for tool in pull["tools"].as_array().unwrap() {
        let properties = &tool["inputSchema"]["properties"];
        assert!(properties.get("project_path").is_none(), "{tool}");
        if graph_tools::PULL_REQUESTS.contains(&s(tool, "name")) {
            assert!(properties.get("repo").is_none(), "{tool}");
            assert!(properties.get("github_repo").is_some(), "{tool}");
        }
        if graph_tools::NAVIGATION.contains(&s(tool, "name")) {
            assert!(properties.get("repo").is_some() && properties.get("as_of").is_some());
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
        }
    }
}

/// The text a graph tool's own server answers on the same graph: the
/// resident episode written as a graph.json and served by the engine's
/// `Server`, as `graphicle graph serve` would serve it.
fn engine_answer(f: &Fixture, episode: &str, name: &str, args: &Value) -> String {
    let resident = f
        .store
        .read(|db| graph_tools::resident(db, &f.repo, episode))
        .unwrap();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(".graphicle/graph.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&resident.context.graph.to_node_link("links")).unwrap(),
    )
    .unwrap();
    graphicle::serve::Server::new(&path)
        .call_tool(name, args)
        .unwrap()
}

/// Each navigation tool answers with the engine's own text on the episode,
/// and cites the nodes it names: `stable_id`, `revision`, the episode and,
/// with the file's current bytes, a checked `file:` range whose bytes are
/// the symbol's.
#[test]
fn graph_tools_answer_with_the_engines_text_and_checked_citations() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let app = f.app();
    let episode = f.current();
    for (name, args) in [
        ("query_graph", json!({"question":"run alpha beta"})),
        ("get_node", json!({"label":"alpha"})),
        ("get_neighbors", json!({"label":"alpha"})),
        ("god_nodes", json!({"top_n":3})),
        ("graph_stats", json!({})),
        (
            "shortest_path",
            json!({"source":"run","target":"beta","undirected":true}),
        ),
    ] {
        let result = call(&f, &app, name, args.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        let structured = &result["structuredContent"];
        assert_eq!(result["content"][0]["text"], structured["text"], "{name}");
        // The engine's server heads a query with its graph file, which this
        // one does not have; the rest is the same text.
        let engine = engine_answer(&f, &episode, name, &args);
        let engine = match engine.strip_prefix("Graph: ") {
            Some(rest) if name == "query_graph" => rest.split_once(" | ").unwrap().1.to_string(),
            _ => engine,
        };
        assert_eq!(
            s(structured, "text"),
            engine,
            "{name}: the engine's own text"
        );
        assert_eq!(structured["episode"], episode.as_str());
        assert_eq!(structured["episode_lag"], 0);
        assert_eq!(structured["repo"], f.repo.as_str());
        if name != "graph_stats" {
            assert!(
                !structured["nodes"].as_array().unwrap().is_empty(),
                "{name} cites nothing: {structured:#}"
            );
        }
        // Every line that names a node is read (the formats the citations
        // parse are pinned here): nothing unmatched, and as many cited nodes
        // as the text names.
        assert!(
            structured.get("nodes_unmatched").is_none(),
            "{name}: {structured:#}"
        );
        let text = s(structured, "text");
        let cited = structured["nodes"].as_array().unwrap().len();
        let lines = |prefix: &str| text.lines().filter(|l| l.starts_with(prefix)).count();
        match name {
            "query_graph" => assert_eq!(cited, lines("NODE "), "{text}"),
            "get_neighbors" => assert_eq!(cited, 1 + lines("  --> ") + lines("  <-- "), "{text}"),
            "god_nodes" => assert_eq!(cited, lines("  "), "{text}"),
            "shortest_path" => {
                let hops: usize = text
                    .split_once("Shortest path (")
                    .and_then(|(_, rest)| rest.split_once(" hops"))
                    .and_then(|(hops, _)| hops.parse().ok())
                    .unwrap_or_else(|| panic!("{text}"));
                assert_eq!(cited, hops + 1, "{text}");
            }
            _ => {}
        }
        // `content` passes the citations on to clients that show the model
        // `content` only: the engine's text first, unchanged, then the block.
        let block = s(&result["content"][1], "text");
        let head = format!("Citations (code-graph episode {}…, current", &episode[..12]);
        assert!(block.starts_with(&head), "{name}: {block}");
        for node in structured["nodes"].as_array().unwrap() {
            let shown = format!("\n- {} ", s(node, "label"));
            assert!(block.contains(&shown), "{name}: {shown} in {block}");
        }
        assert!(!block.contains("not checked"), "{name}: {block}");
        for node in structured["nodes"].as_array().unwrap() {
            assert!(node["stable_id"].is_string(), "{name}: {node}");
            assert!(
                node["revision"].is_string() || node["kind"] == "file",
                "{name}: {node}"
            );
            assert_eq!(node["episode"], episode.as_str());
            if node["checked"] == true {
                let source = if node["relpath"] == "a.py" { A } else { B };
                let bytes = &source[node["byte_start"].as_u64().unwrap() as usize
                    ..node["byte_end"].as_u64().unwrap() as usize];
                assert_eq!(
                    node["sha256"],
                    graphicle_memory::text::sha256(source.as_bytes())
                );
                // A checked range is the bytes the node's `revision` hashes:
                // a symbol's own lines, a file node's whole file.
                assert_eq!(
                    node["revision"],
                    graphicle_memory::text::sha256(bytes.as_bytes()),
                    "{name}: {node}"
                );
                if node["kind"] == "file" {
                    assert_eq!(bytes, source, "{node}");
                }
                if node["label"] == "alpha()" {
                    assert_eq!(bytes, "def alpha():\n    return 1\n\n", "{node}");
                    assert_eq!(node["source_id"], "file:a.py");
                    assert_eq!(
                        (node["line"].clone(), node["end_line"].clone()),
                        (json!(1), json!(3))
                    );
                }
            }
        }
    }
    let node = call(&f, &app, "get_node", json!({"label":"alpha"})).unwrap();
    let cited = &node["structuredContent"]["nodes"][0];
    assert_eq!(cited["label"], "alpha()", "{cited}");
    assert_eq!(cited["checked"], true);
    let neighbours = call(&f, &app, "get_neighbors", json!({"label":"alpha"})).unwrap();
    let labels: Vec<&str> = neighbours["structuredContent"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| s(node, "label"))
        .collect();
    assert_eq!(labels[0], "alpha()");
    assert!(labels.contains(&"run()"), "{labels:?}");
    let path = call(
        &f,
        &app,
        "shortest_path",
        json!({"source":"run","target":"beta","undirected":true}),
    )
    .unwrap();
    let labels: Vec<&str> = path["structuredContent"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| s(node, "label"))
        .collect();
    assert_eq!(labels.first(), Some(&"run()"), "{labels:?}");
    assert_eq!(labels.last(), Some(&"beta()"), "{labels:?}");
    // The episode is clustered: the node's community answers get_community.
    let community = cited["community"].as_i64().expect("a community number");
    let members = call(&f, &app, "get_community", json!({"community_id":community})).unwrap();
    let structured = &members["structuredContent"];
    assert_eq!(
        s(structured, "text"),
        engine_answer(
            &f,
            &episode,
            "get_community",
            &json!({"community_id":community})
        )
    );
    assert!(
        structured["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["label"] == "alpha()"),
        "{structured:#}"
    );
    // The engine's own server reads graph files; this one, its store.
    let refused = call(
        &f,
        &app,
        "query_graph",
        json!({"question":"alpha","project_path":"/tmp"}),
    )
    .unwrap_err();
    assert!(refused.contains("not project_path"), "{refused}");
}

/// While the current head set waits for its projection, the graph tools
/// read the latest episode, say so, and check only the files whose bytes it
/// recorded; `as_of` names an episode.
#[test]
fn a_waiting_projection_and_as_of_read_the_recorded_episode() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let app = f.app();
    let first = f.current();
    f.sync(&f.repo, &[("a.py", A_EDITED), ("b.py", B)]);
    let current = f.current();
    assert_ne!(first, current);
    let query =
        |args: Value| call(&f, &app, "query_graph", args).unwrap()["structuredContent"].clone();
    let lagging = query(json!({"question":"run alpha beta"}));
    assert_eq!(lagging["episode"], first.as_str());
    assert_eq!(lagging["current_episode"], current.as_str());
    assert_eq!(lagging["episode_lag"], 1);
    let by_file = |packet: &Value, file: &str| -> Vec<Value> {
        packet["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["relpath"] == file)
            .cloned()
            .collect()
    };
    let changed = by_file(&lagging, "a.py");
    assert!(!changed.is_empty(), "{lagging:#}");
    for node in changed {
        assert_eq!(node["checked"], false, "{node}");
        assert_eq!(node["reason"], "changed_since_episode");
        assert!(node.get("byte_start").is_none());
    }
    assert!(
        by_file(&lagging, "b.py")
            .iter()
            .all(|node| node["checked"] == true),
        "{lagging:#}"
    );
    // A relation read from the changed file may no longer hold (the rule the
    // code-graph arm follows): alpha's neighbour a.py is reached only
    // through `contains`, read from a.py, and is flagged; run() calls alpha
    // in b.py, which did not change.
    let neighbours = call(&f, &app, "get_neighbors", json!({"label":"alpha"})).unwrap();
    let structured = &neighbours["structuredContent"];
    let cited = |pick: &dyn Fn(&Value) -> bool| -> Value {
        structured["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| pick(node))
            .cloned()
            .unwrap_or_else(|| panic!("{structured:#}"))
    };
    let file = cited(&|node| node["kind"] == "file" && node["relpath"] == "a.py");
    assert_eq!(file["relation_checked"], false, "{structured:#}");
    let caller = cited(&|node| node["label"] == "run()");
    assert_eq!(caller["relation_checked"], true, "{structured:#}");
    assert!(structured["relations_unchecked"].as_u64().unwrap() >= 1);
    assert!(structured["nodes"][0].get("relation_checked").is_none());
    let block = s(&neighbours["content"][1], "text");
    assert!(
        block.contains(
            "waits for its projection: 1 file changed since, and relations read from changed files may no longer hold"
        ),
        "{block}"
    );
    let line = |start: String| -> String {
        block
            .lines()
            .find(|line| line.starts_with(&start))
            .unwrap_or_else(|| panic!("{start}: {block}"))
            .to_string()
    };
    const STALE: &str = "(reached only through relations read from changed files)";
    const CHANGED: &str = "(not checked: changed_since_episode)";
    let center = line("- alpha() file:a.py".into());
    assert!(
        center.contains(CHANGED) && !center.contains(STALE),
        "{center}"
    );
    let stale = line(format!("- {} file:a.py", s(&file, "label")));
    assert!(stale.contains(STALE), "{stale}");
    let fresh = line("- run() file:b.py".into());
    assert!(
        !fresh.contains(STALE) && !fresh.contains("not checked"),
        "{fresh}"
    );
    jobs::drain_projection(&f.store, &f.repo).unwrap();
    let now = query(json!({"question":"run alpha beta"}));
    assert_eq!(now["episode"], current.as_str());
    assert_eq!(now["episode_lag"], 0);
    assert!(now.get("relations_unchecked").is_none(), "{now:#}");
    assert!(
        now["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node.get("relation_checked").is_none())
    );
    let before = query(json!({"question":"run alpha beta","as_of":first}));
    assert_eq!(before["episode"], first.as_str());
    assert_eq!(before["as_of"], first.as_str());
    assert!(
        by_file(&before, "a.py")
            .iter()
            .all(|node| node["checked"] == false)
    );
    let unknown = call(
        &f,
        &app,
        "query_graph",
        json!({"question":"alpha","as_of":"no-such-episode"}),
    )
    .unwrap_err();
    assert!(unknown.contains("Unknown episode"), "{unknown}");
}

/// One identity across the tools: the `stable_id` a graph tool cites for a
/// function is the one on that function's `recall_project` window.
#[test]
fn query_graph_and_recall_cite_one_stable_id() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let app = f.app();
    let graph = call(
        &f,
        &app,
        "query_graph",
        json!({"question":"where is beta defined"}),
    )
    .unwrap();
    let from_graph = graph["structuredContent"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["label"] == "beta()")
        .unwrap_or_else(|| panic!("{:#}", graph["structuredContent"]))
        .clone();
    let recall = call(
        &f,
        &app,
        "recall_project",
        json!({"question":"where is beta defined"}),
    )
    .unwrap();
    let from_recall: Vec<Value> = recall["structuredContent"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|window| window.get("code_graph").cloned())
        .collect();
    let beta = from_recall
        .iter()
        .find(|fact| fact["label"] == "beta()")
        .unwrap_or_else(|| panic!("{from_recall:?}"));
    assert_eq!(beta["stable_id"], from_graph["stable_id"]);
    assert_eq!(beta["revision"], from_graph["revision"]);
    assert_eq!(beta["episode"], from_graph["episode"]);
}

/// `repo` names a project of the workspace by id or key, and over stdio by
/// its folder, always authorized for the caller: a GitHub slug never becomes
/// a project, and a key whose read access to a project was revoked reaches
/// it through no `repo`. The pull-request tools take `github_repo`.
#[test]
fn repo_names_only_readable_projects_and_never_a_github_slug() {
    let f = Fixture::new(&[("a.py", A)]);
    let other = f.project("other-key", &[("b.py", B)]);
    let folder = TempDir::new().unwrap();
    let folder_path = folder.path().canonical().unwrap();
    {
        let db = f.store.connect().unwrap();
        graphicle_runtime::folders::record(&db, &f.workspace, &other, &folder_path).unwrap();
    }
    let mut app = f.app();
    let read = |app: &App, token: &str, repo: &str| -> Result<Value, String> {
        let reply = rpc(
            &f,
            app,
            token,
            "tools/call",
            json!({"name":"get_node","arguments":{"label":"run","repo":repo,"structured":true}}),
        );
        if reply["result"]["isError"] == true || reply.get("error").is_some() {
            return Err(reply.to_string());
        }
        Ok(reply["result"]["structuredContent"].clone())
    };
    // Over HTTP: an id or a key; never a path.
    assert_eq!(read(&app, &f.key, &other).unwrap()["repo"], other.as_str());
    assert_eq!(
        read(&app, &f.key, "other-key").unwrap()["repo"],
        other.as_str()
    );
    let path = folder_path.to_string_lossy().into_owned();
    assert!(
        read(&app, &f.key, &path)
            .unwrap_err()
            .contains("Unknown project")
    );
    for slug in ["octocat/hello-world", "graphicle-ai/graphicle"] {
        let refused = read(&app, &f.key, slug).unwrap_err();
        assert!(refused.contains("Unknown project"), "{refused}");
    }
    // Over stdio a synced folder names its project too.
    app.mcp.cwd = Some(folder_path.clone());
    assert_eq!(read(&app, &f.key, &path).unwrap()["repo"], other.as_str());
    assert_eq!(read(&app, &f.key, ".").unwrap()["repo"], other.as_str());
    assert!(
        read(&app, &f.key, "octocat/hello-world")
            .unwrap_err()
            .contains("Unknown project")
    );
    // A reader whose access to `other` is revoked reaches it no way.
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"restricted"}),
        )
        .unwrap();
    let token = s(&reader, "capability").to_string();
    assert!(read(&app, &token, &other).is_ok());
    f.store
        .graph_access(
            &f.workspace,
            &f.key,
            &json!({"repository_id":other,"credential_id":reader["id"],"state":"revoked"}),
        )
        .unwrap();
    for named in [other.as_str(), "other-key", path.as_str(), "."] {
        let refused = read(&app, &token, named).unwrap_err();
        assert!(!refused.contains("\"repo\""), "{named}: {refused}");
    }
    assert_eq!(
        read(&app, &token, &f.repo).unwrap()["repo"],
        f.repo.as_str()
    );
    // The pull-request tools: only when asked for, and `repo` is no GitHub
    // repository there.
    assert!(call(&f, &app, "list_prs", json!({})).is_err());
    app.mcp.pull_requests = true;
    let refused = call(&f, &app, "list_prs", json!({"repo":"octocat/hello-world"})).unwrap_err();
    assert!(refused.contains("published contract"), "{refused}");
}

/// A switch the server fixes (`--set`) is
/// listed in no schema, applies to every call, is reported in `mechanisms`,
/// and a request for another value is refused.
#[test]
fn server_fixed_switches_apply_are_reported_and_cannot_be_overridden() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let mut app = f.app();
    app.mcp.fixed = api::fixed_switches(&["graph_retrieval=false".into()]).unwrap();
    let recall = |arguments: Value| call(&f, &app, "recall_project", arguments);
    let packet = recall(json!({"question":"who calls beta"})).unwrap()["structuredContent"].clone();
    assert_eq!(packet["mechanisms"]["graph_retrieval_enabled"], false);
    assert_eq!(packet["code_graph"]["status"], "disabled");
    assert_eq!(
        packet["mechanisms"]["server_fixed"],
        json!({"graph_retrieval":false})
    );
    assert!(
        recall(json!({"question":"who calls beta","graph_retrieval":false})).is_ok(),
        "the fixed value itself is accepted"
    );
    let refused = recall(json!({"question":"who calls beta","graph_retrieval":true})).unwrap_err();
    assert!(refused.contains("fixed to false"), "{refused}");
    let listed = rpc(&f, &app, &f.key, "tools/list", json!({}));
    for tool in listed["result"]["tools"].as_array().unwrap() {
        for switch in api::RECALL_SWITCHES {
            assert!(
                tool["inputSchema"]["properties"].get(switch).is_none(),
                "{tool}"
            );
        }
    }
    for bad in ["code_graph=false", "graph_retrieval=no", "graph_retrieval"] {
        assert!(api::fixed_switches(&[bad.into()]).is_err(), "{bad}");
    }
    // A switch is given once: a repeated --set is refused, whatever its value.
    for twice in [
        ["graph_retrieval=false", "graph_retrieval=false"],
        ["graph_retrieval=false", "graph_retrieval=true"],
    ] {
        let refused = api::fixed_switches(&twice.map(String::from)).unwrap_err();
        assert!(refused.message.contains("set twice"), "{}", refused.message);
    }
    // memory_retrieval is fixed the same way, alone or with graph_retrieval.
    let mut memory_off = f.app();
    memory_off.mcp.fixed = api::fixed_switches(&["memory_retrieval=false".into()]).unwrap();
    let packet = call(
        &f,
        &memory_off,
        "recall_project",
        json!({"question":"who calls beta"}),
    )
    .unwrap()["structuredContent"]
        .clone();
    assert_eq!(packet["mechanisms"]["reviewed_memory_enabled"], false);
    assert_eq!(packet["mechanisms"]["graph_retrieval_enabled"], true);
    assert_eq!(
        packet["mechanisms"]["server_fixed"],
        json!({"memory_retrieval":false})
    );
    let refused = call(
        &f,
        &memory_off,
        "recall_project",
        json!({"question":"who calls beta","memory_retrieval":true}),
    )
    .unwrap_err();
    assert!(
        refused.contains("memory_retrieval is fixed to false"),
        "{refused}"
    );
    memory_off.mcp.fixed = api::fixed_switches(&[
        "memory_retrieval=false".into(),
        "graph_retrieval=false".into(),
    ])
    .unwrap();
    let packet = call(
        &f,
        &memory_off,
        "recall_project",
        json!({"question":"who calls beta"}),
    )
    .unwrap()["structuredContent"]
        .clone();
    assert_eq!(
        packet["mechanisms"]["server_fixed"],
        json!({"memory_retrieval":false,"graph_retrieval":false})
    );
    assert_eq!(packet["mechanisms"]["reviewed_memory_enabled"], false);
    assert_eq!(packet["mechanisms"]["graph_retrieval_enabled"], false);
    // `doc_links` is a recall switch too, so a
    // server fixes it the same way.
    let mut links_off = f.app();
    links_off.mcp.fixed = api::fixed_switches(&["doc_links=false".into()]).unwrap();
    let packet = call(
        &f,
        &links_off,
        "recall_project",
        json!({"question":"who calls beta"}),
    )
    .unwrap()["structuredContent"]
        .clone();
    assert_eq!(packet["mechanisms"]["doc_links_enabled"], false);
    assert_eq!(
        packet["mechanisms"]["server_fixed"],
        json!({"doc_links":false})
    );
    let refused = call(
        &f,
        &links_off,
        "recall_project",
        json!({"question":"who calls beta","doc_links":true}),
    )
    .unwrap_err();
    assert!(refused.contains("doc_links is fixed to false"), "{refused}");
    let refused = api::fixed_switches(&["code_graph=false".into()]).unwrap_err();
    assert!(
        refused.message.contains(
            "the server fixes graph_retrieval, memory_retrieval, doc_links, symbol_links, cross_repo, notes_first, identifier_route, code_first, query_focus, identifier_arm, more_files and scope"
        ),
        "{}",
        refused.message
    );
    // Without fixed switches a packet carries no `server_fixed`.
    let plain = call(
        &f,
        &f.app(),
        "recall_project",
        json!({"question":"who calls beta"}),
    )
    .unwrap();
    assert!(
        plain["structuredContent"]["mechanisms"]
            .get("server_fixed")
            .is_none()
    );
}

/// One store, two servers. `--tools graph` serves every tool that
/// reads the code graph (the engine's tools and resources, the graph-history
/// tools, an edition's code-graph tools), `--tools memory` the other tools,
/// each with its own instructions (the history tools, which walk the
/// typed code graph too, left the memory half). The instructions say which
/// tools run commands.
#[test]
fn a_tool_set_serves_one_half_of_the_server() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let mut app = f.app();
    let initialize = |app: &App| {
        rpc(
            &f,
            app,
            &f.key,
            "initialize",
            json!({"protocolVersion":"2025-11-25"}),
        )["result"]
            .clone()
    };
    let all = initialize(&app);
    assert!(all["capabilities"].get("resources").is_some());
    let text = s(&all, "instructions");
    assert!(
        text.contains("query_graph") && text.contains("recall_project"),
        "{text}"
    );
    assert!(
        text.contains("No tool can approve memory or run commands."),
        "{text}"
    );
    assert!(text.contains("then a citations block"), "{text}");
    // With the pull-request tools the sentence names the commands they run.
    app.mcp.pull_requests = true;
    let text = s(&initialize(&app), "instructions").to_string();
    assert!(!text.contains("or run commands"), "{text}");
    assert!(
        text.contains("no tool runs commands except list_prs, get_pr_impact and triage_prs, which run the GitHub CLI and git, read-only, in the project's folder"),
        "{text}"
    );
    app.mcp.pull_requests = false;

    app.mcp.tools = ToolSet::Graph;
    let graph = initialize(&app);
    assert!(graph["capabilities"].get("resources").is_some());
    assert!(!s(&graph, "instructions").contains("recall_project"));
    let listed = names(&rpc(&f, &app, &f.key, "tools/list", json!({}))["result"]);
    let expected: Vec<String> = GRAPH_HISTORY
        .iter()
        .chain(&graph_tools::NAVIGATION)
        .map(|name| name.to_string())
        .collect();
    assert_eq!(listed[..expected.len()], expected[..], "{listed:?}");
    for extra in &listed[expected.len()..] {
        assert!(EDITION_GRAPH_TOOLS.contains(&extra.as_str()), "{listed:?}");
    }
    assert!(call(&f, &app, "recall_project", json!({"question":"alpha"})).is_err());
    assert!(call(&f, &app, "get_node", json!({"label":"alpha"})).is_ok());
    assert!(call(&f, &app, "graph_history_stats", json!({})).is_ok());
    assert!(call(&f, &app, "graph_as_of", json!({"as_of":"latest"})).is_ok());

    app.mcp.tools = ToolSet::Memory;
    let memory = initialize(&app);
    assert!(memory["capabilities"].get("resources").is_none());
    assert!(!s(&memory, "instructions").contains("query_graph"));
    let listed = names(&rpc(&f, &app, &f.key, "tools/list", json!({}))["result"]);
    assert!(listed.contains(&"recall_project".to_string()));
    for name in graph_tools::NAVIGATION
        .iter()
        .chain(&GRAPH_HISTORY)
        .chain(&EDITION_GRAPH_TOOLS)
    {
        assert!(!listed.contains(&name.to_string()), "{name}: {listed:?}");
    }
    let refused = rpc(&f, &app, &f.key, "resources/list", json!({}));
    assert!(refused.get("error").is_some(), "{refused}");
    assert!(call(&f, &app, "get_node", json!({"label":"alpha"})).is_err());
    let refused = call(&f, &app, "graph_as_of", json!({"as_of":"latest"})).unwrap_err();
    assert!(refused.contains("Tool unavailable"), "{refused}");
}

/// The 0.1.0 server, frozen but for 0.1.4's routing (a sentence on when to
/// call recall and the code graph and when grep is enough first in the
/// instructions, and one on when to use it instead of grep at the end of
/// each search tool's description): `graphicle mcp --project .
/// --allow-learning` (what setup writes for every client but Claude Code,
/// and for Claude Code with `--tools all`) initializes and lists exactly
/// this (an edition adds its tools after these).
const FROZEN_ALL: &str = include_str!("../fixtures/one_server_tools.json");
/// What setup gives Claude Code (`--tools essentials`): five tools and short
/// instructions on when to call them and when grep is enough.
const FROZEN_ESSENTIALS: &str = include_str!("../fixtures/one_server_essentials_tools.json");

/// The whole server (`--tools all`) is 0.1.0's but for the routing
/// sentences. The essentials are the five tools setup gives Claude Code:
/// the whole server's schemas, descriptions that start with what an agent
/// searches for and carry the citation details, and short instructions;
/// they answer, the other tools do not, and the server's opt-ins add their
/// tools.
#[test]
fn all_is_the_0_1_0_server_and_the_essentials_are_five_tools() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let mut app = f.app();
    let initialize = |app: &App| {
        rpc(
            &f,
            app,
            &f.key,
            "initialize",
            json!({"protocolVersion":"2025-11-25"}),
        )["result"]
            .clone()
    };
    let listed = |app: &App| -> Vec<Value> {
        rpc(&f, app, &f.key, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .clone()
    };

    app.mcp_learning = true;
    let frozen: Value = serde_json::from_str(FROZEN_ALL).unwrap();
    assert_eq!(initialize(&app)["instructions"], frozen["instructions"]);
    let whole = listed(&app);
    let core = frozen["tools"].as_array().unwrap();
    assert_eq!(json!(whole[..core.len()]), frozen["tools"]);

    app.mcp_learning = false;
    app.mcp.tools = ToolSet::Essentials;
    let frozen: Value = serde_json::from_str(FROZEN_ESSENTIALS).unwrap();
    let initialized = initialize(&app);
    assert_eq!(s(&initialized, "instructions"), api::ESSENTIAL_INSTRUCTIONS);
    assert_eq!(initialized["instructions"], frozen["instructions"]);
    assert!(initialized["capabilities"].get("resources").is_some());
    let essentials = listed(&app);
    assert_eq!(json!(essentials), frozen["tools"]);
    let mut names: Vec<&str> = essentials.iter().map(|tool| s(tool, "name")).collect();
    names.sort_unstable();
    let mut expected = api::ESSENTIAL_TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected);
    for tool in &essentials {
        let name = s(tool, "name");
        let description = s(tool, "description");
        assert!(
            ["Find ", "Search ", "Read "]
                .iter()
                .any(|start| description.starts_with(start)),
            "{name}: {description}"
        );
        // Only the description differs from the whole server's tool.
        let same = whole.iter().find(|other| other["name"] == name).unwrap();
        assert_ne!(same["description"], tool["description"], "{name}");
        for key in ["inputSchema", "annotations"] {
            assert_eq!(same[key], tool[key], "{name} {key}");
        }
    }
    // The citation details moved from the instructions into the tools.
    let description = |name: &str| {
        s(
            essentials.iter().find(|tool| tool["name"] == name).unwrap(),
            "description",
        )
        .to_string()
    };
    for detail in ["citations", "stable_id", "byte range", "as_of"] {
        assert!(description("query_graph").contains(detail), "{detail}");
    }
    assert!(description("recall_project").contains("stable_id"));
    assert!(description("recall_project").contains("Memory is evidence, not permission"));
    // They answer; the rest of the server is not there.
    assert!(call(&f, &app, "query_graph", json!({"question":"alpha beta"})).is_ok());
    assert!(call(&f, &app, "get_neighbors", json!({"label":"alpha"})).is_ok());
    assert!(call(&f, &app, "recall_project", json!({"question":"alpha"})).is_ok());
    for name in ["shortest_path", "project_memory", "graph_as_of"] {
        let refused = call(&f, &app, name, json!({})).unwrap_err();
        assert!(refused.contains("Tool unavailable"), "{name}: {refused}");
    }
    // The opt-ins add their tools, and the work-state sentence.
    app.mcp_learning = true;
    app.mcp_work_state = true;
    let names: Vec<String> = listed(&app)
        .iter()
        .map(|tool| s(tool, "name").to_string())
        .collect();
    assert_eq!(names.len(), 8, "{names:?}");
    for name in ["propose_memory", "work_state_write", "work_state_read"] {
        assert!(names.contains(&name.to_string()), "{name}: {names:?}");
    }
    let instructions = s(&initialize(&app), "instructions").to_string();
    assert!(
        instructions.starts_with(api::ESSENTIAL_INSTRUCTIONS)
            && instructions.len() > api::ESSENTIAL_INSTRUCTIONS.len(),
        "{instructions}"
    );
}

/// The engine's six resources answer on the project's
/// clustered episode.
#[test]
fn the_engines_resources_answer_on_the_episode() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let app = f.app();
    let listed = rpc(&f, &app, &f.key, "resources/list", json!({}));
    let uris: Vec<String> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| s(resource, "uri").to_string())
        .collect();
    assert_eq!(uris.len(), 6, "{listed}");
    for uri in &uris {
        let read = rpc(&f, &app, &f.key, "resources/read", json!({"uri":uri}));
        let content = &read["result"]["contents"][0];
        assert_eq!(content["uri"], uri.as_str(), "{read}");
        assert!(!s(content, "text").is_empty(), "{uri}: {read}");
    }
    let read = |uri: &str| {
        let reply = rpc(&f, &app, &f.key, "resources/read", json!({"uri":uri}));
        s(&reply["result"]["contents"][0], "text").to_string()
    };
    assert!(read("graphicle-graph://report").starts_with("# Graph Report - one"));
    let stats = read("graphicle-graph://stats");
    assert!(
        stats.starts_with("Nodes: ") && stats.contains("Communities: "),
        "{stats}"
    );
    assert!(read("graphicle-graph://audit").starts_with("Total edges: "));
    let unknown = rpc(
        &f,
        &app,
        &f.key,
        "resources/read",
        json!({"uri":"graphicle-graph://nothing"}),
    );
    assert!(unknown.get("error").is_some(), "{unknown}");
    let templates = rpc(&f, &app, &f.key, "resources/templates/list", json!({}));
    assert_eq!(templates["result"]["resourceTemplates"], json!([]));
}

/// Run `program` in `cwd` on the store `data`, with `HOME` at `home`, `env`
/// added and `input` on its standard input: whether it succeeded, and its
/// standard output and error.
fn run(
    program: &Path,
    home: &Path,
    data: &Path,
    cwd: &Path,
    args: &[&str],
    input: &str,
    env: &[(&str, &OsStr)],
) -> (bool, String, String) {
    use std::io::Write;
    let mut command = Command::new(program);
    crate::child_env::home(&mut command, home)
        .current_dir(cwd)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The built program, as [`run`] runs it with `env` added; it must succeed.
/// Its output as JSON (a string when it is not JSON) and its standard error.
fn graphicle_with(
    home: &Path,
    data: &Path,
    cwd: &Path,
    args: &[&str],
    input: &str,
    env: &[(&str, &OsStr)],
) -> (Value, String) {
    let program = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    let (ok, stdout, stderr) = run(program, home, data, cwd, args, input, env);
    assert!(ok, "{args:?}: {stderr}");
    let value = serde_json::from_str(&stdout).unwrap_or_else(|_| json!(stdout));
    (value, stderr)
}

fn graphicle(home: &Path, data: &Path, cwd: &Path, args: &[&str], input: &str) -> (Value, String) {
    graphicle_with(home, data, cwd, args, input, &[])
}

/// The replies of one stdio session of `graphicle mcp` (`args`) to
/// `requests`, by id.
#[cfg(unix)]
fn session(
    home: &Path,
    data: &Path,
    cwd: &Path,
    args: &[&str],
    requests: &[Value],
    env: &[(&str, &OsStr)],
) -> Vec<Value> {
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let program = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    let (ok, stdout, stderr) = run(program, home, data, cwd, args, &input, env);
    assert!(ok, "{args:?}: {stderr}");
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The reply with `id` among `replies`: its `result`.
#[cfg(unix)]
fn reply(replies: &[Value], id: u64) -> Value {
    replies
        .iter()
        .find(|reply| reply["id"] == id)
        .unwrap_or_else(|| panic!("no reply {id}: {replies:#?}"))["result"]
        .clone()
}

/// An executable script `name` in `folder`.
#[cfg(unix)]
fn script(folder: &Path, name: &str, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(folder).unwrap();
    let path = folder.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A folder under the test target's own temporary folder, where scripts may
/// run (the stand-ins for the engine, `codex` and `gh`).
#[cfg(unix)]
fn script_root() -> (TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("one-server")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = dir.path().canonical().unwrap();
    (dir, root)
}

/// A synced project folder `name` under `root` holding `files`; returns its
/// path and its project id.
fn synced(root: &Path, data: &Path, name: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
    let folder = root.join(name);
    std::fs::create_dir_all(&folder).unwrap();
    for (path, text) in files {
        std::fs::write(folder.join(path), text).unwrap();
    }
    let (synced, _) = graphicle(root, data, &folder, &["sync", "."], "");
    assert_eq!(synced["graph"]["status"], "built", "{synced:#}");
    (folder, s(&synced, "repository_id").to_string())
}

/// A JSON file as a client keeps it, written the way `connect` writes.
fn write_json(path: &Path, value: &Value) -> Vec<u8> {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

/// Whether a report's `file` names `file`: the same JSON string on macOS and
/// Linux; on Windows the same path, since the product spells client files
/// with `\` throughout and the tests join some with `/`.
fn reports_file(reported: &Value, file: &Path) -> bool {
    if cfg!(windows) {
        reported.as_str().is_some_and(|s| Path::new(s) == file)
    } else {
        *reported == json!(file)
    }
}

/// End to end: after `sync`
/// and `connect claude-code --graph --guards --tools all` (0.1.0's setup for
/// Claude Code), one stdio session of the entry's own
/// command lists the graph tools, the recall tool and the memory proposal
/// (`--graph` implies `--allow-learning` for the whole server), and
/// `query_graph` and `recall_project` cite one `stable_id` for a function.
/// The guard hooks (`--guards`) point a search in the synced folder to the
/// server's tools, and `disconnect` leaves every other entry and hook
/// byte-identical.
#[test]
fn connect_graph_sets_up_one_server_and_disconnect_removes_only_its_own() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonical().unwrap();
    let data = home.join("data");
    let folder = home.join("billing");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("invoice.py"),
        "from money import quantize_cents\n\n\ndef settle_invoice(total, tax):\n    \"\"\"Settle an invoice: the amount due with tax.\"\"\"\n    return quantize_cents(total + tax)\n",
    )
    .unwrap();
    std::fs::write(
        folder.join("money.py"),
        "def quantize_cents(value):\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n",
    )
    .unwrap();
    // A synced text file: a source of the project the code graph has no node
    // in.
    std::fs::write(folder.join("NOTES.txt"), "Invoices settle monthly.\n").unwrap();
    let (synced, _) = graphicle(&home, &data, &folder, &["sync", "."], "");
    assert_eq!(synced["graph"]["status"], "built", "{synced:#}");
    let mcp_file = folder.join(".mcp.json");
    let settings = folder.join(".claude/settings.local.json");
    let mcp_before = write_json(
        &mcp_file,
        &json!({"mcpServers":{"other":{"command":"other-server","args":["--stdio"]}}}),
    );
    let settings_before = write_json(
        &settings,
        &json!({"permissions":{"allow":["Bash(ls)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo other"}]}]}}),
    );

    let (connected, _) = graphicle(
        &home,
        &data,
        &folder,
        &[
            "connect",
            "claude-code",
            "--graph",
            "--guards",
            "--tools",
            "all",
        ],
        "",
    );
    assert_eq!(
        connected["graph"]["guard_hooks"]["installed"],
        json!(["guard-search", "guard-read"]),
        "{connected:#}"
    );
    assert!(
        s(&connected, "access").contains("memory drafts"),
        "{connected:#}"
    );
    let entry: Value = serde_json::from_slice(&std::fs::read(&mcp_file).unwrap()).unwrap();
    assert_eq!(entry["mcpServers"]["other"]["command"], "other-server");
    let args: Vec<String> = entry["mcpServers"]["graphicle"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();
    assert!(args.contains(&"--allow-learning".to_string()), "{args:?}");
    let hooks: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    let pre = hooks["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 3, "{hooks:#}");
    assert_eq!(pre[0]["hooks"][0]["command"], "echo other");
    assert_eq!(pre[1]["matcher"], "Bash|Grep");
    assert!(s(&pre[2]["hooks"][0], "command").ends_with(" hook claude-code guard-read"));

    // One session of the entry's own command line.
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"i4","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_graph","arguments":{"question":"settle_invoice quantize_cents","structured":true}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":"Which helper does settle_invoice depend on?","structured":true}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"propose_memory","arguments":{"kind":"decision","title":"Cents are rounded half up","body":"quantize_cents rounds half up to whole cents.","evidence":[{"source_id":"file:money.py","quote":"scaled = int(value * 100 + 0.5)"}],"idempotency_key":"i4-1"}}}),
    ]
    .map(|request| request.to_string())
    .join("\n")
        + "\n";
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, &home)
        .current_dir(&folder)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(requests.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let reply = |id: u64| {
        replies
            .iter()
            .find(|reply| reply["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id}: {replies:#?}"))["result"]
            .clone()
    };
    assert!(reply(1)["capabilities"].get("resources").is_some());
    let listed = names(&reply(2));
    for name in [
        "query_graph",
        "get_node",
        "recall_project",
        "graph_history_stats",
        "propose_memory",
    ] {
        assert!(listed.contains(&name.to_string()), "{name}: {listed:?}");
    }
    assert!(!listed.contains(&"list_prs".to_string()));
    let graph = reply(3);
    assert_eq!(graph["isError"], false, "{graph}");
    let from_graph = graph["structuredContent"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["label"] == "quantize_cents()")
        .unwrap_or_else(|| panic!("{:#}", graph["structuredContent"]))
        .clone();
    assert_eq!(from_graph["checked"], true);
    assert_eq!(from_graph["source_id"], "file:money.py");
    // recall_project cites the helper through the code graph with the
    // same identity.
    let recall = reply(4);
    let from_recall = recall["structuredContent"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|window| window.get("code_graph"))
        .find(|fact| fact["label"] == "quantize_cents()")
        .unwrap_or_else(|| panic!("{:#}", recall["structuredContent"]["evidence"]))
        .clone();
    assert_eq!(from_recall["stable_id"], from_graph["stable_id"]);
    assert_eq!(from_recall["revision"], from_graph["revision"]);
    let proposed = reply(5);
    assert_eq!(proposed["isError"], false, "{proposed}");
    assert_eq!(proposed["structuredContent"]["state"], "draft");

    // The guard: a search in the synced folder is pointed to the tools.
    let payload =
        json!({"cwd":folder,"tool_name":"Grep","tool_input":{"pattern":"settle"}}).to_string();
    let (nudge, _) = graphicle(
        &home,
        &data,
        &folder,
        &["hook", "claude-code", "guard-search"],
        &payload,
    );
    let context = s(&nudge["hookSpecificOutput"], "additionalContext");
    assert!(
        context.contains("query_graph") && context.contains("recall_project"),
        "{nudge}"
    );
    let read =
        json!({"cwd":folder,"tool_name":"Read","tool_input":{"file_path":folder.join("money.py")}})
            .to_string();
    let (nudge, _) = graphicle(
        &home,
        &data,
        &folder,
        &["hook", "claude-code", "guard-read"],
        &read,
    );
    assert_eq!(nudge["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(
        s(&nudge["hookSpecificOutput"], "additionalContext").contains("covers this file"),
        "{nudge}"
    );
    let listing =
        json!({"cwd":folder,"tool_name":"Bash","tool_input":{"command":"ls -la"}}).to_string();
    let (quiet, _) = graphicle(
        &home,
        &data,
        &folder,
        &["hook", "claude-code", "guard-search"],
        &listing,
    );
    assert_eq!(quiet, json!(""), "a command that searches nothing");
    // The read guard stays quiet for targets outside the project, however
    // they are named, and for a file of the project the code graph has no
    // node in (the nudge says the graph covers the file).
    let elsewhere = home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("tool.py"), "def tool():\n    return 0\n").unwrap();
    for input in [
        json!({"file_path":elsewhere.join("tool.py")}),
        json!({"file_path":"../elsewhere/tool.py"}),
        json!({"pattern":"**/*.py","path":elsewhere}),
        json!({"pattern":"../elsewhere/*.py"}),
        json!({"file_path":folder.join("NOTES.txt")}),
    ] {
        let tool = if input.get("pattern").is_some() {
            "Glob"
        } else {
            "Read"
        };
        let payload = json!({"cwd":folder,"tool_name":tool,"tool_input":input}).to_string();
        let (quiet, _) = graphicle(
            &home,
            &data,
            &folder,
            &["hook", "claude-code", "guard-read"],
            &payload,
        );
        assert_eq!(quiet, json!(""), "{payload}");
    }
    let glob =
        json!({"cwd":folder,"tool_name":"Glob","tool_input":{"pattern":"**/*.py"}}).to_string();
    let (nudge, _) = graphicle(
        &home,
        &data,
        &folder,
        &["hook", "claude-code", "guard-read"],
        &glob,
    );
    assert!(
        s(&nudge["hookSpecificOutput"], "additionalContext").contains("covers these source files"),
        "{nudge}"
    );

    // `disconnect` (the easy flow's, from the record `connect` kept)
    // removes the entry and the guard hooks; each file is then the same as
    // its backup, which is put back.
    let (disconnected, _) = graphicle(&home, &data, &folder, &["disconnect", "claude-code"], "");
    assert_eq!(disconnected["disconnected"], true, "{disconnected:#}");
    let outcome = |file: &Path| {
        disconnected["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|outcome| reports_file(&outcome["file"], file))
            .unwrap_or_else(|| panic!("{}: {disconnected:#}", file.display()))
            .clone()
    };
    assert_eq!(outcome(&mcp_file)["state"], "restored", "{disconnected:#}");
    assert_eq!(
        outcome(&mcp_file)["removed"],
        json!(["removed the graphicle entry"])
    );
    assert_eq!(outcome(&settings)["state"], "restored", "{disconnected:#}");
    assert_eq!(
        outcome(&settings)["removed"],
        json!(["removed the graph guard hooks"])
    );
    assert_eq!(disconnected["kept_backups"], json!([]), "{disconnected:#}");
    assert_eq!(std::fs::read(&mcp_file).unwrap(), mcp_before);
    assert_eq!(std::fs::read(&settings).unwrap(), settings_before);
}

/// Outside a Graphicle project whose code graph holds code the guard is the
/// graph engine's own: its output is passed on unchanged. That includes a
/// synced folder of conversation sessions, whose graph holds no code
/// symbol, as a folder no project has.
#[cfg(unix)]
#[test]
fn the_guard_hands_other_folders_to_the_engines_guard() {
    let (_dir, root) = script_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("graphicle");
    let built = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    if std::fs::hard_link(built, &program).is_err() {
        std::fs::copy(built, &program).unwrap();
    }
    // A stand-in engine: it names its arguments and echoes its input.
    script(
        &bin,
        "graphicle-graph",
        "#!/bin/sh\nprintf 'engine %s %s:' \"$1\" \"$2\"\nwhile IFS= read -r line; do printf '%s' \"$line\"; done\n",
    );
    let data = root.join("data");
    let path = [("PATH", OsStr::new("/usr/bin:/bin"))];
    let elsewhere: PathBuf = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("2026-10-01.txt"),
        "user: where do invoices settle?\nassistant: in settle_invoice, monthly.\n",
    )
    .unwrap();
    let (ok, synced, stderr) = run(&program, &root, &data, &sessions, &["sync", "."], "", &path);
    assert!(ok, "{stderr}");
    let synced: Value = serde_json::from_str(&synced).unwrap();
    assert_eq!(synced["graph"]["status"], "built", "{synced:#}");
    for folder in [&elsewhere, &sessions] {
        let payload =
            json!({"cwd":folder,"tool_name":"Grep","tool_input":{"pattern":"x"}}).to_string();
        let (ok, stdout, stderr) = run(
            &program,
            &root,
            &data,
            folder,
            &["hook", "claude-code", "guard-search"],
            &format!("{payload}\n"),
            &path,
        );
        assert!(ok, "{stderr}");
        assert_eq!(stdout, format!("engine hook-guard search:{payload}"));
    }
}

/// The guard's code check reads the live-symbol table, which a projection
/// of the head set keeps. A store whose table was never built (as before an
/// upgraded store's first projection) reads the graph's open nodes instead
/// and still finds the code.
#[test]
fn the_guard_finds_code_with_and_without_the_live_symbol_table() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonical().unwrap();
    let data = home.join("data");
    let folder = home.join("billing");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("money.py"),
        "def quantize_cents(value):\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n",
    )
    .unwrap();
    let (synced, _) = graphicle(&home, &data, &folder, &["sync", "."], "");
    assert_eq!(synced["graph"]["status"], "built", "{synced:#}");
    let payload =
        json!({"cwd":folder,"tool_name":"Grep","tool_input":{"pattern":"scaled"}}).to_string();
    let nudged = |label: &str| {
        let (nudge, _) = graphicle(
            &home,
            &data,
            &folder,
            &["hook", "claude-code", "guard-search"],
            &payload,
        );
        assert!(
            s(&nudge["hookSpecificOutput"], "additionalContext").contains("query_graph"),
            "{label}: {nudge}"
        );
    };
    nudged("with the live-symbol table");
    let db = rusqlite::Connection::open(data.join("reviews.sqlite3")).unwrap();
    let symbols: i64 = db
        .query_row("SELECT count(*) FROM gf_symbol_heads", [], |row| row.get(0))
        .unwrap();
    assert!(symbols >= 1, "{symbols}");
    db.execute_batch("DELETE FROM gf_symbol_heads; DELETE FROM gf_symbol_state;")
        .unwrap();
    drop(db);
    nudged("without it");
}

/// Repeated guard calls: each agent's nudges are budgeted as the graph
/// engine's are. The first always fires and at most
/// `GRAPHICLE_HOOK_NUDGE_CAP` (default 5) fire in one session, searches and
/// reads sharing the budget; a subagent (`agent_id`) has its own, the count
/// is a marker in the data folder, 0 lifts the cap, and a call without a
/// session id is not budgeted.
#[test]
fn repeated_guard_calls_nudge_each_agent_a_limited_number_of_times() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonical().unwrap();
    let data = home.join("data");
    let (folder, _) = synced(
        &home,
        &data,
        "billing",
        &[(
            "money.py",
            "def quantize_cents(value):\n    return int(value * 100 + 0.5) / 100\n",
        )],
    );
    let nudged = |hook: &str, payload: &Value, cap: Option<&str>| {
        let env: Vec<(&str, &OsStr)> = cap
            .map(|cap| ("GRAPHICLE_HOOK_NUDGE_CAP", OsStr::new(cap)))
            .into_iter()
            .collect();
        let (output, _) = graphicle_with(
            &home,
            &data,
            &folder,
            &["hook", "claude-code", hook],
            &payload.to_string(),
            &env,
        );
        output != json!("")
    };
    let search = |session: &str, agent: &str| json!({"session_id":session,"agent_id":agent,"cwd":folder,"tool_name":"Grep","tool_input":{"pattern":"cents"}});
    let glob = |session: &str| json!({"session_id":session,"cwd":folder,"tool_name":"Glob","tool_input":{"pattern":"**/*.py"}});
    if std::env::var_os("GRAPHICLE_HOOK_NUDGE_CAP").is_none() {
        let mut fired: Vec<bool> = (0..4)
            .map(|_| nudged("guard-search", &search("session-1", ""), None))
            .collect();
        fired.push(nudged("guard-read", &glob("session-1"), None));
        fired.push(nudged("guard-search", &search("session-1", ""), None));
        fired.push(nudged("guard-read", &glob("session-1"), None));
        assert_eq!(fired, [true, true, true, true, true, false, false]);
        assert_eq!(
            std::fs::read_to_string(data.join("hook-nudges/session-1.nudges")).unwrap(),
            "5"
        );
        assert!(nudged(
            "guard-search",
            &search("session-1", "subagent-1"),
            None
        ));
    }
    let capped: Vec<bool> = (0..3)
        .map(|_| nudged("guard-search", &search("session-2", ""), Some("1")))
        .collect();
    assert_eq!(capped, [true, false, false]);
    assert!((0..7).all(|_| nudged("guard-search", &search("session-3", ""), Some("0"))));
    let anonymous = json!({"cwd":folder,"tool_name":"Grep","tool_input":{"pattern":"cents"}});
    assert!((0..3).all(|_| nudged("guard-search", &anonymous, Some("1"))));
}

/// `disconnect` for every client that keeps a JSON file, after `connect`
/// with and without `--graph` (the easy flow's disconnect, which finds what
/// `connect` wrote in its record): it removes only Graphicle's entry (the
/// one `--name` chose, or in `--config-path`'s file) and its hooks, and a
/// file it changed comes back byte for byte, with its mode, as it was before
/// `connect`, also one that held only an empty server table; a file
/// `connect` created goes. An entry of the same name that does not run
/// `graphicle mcp` stays. `connect --graph` without `--hooks` writes no
/// capture hooks.
#[test]
fn disconnect_restores_each_clients_files_and_leaves_other_entries() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonical().unwrap();
    let data = home.join("data");
    let (folder, project) = synced(&home, &data, "shop", &[("cart.py", A)]);
    let connect = |args: &[&str]| graphicle(&home, &data, &folder, args, "").0;
    let bytes = |path: &Path| std::fs::read(path).unwrap();
    let state = |report: &Value, file: &Path| {
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|outcome| reports_file(&outcome["file"], file))
            .map(|outcome| outcome["state"].clone())
    };

    // VS Code: a file that held only an empty server table, in its own format.
    let vscode = folder.join(".vscode/mcp.json");
    std::fs::create_dir_all(vscode.parent().unwrap()).unwrap();
    let vscode_before = b"{\n\t\"servers\": {}\n}\n".to_vec();
    std::fs::write(&vscode, &vscode_before).unwrap();
    let connected = connect(&["connect", "vscode"]);
    assert_eq!(connected["entry"], "graphicle", "{connected:#}");
    let written: Value = serde_json::from_slice(&bytes(&vscode)).unwrap();
    assert_eq!(written["servers"]["graphicle"]["type"], "stdio");
    let gone = connect(&["disconnect", "vscode"]);
    assert_eq!(state(&gone, &vscode), Some(json!("restored")), "{gone:#}");
    assert_eq!(bytes(&vscode), vscode_before);

    // Cursor: an entry named with --name, in a file connect created; the
    // record names the entry.
    let cursor = home.join(".cursor/mcp.json");
    let connected = connect(&["connect", "cursor", "--name", "shop-graph"]);
    assert_eq!(connected["entry"], "shop-graph", "{connected:#}");
    let gone = connect(&["disconnect", "cursor"]);
    assert_eq!(state(&gone, &cursor), Some(json!("deleted")), "{gone:#}");
    assert!(!cursor.exists(), "connect created it");

    // Claude Desktop: the user's other settings and servers, and the file's
    // mode, stay byte for byte.
    let desktop = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude/claude_desktop_config.json")
    } else if cfg!(windows) {
        // `%APPDATA%\Claude`; child_env::home points APPDATA here.
        home.join("AppData")
            .join("Roaming")
            .join("Claude")
            .join("claude_desktop_config.json")
    } else {
        home.join(".config/Claude/claude_desktop_config.json")
    };
    std::fs::create_dir_all(desktop.parent().unwrap()).unwrap();
    let desktop_before =
        br#"{"globalShortcut":"Alt+Space","mcpServers":{"notes":{"command":"notes-server"}}}"#
            .to_vec();
    std::fs::write(&desktop, &desktop_before).unwrap();
    #[cfg(unix)]
    let mode = || {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&desktop).unwrap().permissions().mode() & 0o777
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&desktop, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let connected = connect(&["connect", "claude"]);
    assert_eq!(connected["entry"], "graphicle-shop", "{connected:#}");
    assert_ne!(bytes(&desktop), desktop_before);
    let gone = connect(&["disconnect", "claude"]);
    assert_eq!(state(&gone, &desktop), Some(json!("restored")), "{gone:#}");
    assert_eq!(bytes(&desktop), desktop_before);
    #[cfg(unix)]
    assert_eq!(mode(), 0o644, "the file keeps its mode");

    // --config-path: another file of the same format, naming the project.
    let team = home.join("team/mcp.json");
    let path = team.to_string_lossy().into_owned();
    let connected = connect(&["connect", "claude-code", "--config-path", &path]);
    let written: Value = serde_json::from_slice(&bytes(&team)).unwrap();
    assert_eq!(connected["entry"], "graphicle", "{connected:#}");
    let args = written["mcpServers"]["graphicle"]["args"].to_string();
    assert!(args.contains(&project), "{args}");
    let gone = connect(&["disconnect", "claude-code"]);
    assert_eq!(state(&gone, &team), Some(json!("deleted")), "{gone:#}");
    assert!(!team.exists(), "connect created it");

    // An entry of the same name that does not run graphicle mcp stays.
    let mcp = folder.join(".mcp.json");
    let lookalike = write_json(
        &mcp,
        &json!({"mcpServers":{"graphicle":{"command":"npx","args":["graphicle-lookalike","mcp"]}}}),
    );
    let kept = connect(&["disconnect", "claude-code"]);
    assert_eq!(state(&kept, &mcp), None, "{kept:#}");
    assert_eq!(bytes(&mcp), lookalike);
    std::fs::remove_file(&mcp).unwrap();

    // connect --graph --guards without --hooks: the guard hooks, no capture
    // hooks; with the whole server, a later plain connect keeps the entry's
    // learning and changes nothing.
    let settings = folder.join(".claude/settings.local.json");
    let connected = connect(&[
        "connect",
        "claude-code",
        "--graph",
        "--guards",
        "--tools",
        "all",
    ]);
    assert!(connected.get("hooks").is_none(), "{connected:#}");
    let hooks: Value = serde_json::from_slice(&bytes(&settings)).unwrap();
    let events: Vec<&String> = hooks["hooks"].as_object().unwrap().keys().collect();
    assert_eq!(events, ["PreToolUse"], "{hooks:#}");
    let commands = hooks["hooks"]["PreToolUse"].to_string();
    assert!(
        commands.contains(" hook claude-code guard-search")
            && commands.contains(" hook claude-code guard-read"),
        "{commands}"
    );
    let entry = bytes(&mcp);
    let again = connect(&["connect", "claude-code", "--tools", "all"]);
    assert_eq!(again["unchanged"], true, "{again:#}");
    assert!(s(&again, "access").contains("memory drafts"), "{again:#}");
    assert_eq!(bytes(&mcp), entry);
    let gone = connect(&["disconnect", "claude-code"]);
    assert_eq!(state(&gone, &mcp), Some(json!("deleted")), "{gone:#}");
    assert_eq!(state(&gone, &settings), Some(json!("deleted")), "{gone:#}");
    assert!(!mcp.exists() && !settings.exists(), "connect created both");
}

/// The engine's pull-request tools with a stand-in GitHub CLI: only
/// `graphicle mcp --pull-requests` started in the project's own folder
/// publishes them (elsewhere it refuses to start), `gh` runs in that folder,
/// `list_prs` reads no code graph, and the impact tools read the project's.
#[cfg(unix)]
#[test]
fn pull_request_tools_run_gh_in_the_projects_folder_only() {
    let (_dir, root) = script_root();
    let bin = root.join("bin");
    let pr = r#"{"number":7,"title":"Round invoices to cents","headRefName":"cents","baseRefName":"main","author":{"login":"octo"},"isDraft":false,"reviewDecision":"","statusCheckRollup":[],"updatedAt":"2026-10-13T10:00:00Z"}"#;
    script(
        &bin,
        "gh",
        &format!(
            r#"#!/bin/sh
printf '%s %s\n' "$(pwd -P)" "$*" >> "$GH_STUB_LOG"
case "$1 $2" in
  "repo view") printf '{{"defaultBranchRef":{{"name":"main"}}}}\n' ;;
  "pr list") printf '[%s]\n' '{pr}' ;;
  "pr view") printf '%s\n' '{pr}' ;;
  "pr diff") printf 'a.py\n' ;;
  *) exit 1 ;;
esac
"#
        ),
    );
    let data = root.join("data");
    let log = root.join("gh.log");
    let search = format!("{}:/usr/bin:/bin", bin.display());
    let env = [
        ("PATH", OsStr::new(&search)),
        ("GH_STUB_LOG", log.as_os_str()),
    ];
    let (folder, project) = synced(&root, &data, "ledger", &[("a.py", A), ("b.py", B)]);
    // Elsewhere the server refuses to start.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let program = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    let (ok, _, stderr) = run(
        program,
        &root,
        &data,
        &elsewhere,
        &["mcp", "--pull-requests", "--project", &project],
        "",
        &env,
    );
    assert!(!ok);
    assert!(
        stderr.contains("start graphicle mcp in the folder of the project it serves"),
        "{stderr}"
    );
    assert!(!log.exists(), "gh never ran");

    let replies = session(
        &root,
        &data,
        &folder,
        &["mcp", "--pull-requests"],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"i4","version":"0"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_prs","arguments":{"structured":true}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_pr_impact","arguments":{"pr_number":7,"structured":true}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_prs","arguments":{"github_repo":"octo/ledger","structured":true}}}),
        ],
        &env,
    );
    assert!(
        s(&reply(&replies, 1), "instructions").contains("run the GitHub CLI and git"),
        "{replies:#?}"
    );
    let listed = names(&reply(&replies, 2));
    for name in graph_tools::PULL_REQUESTS {
        assert!(listed.contains(&name.to_string()), "{listed:?}");
    }
    let listing = reply(&replies, 3);
    assert_eq!(listing["isError"], false, "{listing:#}");
    assert!(
        s(&listing["structuredContent"], "text").contains("Round invoices to cents"),
        "{listing:#}"
    );
    // list_prs reads no code graph: it answers whether or not one is built.
    assert_eq!(listing["structuredContent"]["episode"], Value::Null);
    let impact = reply(&replies, 4);
    assert_eq!(impact["isError"], false, "{impact:#}");
    let text = s(&impact["structuredContent"], "text");
    assert!(
        text.starts_with("PR #7: Round invoices to cents") && text.contains("Graph impact: "),
        "{text}"
    );
    assert!(impact["structuredContent"]["episode"].is_string());
    let named = reply(&replies, 5);
    assert_eq!(named["isError"], false, "{named:#}");
    // Every gh call ran in the project's folder; github_repo reached gh as
    // its --repo.
    let calls = std::fs::read_to_string(&log).unwrap();
    let folder_text = folder.to_string_lossy().into_owned();
    assert!(
        calls
            .lines()
            .all(|line| line.starts_with(&format!("{folder_text} "))),
        "{calls}"
    );
    assert!(calls.contains("--repo octo/ledger"), "{calls}");
}

/// `graphicle serve --mcp-tools/--mcp-set` reach the HTTP MCP route: the
/// endpoint publishes the chosen half, applies and reports the
/// fixed switch and refuses a request for the other value; a switch set
/// twice stops the server before it starts.
#[test]
fn serve_publishes_its_tool_set_and_fixed_switches_over_http() {
    use std::io::{BufRead, Read, Write};
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonical().unwrap();
    let data = home.join("data");
    let (folder, project) = synced(&home, &data, "depot", &[("a.py", A), ("b.py", B)]);
    let program = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    let (ok, _, stderr) = run(
        program,
        &home,
        &data,
        &folder,
        &[
            "serve",
            "--port",
            "0",
            "--mcp-set",
            "graph_retrieval=false",
            "--mcp-set",
            "graph_retrieval=true",
        ],
        "",
        &[],
    );
    assert!(!ok && stderr.contains("set twice"), "{stderr}");

    let session: Value =
        serde_json::from_slice(&std::fs::read(data.join("local-session.json")).unwrap()).unwrap();
    let mut command = Command::new(program);
    crate::child_env::home(&mut command, &home)
        .current_dir(&folder)
        .arg("--data-dir")
        .arg(&data)
        .args([
            "serve",
            "--port",
            "0",
            "--mcp-tools",
            "memory",
            "--mcp-set",
            "graph_retrieval=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    // Stopped when the test ends, whichever way it ends.
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let server = Stop(child);
    let mut address = String::new();
    for _ in 0..50 {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(url) = line.trim().strip_prefix("Graphicle is ready at http://") {
            address = url.to_string();
            break;
        }
    }
    assert!(!address.is_empty(), "the server did not start");
    // The rest of its log is read, so the server never waits on a full pipe.
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    let post = |request: Value| -> Value {
        let body = request.to_string();
        let mut stream = std::net::TcpStream::connect(&address).unwrap();
        write!(
            stream,
            "POST /mcp/{}/{project} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            s(&session, "workspace"),
            s(&session, "capability"),
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{response}");
        serde_json::from_str(body).unwrap_or_else(|_| panic!("{response}"))
    };
    let request = |id: u64, method: &str, params: Value| {
        post(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))["result"].clone()
    };
    let initialized = request(1, "initialize", json!({"protocolVersion":"2025-11-25"}));
    assert!(initialized["capabilities"].get("resources").is_none());
    let listed = names(&request(2, "tools/list", json!({})));
    assert!(listed.contains(&"recall_project".to_string()), "{listed:?}");
    for name in graph_tools::NAVIGATION.iter().chain(&GRAPH_HISTORY) {
        assert!(!listed.contains(&name.to_string()), "{name}: {listed:?}");
    }
    let recall = request(
        3,
        "tools/call",
        json!({"name":"recall_project","arguments":{"question":"who calls beta","structured":true}}),
    );
    assert_eq!(recall["isError"], false, "{recall:#}");
    let mechanisms = &recall["structuredContent"]["mechanisms"];
    assert_eq!(mechanisms["server_fixed"], json!({"graph_retrieval":false}));
    assert_eq!(mechanisms["graph_retrieval_enabled"], false);
    let refused = post(json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"recall_project","arguments":{"question":"who calls beta","graph_retrieval":true}}}));
    assert!(
        s(&refused["error"], "message").contains("graph_retrieval is fixed to false"),
        "{refused:#}"
    );
    let unpublished = post(json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
        "params":{"name":"query_graph","arguments":{"question":"beta"}}}));
    assert_eq!(unpublished["error"]["message"], "Tool unavailable");
    drop(server);
}

/// Conformance (the integration switches): every recall packet echoes the
/// switches in force (`mechanisms.switches`), whether a request set them,
/// the server fixed them or they kept their defaults; `cross_repo` and
/// `scope` are fixed like the others; an unknown switch is refused, never
/// ignored, by `--set` and in a request.
#[test]
fn conformance_switches_are_echoed_fixed_and_never_ignored() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let app = f.app();
    let recall = |app: &App, arguments: Value| {
        call(&f, app, "recall_project", arguments).map(|r| r["structuredContent"].clone())
    };
    let packet = recall(&app, json!({"question":"who calls beta"})).unwrap();
    assert_eq!(
        packet["mechanisms"]["switches"],
        json!({"graph_retrieval":true,"memory_retrieval":true,"doc_links":"counted",
            "symbol_links":"counted","cross_repo":true,"notes_first":false,
            "identifier_route":false,"code_first":"auto","query_focus":"auto",
            "identifier_arm":"auto","more_files":"auto","scope":"project"})
    );
    let packet = recall(
        &app,
        json!({"question":"who calls beta","graph_retrieval":false,"doc_links":true,
            "symbol_links":false,"cross_repo":false,"notes_first":true}),
    )
    .unwrap();
    assert_eq!(
        packet["mechanisms"]["switches"],
        json!({"graph_retrieval":false,"memory_retrieval":true,"doc_links":true,
            "symbol_links":false,"cross_repo":false,"notes_first":true,
            "identifier_route":false,"code_first":"auto","query_focus":"auto",
            "identifier_arm":"auto","more_files":"auto","scope":"project"})
    );
    // `cross_repo` follows `graph_retrieval` when left out.
    let packet = recall(
        &app,
        json!({"question":"who calls beta","graph_retrieval":false}),
    )
    .unwrap();
    assert_eq!(packet["mechanisms"]["switches"]["cross_repo"], false);
    // The essentials set builds 0.1.2's packet unless a call turns
    // `notes_first` on; `--tools all` keeps 0.1.0's windows whatever it says.
    let mut essentials = f.app();
    essentials.mcp.tools = ToolSet::Essentials;
    let plain = recall(&essentials, json!({"question":"who calls beta"})).unwrap();
    assert_eq!(
        plain["notes_first"],
        json!({"status":"off","defaulted":true}),
        "{}",
        plain["notes_first"]
    );
    assert_eq!(plain["source_age"]["status"], "disabled");
    let first = recall(
        &essentials,
        json!({"question":"who calls beta","notes_first":true}),
    )
    .unwrap();
    assert_eq!(
        first["notes_first"]["status"], "reranked",
        "{}",
        first["notes_first"]
    );
    assert_eq!(first["notes_first"]["defaulted"], false);
    assert_eq!(first["mechanisms"]["switches"]["notes_first"], true);
    let all = recall(
        &app,
        json!({"question":"who calls beta","notes_first":true}),
    )
    .unwrap();
    assert_eq!(all["notes_first"]["status"], "windows_as_ranked");
    // Fixed by the server: echoed, listed under `server_fixed`, and a request
    // for another value is refused.
    let mut fixed = f.app();
    fixed.mcp.fixed = api::fixed_switches(&[
        "graph_retrieval=false".into(),
        "memory_retrieval=false".into(),
        "doc_links=false".into(),
        "symbol_links=false".into(),
        "cross_repo=false".into(),
        "notes_first=true".into(),
        "scope=project".into(),
    ])
    .unwrap();
    let packet = recall(&fixed, json!({"question":"who calls beta"})).unwrap();
    assert_eq!(
        packet["mechanisms"]["switches"],
        json!({"graph_retrieval":false,"memory_retrieval":false,"doc_links":false,
            "symbol_links":false,"cross_repo":false,"notes_first":true,
            "identifier_route":false,"code_first":"auto","query_focus":"auto",
            "identifier_arm":"auto","more_files":"auto","scope":"project"})
    );
    assert_eq!(packet["mechanisms"]["server_fixed"]["cross_repo"], false);
    assert_eq!(packet["mechanisms"]["server_fixed"]["notes_first"], true);
    assert_eq!(packet["mechanisms"]["server_fixed"]["scope"], "project");
    for (switch, other) in [
        ("cross_repo", json!(true)),
        ("symbol_links", json!(true)),
        ("notes_first", json!(false)),
        ("scope", json!("workspace")),
    ] {
        let refused =
            recall(&fixed, json!({"question":"who calls beta", switch: other})).unwrap_err();
        assert!(refused.contains("fixed to"), "{switch}: {refused}");
    }
    // Unknown names and values are refused, by `--set` and in a request.
    for bad in [
        "code_graph=false",
        "cross_repo=maybe",
        "notes_first=yes",
        "scope=everything",
        "scope=true",
        "links=true",
    ] {
        assert!(api::fixed_switches(&[bad.into()]).is_err(), "{bad}");
    }
    let refused = api::fixed_switches(&["links=true".into()]).unwrap_err();
    assert!(
        refused.message.contains(
            "the server fixes graph_retrieval, memory_retrieval, doc_links, symbol_links, cross_repo, notes_first, identifier_route, code_first, query_focus, identifier_arm, more_files and scope"
        ),
        "{}",
        refused.message
    );
    let refused = recall(
        &app,
        json!({"question":"who calls beta","code_graph":false}),
    )
    .unwrap_err();
    assert!(refused.contains("published contract"), "{refused}");
    let refused = recall(&app, json!({"question":"who calls beta","cross_repo":"no"})).unwrap_err();
    assert!(
        refused.contains("cross_repo must be true or false"),
        "{refused}"
    );
    // The code-question switches: taken beside the published arguments,
    // fixed by `--set`, and refused outside their values.
    let packet = recall(
        &app,
        json!({"question":"who calls beta","code_first":false,"query_focus":"auto",
            "identifier_arm":true,"more_files":3}),
    )
    .unwrap();
    let switches = &packet["mechanisms"]["switches"];
    assert_eq!(switches["code_first"], false, "{switches}");
    assert_eq!(switches["query_focus"], "auto");
    assert_eq!(switches["identifier_arm"], true);
    assert_eq!(switches["more_files"], 3);
    assert_eq!(packet["code_first"]["applied"], false);
    for (switch, bad, takes) in [
        ("code_first", json!("maybe"), "true, false or auto"),
        ("query_focus", json!(1), "true, false or auto"),
        ("more_files", json!(41), "a number from 0 to 40"),
    ] {
        let refused = recall(&app, json!({"question":"who calls beta", switch: bad})).unwrap_err();
        assert!(
            refused.contains(&format!("{switch} must be {takes}")),
            "{refused}"
        );
    }
    let mut chosen = f.app();
    chosen.mcp.fixed = api::fixed_switches(&[
        "code_first=false".into(),
        "identifier_arm=auto".into(),
        "more_files=0".into(),
    ])
    .unwrap();
    let packet = recall(&chosen, json!({"question":"who calls beta"})).unwrap();
    assert_eq!(packet["mechanisms"]["switches"]["code_first"], false);
    assert_eq!(packet["mechanisms"]["switches"]["identifier_arm"], "auto");
    assert_eq!(packet["mechanisms"]["switches"]["more_files"], 0);
    for bad in [
        "code_first=maybe",
        "more_files=41",
        "more_files=-1",
        "query_focus=yes",
    ] {
        assert!(api::fixed_switches(&[bad.into()]).is_err(), "{bad}");
    }
    // A server that fixes the scope to the workspace serves no project
    // connection.
    let mut whole = f.app();
    whole.mcp.fixed = api::fixed_switches(&["scope=workspace".into()]).unwrap();
    let refused = recall(&whole, json!({"question":"who calls beta"})).unwrap_err();
    assert!(refused.contains("fixes scope to workspace"), "{refused}");
}

/// Conformance (the manifest): `graphicle mcp --manifest` records the
/// program (version, edition, sha256), the tools a connection lists with
/// their hash, the switches the server fixes and those a request may set,
/// and the schema and format versions. Fixing a switch changes the
/// switches, not the tools.
#[test]
fn conformance_the_manifest_records_program_tools_switches_and_schemas() {
    let f = Fixture::new(&[("a.py", A)]);
    let mut app = f.app();
    app.mcp.fixed =
        api::fixed_switches(&["doc_links=false".into(), "scope=project".into()]).unwrap();
    let manifest = api::manifest(&app, &f.workspace, &f.key, Some(&f.repo)).unwrap();
    assert_eq!(manifest["format"], api::MANIFEST_FORMAT);
    // The runtime's version, as `initialize` reports it (a test built in
    // another crate has that crate's version).
    let initialized = rpc(&f, &app, &f.key, "initialize", json!({}));
    assert_eq!(
        manifest["program"]["version"],
        initialized["result"]["serverInfo"]["version"]
    );
    assert_eq!(
        manifest["program"]["sha256"].as_str().map(str::len),
        Some(64),
        "{manifest}"
    );
    assert_eq!(manifest["connection"], "project");
    let listed = rpc(&f, &app, &f.key, "tools/list", json!({}));
    let listed: BTreeSet<String> = names(&listed["result"]).into_iter().collect();
    let recorded: BTreeSet<String> = manifest["tools"]["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_string())
        .collect();
    assert_eq!(recorded, listed);
    assert_eq!(
        manifest["switches"]["server_fixed"],
        json!({"doc_links":false,"scope":"project"})
    );
    assert_eq!(
        manifest["switches"]["request"],
        json!([
            "graph_retrieval",
            "memory_retrieval",
            "doc_links",
            "symbol_links",
            "cross_repo",
            "notes_first",
            "identifier_route",
            "code_first",
            "query_focus",
            "identifier_arm",
            "more_files",
            "scope"
        ])
    );
    assert_eq!(
        manifest["switches"]["notes_first_default"],
        query::NOTES_FIRST_DEFAULT
    );
    for field in [
        "compiled_sha256",
        "deletion",
        "state",
        "export",
        "template",
        "cross_link_state",
    ] {
        assert!(
            manifest["schemas"][field].is_string(),
            "{field}: {manifest}"
        );
    }
    assert!(manifest["schemas"]["store"]["tables"].as_u64().unwrap() > 10);
    assert!(manifest["retrieval_defaults"].is_object(), "{manifest}");
    let plain = api::manifest(&f.app(), &f.workspace, &f.key, Some(&f.repo)).unwrap();
    assert_eq!(plain["tools"]["sha256"], manifest["tools"]["sha256"]);
    assert_eq!(plain["schemas"], manifest["schemas"]);
    assert_ne!(plain["switches"], manifest["switches"]);
}

/// The identifier route: a question that is an identifier, or only asks
/// where named identifiers are defined or used, gets their definitions and
/// references from the code graph as path:line lines, in the essentials set
/// and through `context`, within the budget. A question in plain words, one
/// that asks more about a name, and a name the code graph does not define
/// in the caller's view keep the retrieval; so do `identifier_route: false`
/// and the whole server unless a call asks for the route. The packet's
/// `route` says which path ran.
#[test]
fn identifier_questions_take_the_code_graph_route() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let mut essentials = f.app();
    essentials.mcp.tools = ToolSet::Essentials;
    let recall = |app: &App, arguments: Value| {
        call(&f, app, "recall_project", arguments)
            .map(|r| r["structuredContent"].clone())
            .unwrap()
    };
    // An identifier alone: its definition in a.py and its uses in b.py.
    let packet = recall(&essentials, json!({"question":"`beta`"}));
    assert_eq!(packet["route"]["path"], "identifier", "{packet:#}");
    assert_eq!(packet["route"]["defaulted"], true);
    assert_eq!(packet["mechanisms"]["switches"]["identifier_route"], true);
    let symbol = &packet["symbols"][0];
    assert_eq!(symbol["name"], "beta");
    assert_eq!(symbol["definitions"][0]["relpath"], "a.py", "{symbol:#}");
    assert_eq!(symbol["definitions"][0]["line"], 4, "{symbol:#}");
    assert!(
        symbol["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reference| reference["relpath"] == "b.py"),
        "{symbol:#}"
    );
    let markdown = s(&packet, "markdown");
    assert!(markdown.contains("a.py:4"), "{markdown}");
    assert!(markdown.contains("- reference b.py:"), "{markdown}");
    assert!(packet["sources"].as_array().unwrap().is_empty());
    assert!(packet["tokens"].as_u64() <= packet["max_tokens"].as_u64());
    // The default reply is that text and nothing else.
    let reply = rpc(
        &f,
        &essentials,
        &f.key,
        "tools/call",
        json!({"name":"recall_project","arguments":{"question":"`beta`"}}),
    );
    assert!(
        reply["result"].get("structuredContent").is_none(),
        "{reply}"
    );
    assert_eq!(reply["result"]["content"][0]["text"], packet["markdown"]);
    // Mixed: names with words that only ask where they are and who uses
    // them take the route; words that ask more keep the retrieval.
    for question in [
        "Where is `alpha` defined, and who calls it?",
        "who calls alpha() and beta()",
        "find the callers of run()",
    ] {
        let packet = recall(&essentials, json!({"question":question}));
        assert_eq!(
            packet["route"]["path"], "identifier",
            "{question}: {packet:#}"
        );
    }
    let both = recall(
        &essentials,
        json!({"question":"who calls alpha() and beta()"}),
    );
    assert_eq!(both["route"]["names"], json!(["alpha", "beta"]));
    let more = recall(
        &essentials,
        json!({"question":"how does `run` add alpha and beta together"}),
    );
    assert_eq!(more["route"]["path"], "retrieval");
    assert_eq!(more["route"]["reason"], "asks_more");
    assert_eq!(more["route"]["names"], json!(["run"]));
    assert!(!more["sources"].as_array().unwrap().is_empty(), "{more:#}");
    // Content, outgoing calls and mentions keep it too.
    for question in [
        "what is `beta` used for",
        "callees of run()",
        "show me the code of run()",
    ] {
        let packet = recall(&essentials, json!({"question":question}));
        assert_eq!(packet["route"]["reason"], "asks_more", "{question}");
    }
    // Plain words, a name the code graph does not define, the switch, and
    // a call that names needs.
    for (arguments, reason) in [
        (json!({"question":"who calls beta"}), "plain_words"),
        (json!({"question":"`gamma_delta`"}), "not_in_code_graph"),
        (json!({"question":"`beta`","identifier_route":false}), "off"),
        (
            json!({"question":"`beta`","needs":["the return value"]}),
            "needs",
        ),
    ] {
        let packet = recall(&essentials, arguments.clone());
        assert_eq!(packet["route"]["path"], "retrieval", "{arguments}");
        assert_eq!(packet["route"]["reason"], reason, "{arguments}");
    }
    // The whole server keeps 0.1.0's recall unless a call asks for the route.
    let all = f.app();
    let packet = recall(&all, json!({"question":"`beta`"}));
    assert_eq!(packet["route"]["reason"], "off");
    assert_eq!(packet["route"]["defaulted"], true);
    let packet = recall(&all, json!({"question":"`beta`","identifier_route":true}));
    assert_eq!(packet["route"]["path"], "identifier");
    // `context` (the HTTP API, the SDKs, the app's evidence button): the
    // retrieval unless a call asks for the route; the route's packet has no
    // evidence, and cites only within the caller's view (`include`).
    let context = |arguments: Value| {
        let mut arguments = arguments;
        arguments["repository_id"] = json!(f.repo);
        f.store
            .project_context(&f.workspace, &f.key, &arguments)
            .unwrap()
    };
    let packet = context(json!({"question":"beta()"}));
    assert_eq!(packet["route"]["reason"], "off", "{packet:#}");
    assert_eq!(packet["route"]["defaulted"], true);
    assert!(!packet["evidence"].as_array().unwrap().is_empty());
    let packet = context(json!({"question":"beta()","identifier_route":true}));
    assert_eq!(packet["route"]["path"], "identifier", "{packet:#}");
    assert_eq!(packet["evidence"], json!([]));
    let packet = context(json!({"question":"beta()","identifier_route":true,
        "include":["file:b.py"]}));
    assert_eq!(packet["route"]["reason"], "not_in_code_graph", "{packet:#}");
    let packet = context(json!({"question":"how is the sum returned","identifier_route":true}));
    assert_eq!(packet["route"]["reason"], "plain_words");
}

/// Within the budget: past it, the route lists what fits and counts the
/// rest.
#[test]
fn the_identifier_route_keeps_to_the_budget() {
    let many: String = std::iter::once("from a import beta\n\n".to_string())
        .chain((0..300).map(|i| format!("def caller_{i}():\n    return beta()\n\n")))
        .collect();
    let f = Fixture::new(&[("a.py", A), ("c.py", &many)]);
    let packet = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &json!({"repository_id":f.repo,"question":"`beta`","max_tokens":256,"max_chars":2000,
                "identifier_route":true}),
        )
        .unwrap();
    assert_eq!(packet["route"]["path"], "identifier", "{packet:#}");
    assert!(packet["characters"].as_u64().unwrap() <= 2000, "{packet:#}");
    assert!(packet["tokens"].as_u64().unwrap() <= 256, "{packet:#}");
    assert!(
        packet["symbols"][0]["reference_count"].as_u64().unwrap() > 100,
        "{packet:#}"
    );
    assert!(
        packet["omitted"]["definitions_and_references"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(s(&packet, "markdown").contains("Budget: 256"));
}
