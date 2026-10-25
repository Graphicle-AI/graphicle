//! One MCP server: the graph engine's
//! navigation tools, under Graphify's names, answered from a project's
//! recorded code-graph episode next to the service's own tools, under the
//! service's authorization.
//!
//! - **Tools.** The specs come from the engine (`graphicle::serve::tools_list`),
//!   so a port of the engine's tool texts flows through, and are rewritten the
//!   same way for every tool: the engine's `project_path` (a graph file the
//!   engine's own server may read) gives way to `repo` (a project of the
//!   store: an id or a key, and over stdio a synced folder) and `as_of` (an
//!   episode id, a label such as a commit sha, `latest` or `working`); the
//!   pull-request tools' GitHub `repo` becomes `github_repo`, so a GitHub
//!   slug never reaches the project argument; every schema is strict.
//!   `retrieve_context` is never published, and the
//!   pull-request tools only over stdio, behind `graphicle mcp
//!   --pull-requests`, in the project's own folder ([`PULL_REQUESTS`]).
//! - **Answers.** `content` starts with the engine's text, unchanged, then a
//!   short citations block ([`citations_text`]) for clients that show the
//!   model `content` only. `structuredContent` adds the episode read and the
//!   nodes the answer names: each with `stable_id`, `revision`, its
//!   line range and, when its file's current bytes are the ones the episode
//!   recorded, the checked `file:` byte range and the file's sha256, the
//!   window `recall_project` and `read_project_source` cite for the same
//!   symbol. While the answer reads an earlier episode than the current
//!   files' (a waiting projection, or `as_of`), each node the answer reaches
//!   through a relation says whether a relation read from an unchanged file
//!   reaches it (`relation_checked`), the rule the code-graph arm follows
//!   when it withholds such neighbours.
//! - **Back-links.** `get_node`'s node also lists, under `linked`, the
//!   current reviewed memories linked to that symbol and the documents the
//!   reader may read that mention it ([`crate::symbol_links::attach`]); the
//!   citations block names them too.
//! - **Communities**. The episode is clustered when the
//!   server first reads it (the engine's Leiden clustering, as `cluster-only`
//!   runs it), so `get_community`, the nodes' community numbers and the six
//!   resources answer. Measured on Django's `django/` package (13,985 nodes,
//!   29,153 edges, release builds): clustering took 0.18 s against a 6.0 s
//!   projection, 3%. The projection and the stored
//!   episode do not change.
//! - **Unchanged:** the engine's own server, `graphicle graph serve` (the
//!   parity contract), and the service's tools. A query never writes to the
//!   store; the engine's opt-in query log (`GRAPHICLE_QUERY_LOG*`), when set
//!   in the server's environment, also records `query_graph` questions, as
//!   it does for `graphicle graph serve`.
use crate::{
    Error, Result, code_graph_arm,
    extension::McpTool,
    graph_projection,
    store::{Principal, Store, one, repo, s},
    string,
};
use graphicle::{
    graph::Graph,
    serve::GraphContext,
    serve_search::{label, neighbors},
};
use graphicle_memory::graph::span;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, PoisonError},
};

/// The engine's navigation tools the unified server publishes, Graphify's
/// names (installed rules name `query_graph`, `shortest_path` and
/// `get_node`).
pub const NAVIGATION: [&str; 7] = [
    "query_graph",
    "get_node",
    "get_neighbors",
    "get_community",
    "god_nodes",
    "graph_stats",
    "shortest_path",
];
/// The engine's pull-request dashboard tools: they run `gh` in the project's
/// folder, so only `graphicle mcp --pull-requests` publishes them.
pub const PULL_REQUESTS: [&str; 3] = ["list_prs", "get_pr_impact", "triage_prs"];
/// The operation a navigation tool runs (`api::command`, also over HTTP).
pub const NAVIGATE: &str = "graph.navigate";
/// The pull-request tools' operation: answered by the stdio server only,
/// never an `api::command` operation.
pub const PULL_REQUEST: &str = "graph.pull_requests";
/// Most nodes one answer cites in `structuredContent`.
pub const CITED_LIMIT: usize = 100;
/// Most nodes the citations block of `content` lists; the rest are in
/// `structuredContent`.
pub const CITED_IN_TEXT: usize = 20;
/// The characters of a `stable_id` the citations block of `content` shows:
/// enough to match the one `recall_project` cites.
const STABLE_ID_IN_TEXT: usize = 12;
/// The characters of the episode id the citations block shows
/// (`structuredContent` has the whole id).
const EPISODE_IN_TEXT: usize = 12;
/// Episodes kept resident for the graph tools, most recently used last.
const RESIDENT_LIMIT: usize = 2;
/// Arguments the server reads itself; the engine never sees them.
const SERVER_ARGUMENTS: [&str; 5] = ["tool", "repo", "as_of", "repository_id", "github_repo"];

const REPO: &str = "The project to read: its id or key, or with graphicle mcp a synced folder. Defaults to this connection's project.";
const AS_OF: &str = "The code-graph episode to read: an episode id, a label such as a commit sha, latest or working. Defaults to the project's current code graph.";

/// Whether `tool` is one of the engine's tools (navigation or pull requests).
pub fn is_graph_tool(tool: &McpTool) -> bool {
    tool.operation == NAVIGATE || tool.operation == PULL_REQUEST
}

/// The engine's tools as the unified server publishes them: the navigation
/// tools, and with `pull_requests` the pull-request tools, in the engine's
/// order. Navigation tools take `repo` through `repo_arg`.
pub fn tools(pull_requests: bool) -> Vec<McpTool> {
    graphicle::serve::tools_list()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|spec| {
            let name = spec["name"].as_str()?;
            if NAVIGATION.contains(&name) {
                Some(McpTool::new(published(spec, false), NAVIGATE, true))
            } else if pull_requests && PULL_REQUESTS.contains(&name) {
                Some(McpTool::new(published(spec, true), PULL_REQUEST, false))
            } else {
                None
            }
        })
        .collect()
}

/// One engine tool spec, rewritten for the unified server (see the module
/// documentation): no `project_path`; `github_repo` for the pull-request
/// tools' GitHub `repo`; `repo` and `as_of` for the navigation tools; a
/// strict schema and annotations.
fn published(spec: &Value, pull_request: bool) -> Value {
    let mut properties = Map::new();
    for (key, schema) in spec["inputSchema"]["properties"]
        .as_object()
        .into_iter()
        .flatten()
    {
        match key.as_str() {
            "project_path" => {}
            "repo" if pull_request => {
                properties.insert("github_repo".into(), schema.clone());
            }
            _ => {
                properties.insert(key.clone(), schema.clone());
            }
        }
    }
    if !pull_request {
        properties.insert("repo".into(), json!({"type":"string","description":REPO}));
        properties.insert("as_of".into(), json!({"type":"string","description":AS_OF}));
    }
    let required: Vec<Value> = spec["inputSchema"]["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|name| *name != "project_path")
        .map(|name| {
            if pull_request && name == "repo" {
                json!("github_repo")
            } else {
                name.clone()
            }
        })
        .collect();
    json!({"name":spec["name"],"description":spec["description"],
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":pull_request}})
}

/// The engine tool's own arguments: the call's, without those the server
/// reads, and `github_repo` under the engine's name, `repo`.
fn engine_arguments(args: &Value) -> Value {
    let mut out = args.as_object().cloned().unwrap_or_default();
    for key in SERVER_ARGUMENTS {
        out.shift_remove(key);
    }
    if let Some(github) = args.get("github_repo") {
        out.insert("repo".into(), github.clone());
    }
    Value::Object(out)
}

/// The episode a graph tool reads, and how its files compare with the
/// project's current head set.
struct Reading {
    episode: String,
    /// The current head set's episode.
    current: String,
    /// path → sha256 of the current head set.
    heads: BTreeMap<String, String>,
    /// path → sha256 the episode recorded, when the store kept its manifest.
    recorded: Option<BTreeMap<String, String>>,
}

impl Reading {
    /// Whether `path`'s current bytes are the ones the episode recorded, so a
    /// line range the episode gives describes them.
    fn current_file(&self, path: &str) -> bool {
        let Some(head) = self.heads.get(path) else {
            return false;
        };
        self.episode == self.current
            || self
                .recorded
                .as_ref()
                .is_some_and(|recorded| recorded.get(path) == Some(head))
    }
    /// Files added, changed or removed since the episode; `null` when the
    /// episode was recorded without its manifest (older stores).
    fn lag(&self) -> Value {
        if self.episode == self.current {
            return json!(0);
        }
        match &self.recorded {
            Some(recorded) => {
                let changed = self
                    .heads
                    .iter()
                    .filter(|(path, sha)| recorded.get(*path) != Some(*sha))
                    .count();
                json!(
                    changed
                        + recorded
                            .keys()
                            .filter(|p| !self.heads.contains_key(*p))
                            .count()
                )
            }
            None => Value::Null,
        }
    }
}

/// The episode to read: `as_of` (an episode id, a label, `latest` or
/// `working`), else the current head set's episode, else, while the current
/// head set waits for its projection, the latest recorded one (as the
/// code-graph arm reads it). A project with no episode is refused with the
/// state of its projection.
fn reading(
    db: &Connection,
    p: &Principal,
    repository: &str,
    as_of: Option<&str>,
) -> Result<Reading> {
    let heads = crate::store::heads(db, repository)?;
    let current = graph_projection::episode_of(&heads);
    let episode = match as_of {
        Some(selector) => {
            graph_projection::resolve_episode(db, &p.workspace, repository, selector)?
        }
        None if graph_projection::is_recorded(db, repository, &current)? => current.clone(),
        None => match one(
            db,
            "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
            [repository],
        )? {
            Some(row) => s(&row, "episode").to_string(),
            None => {
                let state = crate::jobs::projection_state(db, repository, &current)?;
                return Err(Error::bad(format!(
                    "This project's code graph is not built yet (projection: {state}). graphicle mcp builds it when it starts and graphicle sync builds it in the foreground; call again once it is built."
                )));
            }
        },
    };
    let recorded = graph_projection::stored_episode_heads(db, repository, &episode)?;
    Ok(Reading {
        episode,
        current,
        heads,
        recorded,
    })
}

/// An episode's graph as the graph tools read it: the engine's context
/// (graph, search index and communities), the episode's communities named
/// after their hub nodes, and the indexes the citations use.
pub struct Resident {
    pub repository: String,
    pub episode: String,
    pub context: GraphContext,
    /// Community id → its hub node's label, as `cluster-only` names them
    /// without a model (`graphicle::cluster::label_communities_by_hub`).
    pub labels: Value,
    starts: HashMap<String, Vec<usize>>,
    /// (source file, location) as a `query_graph` NODE line prints them →
    /// nodes.
    by_location: HashMap<(String, String), Vec<usize>>,
    /// Label as `shortest_path` prints it → nodes.
    by_label: HashMap<String, Vec<usize>>,
}

static RESIDENT: Mutex<VecDeque<Arc<Resident>>> = Mutex::new(VecDeque::new());
/// Held while an episode is built: one build at a time, so a call that
/// arrives while the same episode is built waits for it and reuses it, and
/// first calls on several episodes never hold several builds in memory.
static BUILDING: Mutex<()> = Mutex::new(());

/// The resident entry of `episode` of `repository`, most recently used last.
fn held(repository: &str, episode: &str) -> Option<Arc<Resident>> {
    let mut held = RESIDENT.lock().unwrap_or_else(PoisonError::into_inner);
    let found = held
        .iter()
        .position(|r| r.repository == repository && r.episode == episode)
        .and_then(|i| held.remove(i))?;
    held.push_back(found.clone());
    Some(found)
}

/// The resident graph of `episode` of `repository`: reconstructed from the
/// recorded tables once, clustered, and kept for the next calls (at most
/// [`RESIDENT_LIMIT`] episodes). A recorded episode never changes, so the
/// entry stays valid; access is checked on every call before it is read.
/// Builds are serialized ([`BUILDING`]).
pub fn resident(db: &Connection, repository: &str, episode: &str) -> Result<Arc<Resident>> {
    if let Some(found) = held(repository, episode) {
        return Ok(found);
    }
    let _building = BUILDING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = held(repository, episode) {
        return Ok(found);
    }
    let mut graph = graph_projection::load(db, repository, episode)?;
    let communities = graphicle::cluster::cluster(&graph, 1.0, None, "auto")?;
    let labels = graphicle::cluster::label_communities_by_hub(&graph, &communities);
    // The community numbers the engine's export writes (`community`), so the
    // engine's tools print and group them as for a clustered graph.json.
    let mut assigned = vec![None; graph.nodes().len()];
    for (community, members) in &communities {
        for id in members {
            if let Some(i) = graph.node_index(id) {
                assigned[i] = Some(*community);
            }
        }
    }
    for (node, community) in graph.nodes_mut().zip(&assigned) {
        if let Some(community) = community {
            node.attributes.insert("community".into(), json!(community));
        }
    }
    let starts = code_graph_arm::symbol_starts(&graph);
    let mut by_location: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut by_label: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in graph.nodes().iter().enumerate() {
        // As the engine prints them: `sanitize` of the attribute's text.
        let text = |key: &str| {
            graphicle::serve_text::sanitize(
                &node
                    .attributes
                    .get(key)
                    .map(graphicle::serve_search::string)
                    .unwrap_or_default(),
            )
        };
        by_location
            .entry((text("source_file"), text("source_location")))
            .or_default()
            .push(i);
        by_label.entry(label(&graph, i)).or_default().push(i);
    }
    let built = Arc::new(Resident {
        repository: repository.to_string(),
        episode: episode.to_string(),
        context: GraphContext::new(graph)?,
        labels,
        starts,
        by_location,
        by_label,
    });
    let mut entries = RESIDENT.lock().unwrap_or_else(PoisonError::into_inner);
    entries.retain(|r| !(r.repository == repository && r.episode == episode));
    entries.push_back(built.clone());
    while entries.len() > RESIDENT_LIMIT {
        entries.pop_front();
    }
    Ok(built)
}

/// The `graph.navigate` operation: run the navigation tool `tool` (one of
/// [`NAVIGATION`]) on `repository_id`'s episode (`as_of`, else its current
/// code graph) and return the engine's text with the episode read and the
/// nodes it names. Read on an earlier episode than the current files' (a
/// waiting projection, or `as_of`), each node the answer reaches through a
/// relation carries `relation_checked` ([`relation_checks`]), and
/// `relations_unchecked` counts the `false` ones. `nodes_unmatched` counts
/// the lines of the text that name a node this server could not tell
/// ([`named_nodes`]), so a change in the engine's text formats shows instead
/// of silently citing less.
pub fn call(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let tool = string(args, "tool", 40)?;
    if !NAVIGATION.contains(&tool) {
        return Err(Error::bad(format!(
            "Unknown graph tool {tool}; use one of {}",
            NAVIGATION.join(", ")
        )));
    }
    let engine = engine_arguments(args);
    store.authorized_read(workspace, token, "read", |db, p| {
        let repository = repo(db, p, string(args, "repository_id", 100)?)?;
        let identifier = s(&repository, "id");
        let as_of = match args.get("as_of") {
            None | Some(Value::Null) => None,
            Some(_) => Some(string(args, "as_of", 128)?),
        };
        let reading = reading(db, p, identifier, as_of)?;
        let resident = resident(db, identifier, &reading.episode)?;
        let text = graphicle::serve::call_graph_tool(&resident.context, tool, &engine, None)?;
        let named = named_nodes(&resident, tool, &engine, &text);
        let omitted = named.nodes.len().saturating_sub(CITED_LIMIT);
        // Under a lag (or `as_of`), whether a relation read from an unchanged
        // file reaches each node the answer reaches through a relation.
        let checks = (reading.episode != reading.current).then(|| {
            relation_checks(&resident.context.graph, &named, |path| {
                reading.current_file(path)
            })
        });
        let mut contents = HashMap::new();
        let mut nodes = named
            .nodes
            .iter()
            .take(CITED_LIMIT)
            .map(|&i| {
                let mut fact = citation(db, identifier, &resident, &reading, i, &mut contents)?;
                if let Some(checked) = checks.as_ref().and_then(|checks| checks.get(&i)) {
                    fact["relation_checked"] = json!(checked);
                }
                Ok(fact)
            })
            .collect::<Result<Vec<_>>>()?;
        // The symbol `get_node` describes, with what links to it.
        if tool == "get_node" {
            crate::symbol_links::attach(db, p, &repository, &mut nodes)?;
        }
        let mut reply = json!({"tool":tool,"text":text,"repo":identifier,
            "episode":reading.episode,"current_episode":reading.current,"episode_lag":reading.lag(),
            "nodes":nodes});
        if let Some(selector) = as_of {
            reply["as_of"] = json!(selector);
        }
        if omitted > 0 {
            reply["nodes_omitted"] = json!(omitted);
        }
        if named.unmatched > 0 {
            reply["nodes_unmatched"] = json!(named.unmatched);
        }
        if let Some(checks) = &checks {
            reply["relations_unchecked"] = json!(checks.values().filter(|c| !**c).count());
        }
        Ok(reply)
    })
}

/// The short block that follows the engine's text in a graph tool's
/// `content`: many clients show the model `content` only.
/// Its first line names the episode read (abbreviated) and, when that is not
/// the current files' graph, how many files changed since; then, for at most
/// [`CITED_IN_TEXT`] cited nodes, the `file:` line range, the start of the
/// `stable_id` and what is not checked: a file whose bytes changed since the
/// episode, and, under a lag, a node the answer reaches only through
/// relations read from changed files. `structuredContent` has every citation
/// with its byte range and the whole episode id.
pub fn citations_text(reply: &Value) -> String {
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    let mut out = format!(
        "Citations (code-graph episode {}",
        abbreviated(reply["episode"].as_str().unwrap_or(""), EPISODE_IN_TEXT)
    );
    let as_of = reply["as_of"].as_str();
    if let Some(selector) = as_of {
        out.push_str(&format!(", as_of {selector}"));
    }
    match reply["episode_lag"].as_u64() {
        Some(0) => out.push_str(", current"),
        lag => {
            if as_of.is_none() {
                out.push_str(", the previous graph while the current one waits for its projection");
            }
            out.push_str(&match lag {
                Some(n) => format!(
                    ": {n} file{} changed since, and relations read from changed files may no longer hold",
                    plural(n)
                ),
                None => ": its files were not recorded, so no range is checked".into(),
            });
        }
    }
    let nodes = reply["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if nodes.is_empty() {
        out.push_str("): no node cited.");
    } else {
        out.push_str("; file: ranges checked unless marked, read_project_source reads them):");
    }
    for node in nodes.iter().take(CITED_IN_TEXT) {
        out.push_str(&format!("\n- {}", node["label"].as_str().unwrap_or("")));
        match node["relpath"].as_str() {
            Some(path) => {
                out.push_str(&format!(" file:{path}"));
                match (node["line"].as_u64(), node["end_line"].as_u64()) {
                    (Some(start), Some(end)) if end > start => {
                        out.push_str(&format!(" L{start}-{end}"))
                    }
                    (Some(start), _) => out.push_str(&format!(" L{start}")),
                    _ => {}
                }
            }
            None => out.push_str(" (no file)"),
        }
        if let Some(stable) = node["stable_id"].as_str() {
            out.push_str(&format!(
                " stable_id {}",
                abbreviated(stable, STABLE_ID_IN_TEXT)
            ));
        }
        if node["checked"] != true && node["relpath"].is_string() {
            out.push_str(&format!(
                " (not checked: {})",
                node["reason"].as_str().unwrap_or("unknown")
            ));
        }
        if node["relation_checked"] == false {
            out.push_str(" (reached only through relations read from changed files)");
        }
        out.push_str(&linked_text(&node["linked"]));
    }
    let more = nodes.len().saturating_sub(CITED_IN_TEXT) as u64;
    if more > 0 {
        out.push_str(&format!(
            "\n- {more} more cited node{} in structuredContent",
            plural(more)
        ));
    }
    if let Some(omitted) = reply["nodes_omitted"].as_u64().filter(|n| *n > 0) {
        out.push_str(&format!(
            "\n- {omitted} more named node{} not cited (at most {CITED_LIMIT} are)",
            plural(omitted)
        ));
    }
    if let Some(unmatched) = reply["nodes_unmatched"].as_u64().filter(|n| *n > 0) {
        out.push_str(&format!(
            "\n- {unmatched} line{} of the answer name{} a node this server could not tell apart; not cited",
            plural(unmatched),
            if unmatched == 1 { "s" } else { "" }
        ));
    }
    out
}

/// Back-links named per node in the citations block.
const LINKED_IN_TEXT: usize = 3;

/// A node's back-links in the citations block: the reviewed memories (by
/// title and id) and documents linked to it, at most [`LINKED_IN_TEXT`] of
/// each named; empty when nothing links to it.
fn linked_text(linked: &Value) -> String {
    let list = |key: &str| linked[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let (memories, documents) = (list("memories"), list("documents"));
    let mut out = String::new();
    let more = |n: usize| {
        let extra = n.saturating_sub(LINKED_IN_TEXT);
        if extra > 0 {
            format!(" and {extra} more")
        } else {
            String::new()
        }
    };
    if !memories.is_empty() {
        let named: Vec<String> = memories
            .iter()
            .take(LINKED_IN_TEXT)
            .map(|m| {
                format!(
                    "{} ({})",
                    m["title"].as_str().unwrap_or(""),
                    m["id"].as_str().unwrap_or("")
                )
            })
            .collect();
        out.push_str(&format!(
            "\n  reviewed memory linked: {}{}",
            named.join("; "),
            more(memories.len())
        ));
    }
    if !documents.is_empty() {
        let named: Vec<&str> = documents
            .iter()
            .take(LINKED_IN_TEXT)
            .map(|d| d["document_id"].as_str().unwrap_or(""))
            .collect();
        out.push_str(&format!(
            "\n  documents mentioning it: {}{}",
            named.join(", "),
            more(documents.len())
        ));
    }
    out
}

/// The first `n` characters of `text`, and `…` when there are more.
fn abbreviated(text: &str, n: usize) -> String {
    let shown: String = text.chars().take(n).collect();
    if shown.len() < text.len() {
        format!("{shown}…")
    } else {
        shown
    }
}

/// A pull-request tool (one of [`PULL_REQUESTS`]) for the connection's
/// project, over stdio only: `gh` and `git` run in this process's folder,
/// which `graphicle mcp --pull-requests` checked is the project's own. The
/// impact `get_pr_impact` and `triage_prs` report is read on the project's
/// current code graph; `list_prs` reads no graph, so it answers before one is
/// recorded (`episode` null).
pub fn pull_request(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
    tool: &str,
    args: &Value,
) -> Result<Value> {
    if !PULL_REQUESTS.contains(&tool) {
        return Err(Error::bad("Unknown pull-request tool"));
    }
    let impact = tool != "list_prs";
    let (identifier, episode, resident) =
        store.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id").to_string();
            if !impact {
                return Ok((identifier, None, None));
            }
            let reading = reading(db, p, &identifier, None)?;
            let resident = resident(db, &identifier, &reading.episode)?;
            Ok((identifier, Some(reading.episode), Some(resident)))
        })?;
    // `gh` may take its time: outside the store's read transaction.
    let arguments = engine_arguments(args);
    let text = match &resident {
        Some(resident) => {
            graphicle::serve::call_graph_tool(&resident.context, tool, &arguments, None)?
        }
        None => graphicle::serve::call_graph_tool(
            &GraphContext::new(Graph::new(true, false))?,
            tool,
            &arguments,
            None,
        )?,
    };
    Ok(json!({"tool":tool,"text":text,"repo":identifier,"episode":episode}))
}

/// The resources of the engine's server (`graphicle-graph://report`, stats,
/// god-nodes, surprises, audit and questions), for the connection's project.
pub fn resources() -> Value {
    graphicle::serve::resources_list()
}

/// Read one of [`resources`] on the project's current code graph, as the
/// engine's server reads it on its graph; the report is generated from the
/// episode, as `cluster-only` writes `GRAPH_REPORT.md`.
pub fn read_resource(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
    uri: &str,
) -> Result<String> {
    store.authorized_read(workspace, token, "read", |db, p| {
        let repository = repo(db, p, repository)?;
        let identifier = s(&repository, "id");
        let reading = reading(db, p, identifier, None)?;
        let resident = resident(db, identifier, &reading.episode)?;
        let ctx = &resident.context;
        let g = &ctx.graph;
        // Members by node id, in the order the engine's server derives them.
        let communities: graphicle::cluster::Communities = ctx
            .communities
            .iter()
            .map(|(cid, members)| (*cid, members.iter().map(|i| g.nodes()[*i].id.clone()).collect()))
            .collect();
        let labels = resident.labels.as_object().cloned().unwrap_or_default();
        Ok(match uri {
            "graphicle-graph://report" => graphicle::report::generate(
                g,
                &json!({"communities":graphicle::cluster::communities_to_json(&communities),
                    "cohesion_scores":graphicle::cluster::score_all(g, &communities),
                    "community_labels":resident.labels,
                    "god_node_list":graphicle::analyze::god_nodes(g, 10, None)?,
                    "surprise_list":graphicle::analyze::surprising_connections(g, &communities, graphicle::analyze::SURPRISES_TOP_N)?,
                    "detection_result":{"warning":format!("Graphicle project episode {}: files and their coverage are in repository_status", reading.episode)},
                    "token_cost":{"input":0,"output":0},
                    "root":s(&repository, "name"),
                    "suggested_questions":graphicle::analyze::suggest_questions(g, &communities, &labels, graphicle::analyze::QUESTIONS_TOP_N)?,
                    "min_community_size":3,"built_at_commit":null,"learning":null}),
            )?,
            "graphicle-graph://stats" => {
                graphicle::serve::call_graph_tool(ctx, "graph_stats", &json!({}), None)?
            }
            "graphicle-graph://audit" => audit(g),
            "graphicle-graph://god-nodes" => {
                graphicle::serve::call_graph_tool(ctx, "god_nodes", &json!({"top_n":10}), None)?
            }
            "graphicle-graph://surprises" => {
                let found = graphicle::analyze::surprising_connections(g, &communities, 10)?;
                if found.is_empty() {
                    "No surprising connections found.".into()
                } else {
                    let text = graphicle::serve_search::text;
                    std::iter::once("Surprising cross-community connections:".to_string())
                        .chain(found.iter().map(|c| {
                            format!(
                                "  {} <-> {} [{}]",
                                text(&c["source"]),
                                text(&c["target"]),
                                text(&c["relation"])
                            )
                        }))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "graphicle-graph://questions" => {
                let questions = graphicle::analyze::suggest_questions(g, &communities, &labels, 10)?;
                if questions.is_empty() {
                    "No suggested questions available.".into()
                } else {
                    std::iter::once("Suggested questions:".to_string())
                        .chain(questions.iter().map(|q| {
                            format!(
                                "  - {}",
                                if q.is_object() {
                                    graphicle::serve_search::text(&q["question"])
                                } else {
                                    graphicle::serve_search::string(q)
                                }
                            )
                        }))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            _ => return Err(Error::bad(format!("Unknown resource: {uri}"))),
        })
    })
}

/// The audit resource: the engine's edge breakdown by confidence label.
fn audit(g: &Graph) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for (_, _, edge) in g.iter_edges() {
        *counts
            .entry(
                edge.attributes
                    .get("confidence")
                    .map(graphicle::serve_search::string)
                    .unwrap_or_else(|| "EXTRACTED".into()),
            )
            .or_default() += 1;
    }
    let total = g.edge_count().max(1);
    let mut out = format!("Total edges: {total}\n");
    for key in ["EXTRACTED", "INFERRED", "AMBIGUOUS"] {
        let count = counts.get(key).copied().unwrap_or(0);
        let percent = (count as f64 / total as f64 * 100.0).round_ties_even() as i64;
        out.push_str(&format!("{key}: {count} ({percent}%)\n"));
    }
    out
}

/// The node an engine text names on its `  ID: <id>` line (`get_node`).
fn node_of(g: &Graph, text: &str) -> Option<usize> {
    text.lines()
        .find_map(|line| line.strip_prefix("  ID: "))
        .and_then(|id| g.node_index(&json!(id)))
}

/// How the engine's tools print node `i`'s label on a NODE, member or
/// neighbour line: `sanitize(label)` (the Graphify 0.9.77 port) or
/// `sanitize_label` of the label attribute (`label_text`, the 0.9.88 port).
/// They differ only for a null label.
fn printed_labels(g: &Graph, i: usize) -> [String; 2] {
    let node = &g.nodes()[i];
    [
        graphicle::serve_text::sanitize(&label(g, i)),
        graphicle::security::sanitize_label(node.attributes.get("label").unwrap_or(&node.id)),
    ]
}

/// The node of one `query_graph` NODE line (`NODE <label> [src=<file>
/// loc=<location> community=…]`): the only node at that file and location,
/// else the only one there with that label.
fn node_line(resident: &Resident, line: &str) -> Option<usize> {
    let g = &resident.context.graph;
    let rest = line.strip_prefix("NODE ")?;
    let (name, rest) = rest.split_once(" [src=")?;
    let (file, rest) = rest.split_once(" loc=")?;
    let location = rest
        .split_once(" community=")
        .map_or(rest, |(location, _)| location);
    let found = resident
        .by_location
        .get(&(file.to_string(), location.to_string()))?;
    if found.len() == 1 {
        return Some(found[0]);
    }
    let named: Vec<usize> = found
        .iter()
        .copied()
        .filter(|i| printed_labels(g, *i).iter().any(|printed| printed == name))
        .collect();
    (named.len() == 1).then(|| named[0])
}

/// How many start nodes a `query_graph` answer begins from: the labels of
/// its header's `Start: [...]` list (a Python list of strings). Its first
/// NODE lines are those nodes, in that order.
fn start_count(text: &str) -> usize {
    let head = text.lines().next().unwrap_or("");
    let Some(at) = ["Start: [", "Start (given): ["]
        .iter()
        .find_map(|prefix| head.find(prefix).map(|at| at + prefix.len()))
    else {
        return 0;
    };
    let mut count = 0;
    let mut chars = head[at..].chars();
    while let Some(c) = chars.next() {
        match c {
            ']' => break,
            '\'' | '"' => {
                count += 1;
                while let Some(d) = chars.next() {
                    if d == '\\' {
                        chars.next();
                    } else if d == c {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// The nodes of the path a `shortest_path` answer prints, in order: each
/// printed label among the nodes of that label that are adjacent to a
/// candidate for the previous and the next position, so the path itself
/// tells apart nodes that share a label; `None` where that leaves no node or
/// several.
fn path_nodes(resident: &Resident, labels: &[&str]) -> Vec<Option<usize>> {
    let g = &resident.context.graph;
    let adjacent = |i: usize, others: &HashSet<usize>| {
        neighbors(g, i, false)
            .into_iter()
            .chain(neighbors(g, i, true))
            .any(|j| others.contains(&j))
    };
    let mut sets: Vec<Vec<usize>> = labels
        .iter()
        .map(|name| resident.by_label.get(*name).cloned().unwrap_or_default())
        .collect();
    for i in 1..sets.len() {
        let before: HashSet<usize> = sets[i - 1].iter().copied().collect();
        sets[i].retain(|n| adjacent(*n, &before));
    }
    for i in (0..sets.len().saturating_sub(1)).rev() {
        let after: HashSet<usize> = sets[i + 1].iter().copied().collect();
        sets[i].retain(|n| adjacent(*n, &after));
    }
    sets.into_iter()
        .map(|set| (set.len() == 1).then(|| set[0]))
        .collect()
}

/// The nodes an engine answer names, and how it reaches them.
struct Named {
    /// The nodes the answer names, in its order, each once.
    nodes: Vec<usize>,
    /// How the answer reaches them through relations ([`relation_checks`]).
    walk: Walk,
    /// Lines of the answer that name a node none of `nodes` stands for: a
    /// line several nodes match, or text this server does not read as the
    /// engine's (a format change shows here instead of citing less).
    unmatched: usize,
}

/// How an answer reaches the nodes it names through relations.
enum Walk {
    /// Through none: `get_node`, `get_community`, `god_nodes`, `graph_stats`.
    None,
    /// `get_neighbors`: every other node through a relation with this one.
    Star(usize),
    /// `query_graph`: from these start nodes, along relations among the
    /// nodes it names.
    From(Vec<usize>),
    /// `shortest_path`: along the path, in order (`None` where a position
    /// could not be told).
    Path(Vec<Option<usize>>),
}

/// The nodes the engine's answer names, in its order, as far as they can be
/// told exactly: `get_node`'s node (its `ID` line); `get_neighbors`' node
/// and the neighbours its text lists (the engine's selection, cut where its
/// text is cut); `get_community`'s listed members; `god_nodes`' nodes;
/// `query_graph`'s NODE lines matched by file, location and label; and
/// `shortest_path`'s path ([`path_nodes`]). A line that matches several
/// nodes is left out rather than guessed, and counted in `unmatched`. The
/// formats read here are the engine's on both its 0.9.77 and 0.9.88 ports
/// (labels printed raw, with `sanitize` or with `label_text`; the query
/// header's `Start:` or `Start (given):`); the tests pin them (`one_server`
/// and the unit tests below).
fn named_nodes(resident: &Resident, tool: &str, args: &Value, text: &str) -> Named {
    let ctx = &resident.context;
    let g = &ctx.graph;
    let starting = |prefix: &str| text.lines().filter(|line| line.starts_with(prefix)).count();
    let (mut nodes, walk, unmatched): (Vec<usize>, Walk, usize) = match tool {
        "get_node" => {
            let found: Vec<usize> = node_of(g, text).into_iter().collect();
            let unmatched = starting("  ID: ").saturating_sub(found.len());
            (found, Walk::None, unmatched)
        }
        "get_neighbors" => {
            let shown = starting("  --> ") + starting("  <-- ");
            let Some(center) = graphicle::serve::call_graph_tool(ctx, "get_node", args, None)
                .ok()
                .and_then(|node| node_of(g, &node))
            else {
                return Named {
                    nodes: Vec::new(),
                    walk: Walk::None,
                    unmatched: shown + starting("Neighbors of "),
                };
            };
            let filter = args["relation_filter"]
                .as_str()
                .unwrap_or("")
                .to_lowercase();
            let mut listed = Vec::new();
            for incoming in [false, true] {
                for neighbour in neighbors(g, center, incoming) {
                    let (a, b) = if incoming {
                        (neighbour, center)
                    } else {
                        (center, neighbour)
                    };
                    // The engine prints one line per relationship: a pair
                    // joined by parallel edges is listed once per edge
                    // (Graphify 0.9.92, #4500).
                    for edge in g.edges_between(&g.nodes()[a].id, &g.nodes()[b].id) {
                        let relation = edge
                            .attributes
                            .get("relation")
                            .map(graphicle::serve_search::string)
                            .unwrap_or_default();
                        if filter.is_empty() || relation.to_lowercase().contains(&filter) {
                            listed.push(neighbour);
                        }
                    }
                }
            }
            let unmatched = shown.saturating_sub(listed.len());
            (
                std::iter::once(center)
                    .chain(listed.into_iter().take(shown))
                    .collect(),
                Walk::Star(center),
                unmatched,
            )
        }
        "get_community" => {
            let shown = starting("  ");
            let members: Vec<usize> = args["community_id"]
                .as_i64()
                .and_then(|id| ctx.communities.iter().find(|(cid, _)| *cid == id))
                .map(|(_, members)| members.iter().copied().take(shown).collect())
                .unwrap_or_default();
            let unmatched = shown.saturating_sub(members.len());
            (members, Walk::None, unmatched)
        }
        "god_nodes" => {
            let found: Vec<usize> = graphicle::analyze::god_nodes(
                g,
                args["top_n"].as_i64().unwrap_or(10),
                args["exclude_hubs_percentile"].as_f64(),
            )
            .unwrap_or_default()
            .iter()
            .filter_map(|node| g.node_index(&node["id"]))
            .collect();
            let unmatched = starting("  ").saturating_sub(found.len());
            (found, Walk::None, unmatched)
        }
        "query_graph" => {
            let starts = start_count(text);
            let mut found = Vec::new();
            let mut roots = Vec::new();
            let mut unmatched = 0;
            for (position, line) in text
                .lines()
                .filter(|line| line.starts_with("NODE "))
                .enumerate()
            {
                match node_line(resident, line) {
                    Some(i) => {
                        if position < starts {
                            roots.push(i);
                        }
                        found.push(i);
                    }
                    None => unmatched += 1,
                }
            }
            (found, Walk::From(roots), unmatched)
        }
        "shortest_path" => {
            let path = path_labels(text)
                .map(|labels| path_nodes(resident, &labels))
                .unwrap_or_default();
            let unmatched = path.iter().filter(|node| node.is_none()).count();
            (
                path.iter().flatten().copied().collect(),
                Walk::Path(path),
                unmatched,
            )
        }
        _ => (Vec::new(), Walk::None, 0),
    };
    let mut seen = BTreeSet::new();
    nodes.retain(|i| seen.insert(*i));
    Named {
        nodes,
        walk,
        unmatched,
    }
}

/// The labels of the path a `shortest_path` answer prints, in order: the
/// line after `Shortest path (N hops):`, split at its `--relation [conf]-->`
/// and `<--relation [conf]--` separators. `None` when the answer has no path.
fn path_labels(text: &str) -> Option<Vec<&str>> {
    static SEPARATOR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r" (?:--\S*?(?: \[[^\]]*\])?--> |<--\S*?(?: \[[^\]]*\])?-- )")
            .expect("path separator")
    });
    let path = text
        .lines()
        .skip_while(|line| !line.starts_with("Shortest path ("))
        .nth(1)?;
    Some(SEPARATOR.split(path.trim_start()).collect())
}

/// Under a lag (or `as_of`), for each node `named` reaches through a
/// relation, whether a relation read from a file whose current bytes are the
/// ones the episode recorded (`current`) reaches it, the rule the code-graph
/// arm applies to the neighbours it cites: `get_neighbors`' nodes
/// through an edge with the node asked about; `query_graph`'s through a
/// chain of such edges among the nodes it names, from a start node;
/// `shortest_path`'s along every hop of the path up to them. An edge that
/// names no file is not current. The engine's text is not changed.
fn relation_checks(
    g: &Graph,
    named: &Named,
    current: impl Fn(&str) -> bool,
) -> BTreeMap<usize, bool> {
    let current_between = |a: usize, b: usize| {
        let (x, y) = (&g.nodes()[a].id, &g.nodes()[b].id);
        g.edges_between(x, y)
            .into_iter()
            .chain(g.edges_between(y, x))
            .any(|edge| {
                code_graph_arm::edge_files(&edge.attributes)
                    .iter()
                    .any(|file| current(file))
            })
    };
    let mut checks = BTreeMap::new();
    match &named.walk {
        Walk::None => {}
        Walk::Star(center) => {
            for &i in named.nodes.iter().filter(|i| *i != center) {
                checks.insert(i, current_between(*center, i));
            }
        }
        Walk::From(roots) => {
            let members: HashSet<usize> = named.nodes.iter().copied().collect();
            let mut reached: HashSet<usize> = roots.iter().copied().collect();
            let mut queue: VecDeque<usize> = roots.iter().copied().collect();
            while let Some(i) = queue.pop_front() {
                for j in neighbors(g, i, false)
                    .into_iter()
                    .chain(neighbors(g, i, true))
                {
                    if members.contains(&j) && !reached.contains(&j) && current_between(i, j) {
                        reached.insert(j);
                        queue.push_back(j);
                    }
                }
            }
            for &i in named.nodes.iter().filter(|i| !roots.contains(i)) {
                checks.insert(i, reached.contains(&i));
            }
        }
        Walk::Path(path) => {
            let mut checked = true;
            for pair in path.windows(2) {
                checked = checked && matches!(pair, [Some(a), Some(b)] if current_between(*a, *b));
                if let Some(b) = pair[1] {
                    checks.insert(b, checked);
                }
            }
        }
    }
    checks
}

/// What a cited node is and where its source is: `stable_id`, `revision`,
/// the episode, its line range and, when its file's current bytes are the
/// ones the episode recorded, its checked `file:` byte range with the file's
/// sha256 (`checked`); otherwise the reason it is not checked.
fn citation(
    db: &Connection,
    repository: &str,
    resident: &Resident,
    reading: &Reading,
    i: usize,
    contents: &mut HashMap<String, Option<String>>,
) -> Result<Value> {
    let g = &resident.context.graph;
    let node = &g.nodes()[i];
    let field = |key: &str| node.attributes.get(key).cloned().unwrap_or(Value::Null);
    let mut fact = json!({"node_id":node.id,"label":field("label"),"kind":field("kind"),
        "qualified_name":field("qualified_name"),"stable_id":field("stable_id"),
        "revision":field("revision"),"episode":reading.episode});
    if let Some(community) = node.attributes.get("community") {
        fact["community"] = community.clone();
    }
    let Some(path) = code_graph_arm::relpath(&node.attributes) else {
        fact["checked"] = json!(false);
        fact["reason"] = json!("no_file");
        return Ok(fact);
    };
    fact["relpath"] = json!(path);
    if let Some(line) = span::start_line(&node.attributes) {
        fact["line"] = json!(line);
    }
    if !reading.current_file(&path) {
        fact["checked"] = json!(false);
        fact["reason"] = json!(if reading.heads.contains_key(&path) {
            "changed_since_episode"
        } else {
            "not_in_current_files"
        });
        return Ok(fact);
    }
    let sha = reading.heads[&path].clone();
    let content = match contents.get(&path) {
        Some(content) => content.clone(),
        None => {
            let content = one(
                db,
                "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
                params![repository, sha],
            )?
            .map(|row| s(&row, "content").to_string())
            .filter(|content| graphicle_memory::text::sha256(content.as_bytes()) == sha);
            contents.insert(path.clone(), content.clone());
            content
        }
    };
    let Some(content) = content else {
        fact["checked"] = json!(false);
        fact["reason"] = json!("source_unavailable");
        return Ok(fact);
    };
    let offsets = span::line_starts(&content);
    // A file node stands for its whole file, the bytes its `revision`
    // hashes; a symbol for its own lines, as the code-graph arm cites it.
    let (line, end_line, byte_start, byte_end) =
        if node.attributes.get("kind") == Some(&json!("file")) {
            (1, offsets.len(), 0, content.len())
        } else {
            code_graph_arm::node_span(
                &node.attributes,
                &path,
                &resident.starts,
                &offsets,
                content.len(),
            )
        };
    if byte_end <= byte_start {
        fact["checked"] = json!(false);
        fact["reason"] = json!("empty_range");
        return Ok(fact);
    }
    fact["source_id"] = json!(format!("file:{path}"));
    fact["sha256"] = json!(sha);
    fact["line"] = json!(line);
    fact["end_line"] = json!(end_line);
    fact["byte_start"] = json!(byte_start);
    fact["byte_end"] = json!(byte_end);
    fact["checked"] = json!(true);
    Ok(fact)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rewrite is generic over the engine's specs: `project_path` goes,
    /// the pull-request tools' GitHub `repo` becomes `github_repo` (also in
    /// `required`), the navigation tools gain `repo` and `as_of`, and every
    /// schema is strict.
    #[test]
    fn engine_specs_are_rewritten_for_the_unified_server() {
        let navigation = published(
            &json!({"name":"x","description":"d","inputSchema":{"type":"object",
                "properties":{"question":{"type":"string"},"project_path":{"type":"string"}},
                "required":["question","project_path"]}}),
            false,
        );
        let schema = &navigation["inputSchema"];
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["question", "repo", "as_of"]
        );
        assert_eq!(schema["required"], json!(["question"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(navigation["annotations"]["readOnlyHint"], true);
        let pull = published(
            &json!({"name":"y","description":"d","inputSchema":{"type":"object",
                "properties":{"repo":{"type":"string","description":"GitHub"},"pr_number":{"type":"integer"}},
                "required":["repo"]}}),
            true,
        );
        let schema = &pull["inputSchema"];
        assert!(schema["properties"].get("repo").is_none(), "{schema}");
        assert_eq!(schema["properties"]["github_repo"]["description"], "GitHub");
        assert!(schema["properties"].get("as_of").is_none());
        assert_eq!(schema["required"], json!(["github_repo"]));
        assert_eq!(pull["annotations"]["openWorldHint"], true);
        assert_eq!(
            engine_arguments(
                &json!({"tool":"list_prs","github_repo":"o/r","repository_id":"p","repo":"p","base":"main"})
            ),
            json!({"base":"main","repo":"o/r"})
        );
    }

    /// The unified list: Graphify's navigation names in the engine's order,
    /// never `retrieve_context`, the pull-request tools only when asked for.
    #[test]
    fn the_engine_tools_published_and_their_operations() {
        let names = |tools: Vec<McpTool>| -> Vec<String> {
            tools.iter().map(|t| t.name().to_string()).collect()
        };
        assert_eq!(names(tools(false)), NAVIGATION);
        let all = tools(true);
        assert_eq!(
            names(tools(true))[NAVIGATION.len()..],
            PULL_REQUESTS.map(String::from)
        );
        for tool in &all {
            let navigation = NAVIGATION.contains(&tool.name());
            assert_eq!(tool.repo_arg, navigation, "{}", tool.name());
            assert_eq!(
                tool.operation,
                if navigation { NAVIGATE } else { PULL_REQUEST }
            );
            assert!(is_graph_tool(tool));
            assert!(
                tool.spec["inputSchema"]["properties"]
                    .get("project_path")
                    .is_none()
            );
        }
        assert!(!names(all).contains(&"retrieve_context".to_string()));
    }

    /// The text formats the citations read, as the engine prints them on its
    /// 0.9.77 port (this tree) and its 0.9.88 port (`Start (given):`, labels
    /// through `label_text`): the query header's start labels (a Python list
    /// repr) and the path line of `shortest_path`.
    #[test]
    fn the_engine_text_formats_the_citations_read() {
        let header = |start: &str| {
            format!(
                "Traversal: BFS depth=3 | {start} | 4 nodes found\n\nNODE a() [src=a.py loc=L1 community=0]"
            )
        };
        assert_eq!(start_count(&header("Start: ['run()', 'alpha()']")), 2);
        assert_eq!(start_count(&header("Start (given): ['run()']")), 1);
        // Quotes and brackets inside a label, as Python's repr writes them.
        assert_eq!(
            start_count(&header(r#"Start: ["it's()", 'a]b', 'c\'d']"#)),
            3
        );
        assert_eq!(start_count("No matching nodes found."), 0);
        assert_eq!(
            path_labels(
                "Shortest path (3 hops):\n  run() --calls [EXTRACTED]--> alpha() <--contains [EXTRACTED]-- a.py --imports--> b.py"
            ),
            Some(vec!["run()", "alpha()", "a.py", "b.py"])
        );
        assert_eq!(
            path_labels(
                "warning: source match was ambiguous (top score 2, runner-up 1.9)\nShortest path (1 hops):\n  x() --calls/uses [EXTRACTED/INFERRED]--> y()"
            ),
            Some(vec!["x()", "y()"])
        );
        assert_eq!(path_labels("No path found between 'a' and 'b'."), None);
    }

    /// The citations block of `content`: the episode, and what is not
    /// checked, compactly; a lag and the unmatched lines are said.
    #[test]
    fn the_citations_block_names_the_episode_and_what_is_not_checked() {
        let node = |label: &str, checked: bool| {
            let mut node = json!({"label":label,"relpath":"a.py","line":1,"end_line":3,
                "stable_id":"0123456789abcdef0123","checked":checked});
            if !checked {
                node["reason"] = json!("changed_since_episode");
            }
            node
        };
        let current = citations_text(&json!({"episode":"b75761d5b618bf3eae9b","episode_lag":0,
            "nodes":[node("alpha()", true)]}));
        assert_eq!(
            current,
            "Citations (code-graph episode b75761d5b618…, current; file: ranges checked unless marked, read_project_source reads them):\n- alpha() file:a.py L1-3 stable_id 0123456789ab…"
        );
        let mut stale = node("run()", true);
        stale["relation_checked"] = json!(false);
        let lagging = citations_text(&json!({"episode":"e1","episode_lag":2,
            "nodes":[node("alpha()", false), stale],"nodes_unmatched":1}));
        assert!(lagging.starts_with("Citations (code-graph episode e1, the previous graph while the current one waits for its projection: 2 files changed since, and relations read from changed files may no longer hold; file: ranges"), "{lagging}");
        assert!(lagging.contains("- alpha() file:a.py L1-3 stable_id 0123456789ab… (not checked: changed_since_episode)"), "{lagging}");
        assert!(lagging.contains("- run() file:a.py L1-3 stable_id 0123456789ab… (reached only through relations read from changed files)"), "{lagging}");
        assert!(
            lagging.ends_with(
                "- 1 line of the answer names a node this server could not tell apart; not cited"
            ),
            "{lagging}"
        );
        let earlier =
            citations_text(&json!({"episode":"e0","as_of":"e0","episode_lag":1,"nodes":[]}));
        assert_eq!(
            earlier,
            "Citations (code-graph episode e0, as_of e0: 1 file changed since, and relations read from changed files may no longer hold): no node cited."
        );
    }

    #[test]
    fn audit_counts_edges_by_confidence() {
        let mut g = Graph::new(true, false);
        for id in ["a", "b", "c"] {
            g.add_node(json!(id), serde_json::Map::new()).unwrap();
        }
        let edge = |confidence: Option<&str>| {
            let mut fields = serde_json::Map::new();
            if let Some(confidence) = confidence {
                fields.insert("confidence".into(), json!(confidence));
            }
            fields
        };
        g.add_edge(json!("a"), json!("b"), None, edge(Some("INFERRED")))
            .unwrap();
        g.add_edge(json!("b"), json!("c"), None, edge(None))
            .unwrap();
        g.add_edge(json!("a"), json!("c"), None, edge(Some("EXTRACTED")))
            .unwrap();
        assert_eq!(
            audit(&g),
            "Total edges: 3\nEXTRACTED: 2 (67%)\nINFERRED: 1 (33%)\nAMBIGUOUS: 0 (0%)\n"
        );
    }
    /// The engine prints a pair joined by parallel edges once per edge
    /// (Graphify 0.9.92, #4500); the citations read one neighbour per printed
    /// line, so none is left unmatched, with or without a relation filter.
    #[test]
    fn get_neighbors_citations_follow_parallel_edges() {
        let graph = Graph::from_node_link(
            &json!({"directed": true, "multigraph": true, "graph": {},
                "nodes": [
                    {"id": "alpha", "label": "alpha()", "source_file": "a.py", "source_location": "L1"},
                    {"id": "beta", "label": "beta()", "source_file": "a.py", "source_location": "L4"},
                    {"id": "gamma", "label": "gamma()", "source_file": "b.py", "source_location": "L1"}],
                "links": [
                    {"source": "alpha", "target": "beta", "relation": "calls", "key": "calls"},
                    {"source": "alpha", "target": "beta", "relation": "references", "key": "references"},
                    {"source": "gamma", "target": "alpha", "relation": "calls", "key": "calls"}]}),
            "links",
        )
        .unwrap();
        let resident = Resident {
            repository: "r".into(),
            episode: "e".into(),
            context: GraphContext::new(graph).unwrap(),
            labels: json!({}),
            starts: HashMap::new(),
            by_location: HashMap::new(),
            by_label: HashMap::new(),
        };
        let index = |id: &str| resident.context.graph.node_index(&json!(id)).unwrap();
        let (alpha, beta, gamma) = (index("alpha"), index("beta"), index("gamma"));
        for (filter, lines, cited) in [
            ("", 3, vec![alpha, beta, gamma]),
            ("refer", 1, vec![alpha, beta]),
        ] {
            let args = json!({"label": "alpha", "relation_filter": filter});
            let text =
                graphicle::serve::call_graph_tool(&resident.context, "get_neighbors", &args, None)
                    .unwrap();
            let arrows = text
                .lines()
                .filter(|line| line.starts_with("  --> ") || line.starts_with("  <-- "))
                .count();
            assert_eq!(arrows, lines, "{text}");
            let named = named_nodes(&resident, "get_neighbors", &args, &text);
            assert_eq!(named.unmatched, 0, "{text}");
            assert_eq!(named.nodes, cited, "{text}");
        }
    }
}
