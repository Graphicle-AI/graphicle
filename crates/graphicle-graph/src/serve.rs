// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Native graph query service and MCP stdio boundary.
use crate::{Result, graph::Graph, paths, security, serve_find, serve_search::*, serve_text::*};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
pub fn load_graph(path: &Path) -> Result<Graph> {
    let path = checked_graph_path(path)?;
    let mut graph = read_graph_file(&path)?;
    graph.attributes["_learning_overlay"] =
        crate::reflect::load_learning_overlay(&path).unwrap_or(json!({}));
    Ok(graph)
}
/// The resolved graph path after the checks `load_graph` applies first.
fn checked_graph_path(path: &Path) -> Result<PathBuf> {
    let path = crate::extractors::resolution::resolved(path);
    if path.extension().and_then(|s| s.to_str()) != Some("json") {
        return Err(format!("Graph path must be a .json file, got: {}", path.display()).into());
    }
    if !path.exists() {
        return Err(format!("Graph file not found: {}", path.display()).into());
    }
    security::check_graph_file_size_cap(&path)?;
    Ok(path)
}
/// `load_graph` without the learning overlay: everything read from graph.json.
fn read_graph_file(path: &Path) -> Result<Graph> {
    let b = fs::read(path).map_err(crate::Error::from)?;
    let mut data = serde_json::from_slice::<Value>(&b).map_err(|e| {
        format!(
            "graph.json is corrupted ({e}). Re-run {} to rebuild.",
            crate::cli::program()
        )
    })?;
    if !data.is_object() {
        return Err("Graph JSON must be an object".into());
    }
    if data.get("links").is_none() {
        data["links"] = data.get_mut("edges").map(Value::take).unwrap_or(json!([]));
    }
    if let Some(links) = data.get_mut("links").and_then(Value::as_array_mut) {
        crate::extractors::evidence::filter_links(links);
    }
    let directed = data["directed"].as_bool().unwrap_or(false);
    data["directed"] = json!(true);
    // The node-link loader keeps only `graph` of the top-level keys, so the
    // build commit the graph.json writer stores is kept by hand (#3354).
    let commit = data
        .get("built_at_commit")
        .and_then(Value::as_str)
        .map(|s| s.trim_matches(crate::dedup_strings::whitespace))
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut graph = Graph::from_node_link_owned(data, "links")?;
    graph.attributes["_logical_directed"] = json!(directed);
    if let Some(commit) = commit {
        graph.attributes["_built_at_commit"] = json!(commit);
    }
    Ok(graph)
}
/// `GraphContext::new(load_graph(path)?)`, with the graph and its index read
/// from the query snapshot when it is current (`crate::snapshot`). The
/// learning overlay depends on more than graph.json and is always recomputed.
pub fn load_context(path: &Path) -> Result<GraphContext> {
    let path = checked_graph_path(path)?;
    let build = || {
        let graph = read_graph_file(&path)?;
        let index = SearchIndex::new(&graph);
        Ok((graph, index))
    };
    // A `--min-evidence` load has its own snapshot (the filtered graph).
    let (mut graph, index) = crate::snapshot::load(&path, crate::snapshot::Variant::Serve, build)?;
    graph.attributes["_learning_overlay"] =
        crate::reflect::load_learning_overlay(&path).unwrap_or(json!({}));
    GraphContext::with_index(graph, index)
}
pub fn communities(g: &Graph) -> Result<Vec<(i64, Vec<usize>)>> {
    let mut out = Vec::<(i64, Vec<usize>)>::new();
    for (i, n) in g.nodes.iter().enumerate() {
        let Some(v) = n.attributes.get("community").filter(|v| !v.is_null()) else {
            continue;
        };
        let cid = v
            .as_i64()
            .or_else(|| v.as_f64().map(|n| n as i64))
            .or_else(|| v.as_str()?.parse().ok())
            .ok_or("invalid community ID")?;
        if let Some((_, ns)) = out.iter_mut().find(|(c, _)| *c == cid) {
            ns.push(i);
        } else {
            out.push((cid, vec![i]));
        }
    }
    Ok(out)
}
pub struct GraphContext {
    pub graph: Graph,
    pub index: SearchIndex,
    pub communities: Vec<(i64, Vec<usize>)>,
}
impl GraphContext {
    pub fn new(graph: Graph) -> Result<Self> {
        let index = SearchIndex::new(&graph);
        Self::with_index(graph, index)
    }
    /// A context for `graph` with an index already built for it.
    pub fn with_index(graph: Graph, index: SearchIndex) -> Result<Self> {
        let communities = communities(&graph)?;
        Ok(Self {
            graph,
            index,
            communities,
        })
    }
}
#[derive(PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}
impl From<fs::Metadata> for FileStamp {
    fn from(meta: fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}
#[derive(PartialEq, Eq)]
struct ContextStamp {
    graph: FileStamp,
    learning: Option<FileStamp>,
}
struct Entry {
    path: PathBuf,
    key: ContextStamp,
    context: Arc<GraphContext>,
}
#[derive(Default)]
struct CacheState {
    entries: VecDeque<Entry>,
    pinned: BTreeMap<PathBuf, Entry>,
}
pub struct GraphContextCache {
    max: usize,
    state: Mutex<CacheState>,
}
impl GraphContextCache {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            state: Mutex::new(CacheState::default()),
        }
    }
    pub fn load(&self, path: &Path, pinned: bool) -> Result<Arc<GraphContext>> {
        let path = crate::extractors::resolution::resolved(path);
        let meta = path
            .metadata()
            .map_err(|e| format!("graph.json not found: {}: {e}", path.display()))?;
        let learning = path.with_file_name(crate::reflect::LEARNING_SIDECAR_NAME);
        let learning = match learning.metadata() {
            Ok(meta) => Some(meta.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let key = ContextStamp {
            graph: meta.into(),
            learning,
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if pinned {
            if let Some(e) = state.pinned.get(&path).filter(|e| e.key == key) {
                return Ok(e.context.clone());
            }
        } else if let Some(i) = state.entries.iter().position(|e| e.path == path)
            && let Some(entry) = state.entries.remove(i)
            && entry.key == key
        {
            let context = entry.context.clone();
            state.entries.push_back(entry);
            return Ok(context);
        }
        let context = Arc::new(load_context(&path)?);
        let entry = Entry {
            path: path.clone(),
            key,
            context: context.clone(),
        };
        if pinned {
            state.pinned.insert(path, entry);
        } else {
            state.entries.push_back(entry);
            while state.entries.len() > self.max {
                state.entries.pop_front();
            }
        }
        Ok(context)
    }
    pub fn cached_paths(&self) -> Vec<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect()
    }
}
pub fn max_contexts() -> usize {
    std::env::var("GRAPHICLE_MAX_CONTEXTS")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(|n| n.max(1) as usize)
        .unwrap_or(8)
}
pub fn community_header(cid: i64, name: Option<&Value>) -> String {
    let base = format!("Community {cid}");
    if let Some(name) = name.filter(|v| crate::build::truthy(v)) {
        let clean = sanitize(&string(name));
        if !clean.is_empty() && clean != base {
            return format!("{base} — {clean}");
        }
    }
    base
}
pub fn tools_list() -> Value {
    let mut tools = CONSTANTS["tools"].clone();
    // Opt-in (`serve --retrieval`): the default list is Graphify's.
    if crate::retrieve::enabled()
        && let Some(list) = tools.as_array_mut()
    {
        list.push(crate::retrieve::tool());
    }
    tools
}
pub fn resources_list() -> Value {
    json!([
 {"uri":"graphicle-graph://report","name":"Graph Report","description":"Full GRAPH_REPORT.md","mimeType":"text/markdown"},
 {"uri":"graphicle-graph://stats","name":"Graph Stats","description":"Node/edge/community counts and confidence breakdown","mimeType":"text/plain"},
 {"uri":"graphicle-graph://god-nodes","name":"God Nodes","description":"Top 10 most-connected nodes","mimeType":"text/plain"},
 {"uri":"graphicle-graph://surprises","name":"Surprising Connections","description":"Cross-community surprising connections","mimeType":"text/plain"},
 {"uri":"graphicle-graph://audit","name":"Confidence Audit","description":"EXTRACTED/INFERRED/AMBIGUOUS edge breakdown","mimeType":"text/plain"},
 {"uri":"graphicle-graph://questions","name":"Suggested Questions","description":"Suggested questions for this codebase","mimeType":"text/plain"}])
}
/// Which commit the graph was built from and whether HEAD has moved on
/// (Graphify 0.9.80, #3354): an MCP client cannot stat graph.json, so this is
/// its only staleness signal. HEAD is read from the repository holding the
/// graph; without git or a repository only the commit is named. Empty when the
/// graph records no commit.
fn build_commit_line(g: &Graph, path: Option<&Path>) -> String {
    let Some(commit) = g
        .attributes
        .get("_built_at_commit")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return String::new();
    };
    let dir = path
        .and_then(Path::parent)
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let short = |s: &str| s.chars().take(7).collect::<String>();
    match crate::export::git_head(Some(dir)).filter(|h| !h.is_empty()) {
        None => format!("Built at commit: {commit}\n"),
        Some(head) if head == commit => format!("Built at commit: {commit} (matches HEAD)\n"),
        Some(head) => format!(
            "Built at commit: {commit}\nHEAD is {}, graph built at {}: graph may be stale\n",
            short(&head),
            short(commit)
        ),
    }
}
/// The `graph_stats` tool and the stats resource, or with `audit` the audit
/// resource; `path` is the graph's file, whose repository the stats compare
/// the build commit with.
pub fn graph_stats(ctx: &GraphContext, audit: bool, path: Option<&Path>) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for (_, _, e) in ctx.graph.iter_edges() {
        *counts
            .entry(
                e.attributes
                    .get("confidence")
                    .map(string)
                    .unwrap_or("EXTRACTED".into()),
            )
            .or_default() += 1;
    }
    let total = ctx.graph.edge_count().max(1);
    let mut out = if audit {
        format!("Total edges: {total}\n")
    } else {
        format!(
            "Nodes: {}\nEdges: {}\nCommunities: {}\n",
            ctx.graph.nodes.len(),
            ctx.graph.edge_count(),
            ctx.communities.len()
        )
    };
    for key in ["EXTRACTED", "INFERRED", "AMBIGUOUS"] {
        let count = counts.get(key).copied().unwrap_or(0);
        let pct = (count as f64 / total as f64 * 100.0).round_ties_even() as i64;
        out.push_str(&if audit {
            format!("{key}: {count} ({pct}%)\n")
        } else {
            format!("{key}: {pct}%\n")
        });
    }
    if !audit {
        out.push_str(&build_commit_line(&ctx.graph, path));
    }
    out
}
pub fn call_graph_tool(
    ctx: &GraphContext,
    name: &str,
    args: &Value,
    path: Option<&Path>,
) -> Result<String> {
    let g = &ctx.graph;
    let required = |key: &str| {
        args[key]
            .as_str()
            .ok_or_else(|| format!("missing required string: {key}"))
    };
    let budget = args["token_budget"].as_i64().unwrap_or(2000);
    let node_query = || {
        ["label", "node_id", "id"]
            .into_iter()
            .map(|key| &args[key])
            .find(|value| crate::build::truthy(value))
            .map(string)
            .unwrap_or_default()
    };
    match name {
        "query_graph" => {
            let q = required("question")?;
            let mode = args["mode"].as_str().unwrap_or("bfs");
            let depth = args["depth"].as_i64().unwrap_or(3).min(6);
            let filters = args["context_filter"]
                .as_array()
                .into_iter()
                .flatten()
                .map(string)
                .collect::<Vec<_>>();
            let start = Instant::now();
            let output = query_text(
                g,
                &ctx.index,
                q,
                crate::serve_text::QueryOptions {
                    mode,
                    depth,
                    budget,
                    filters: &filters,
                    path: path.and_then(Path::to_str),
                    seeds: None,
                    seed_root: None,
                },
            )?;
            crate::querylog::log_query(
                &json!({"kind":"mcp_query","question":q,"corpus":path,"result":output,"duration_ms":start.elapsed().as_secs_f64()*1000.0,"extra":{"mode":mode,"depth":depth,"token_budget":budget}}),
            );
            Ok(output)
        }
        "get_node" => {
            let query = node_query().to_lowercase();
            if query.is_empty() {
                return Ok(
                    "Provide a node label or id (accepted keys: label, node_id, id).".into(),
                );
            }
            let i = match serve_find::resolve(g, &ctx.index, &query) {
                Ok(i) => i,
                Err(e) => return Ok(e),
            };
            let get = |k| g.nodes[i].attributes.get(k).map(string).unwrap_or_default();
            let mut lines = vec![
                format!("Node: {}", label_text(g, i)),
                format!("  ID: {}", sanitize(&id(g, i))),
                format!(
                    "  Source: {} {}",
                    sanitize(&get("source_file")),
                    sanitize(&get("source_location"))
                ),
            ];
            if crate::build::truthy(field(g, i, "definition_file")) {
                lines.push(format!(
                    "  Defined in: {} {}",
                    sanitize(&get("definition_file")),
                    sanitize(&get("definition_location"))
                ));
            }
            let community = if crate::build::truthy(field(g, i, "community_name")) {
                get("community_name")
            } else {
                get("community")
            };
            lines.extend([
                format!("  Type: {}", sanitize(&get("file_type"))),
                format!("  Community: {}", sanitize(&community)),
                format!("  Degree: {}", degrees(g)[i]),
            ]);
            let attrs = field(g, i, "attributes");
            if attrs.as_object().is_some_and(|m| !m.is_empty()) {
                let mut raw = json_text(&crate::extract_normalize::canonical(attrs), None, true);
                if raw.chars().count() > 1000 {
                    raw = format!("{}...", cut(&raw, 997));
                }
                lines.push(format!(
                    "  Attributes: {}",
                    raw.chars()
                        .filter(|c| *c > '\x1f' && *c != '\x7f')
                        .collect::<String>()
                ));
            }
            Ok(lines.join("\n"))
        }
        "get_neighbors" => {
            let query = node_query().to_lowercase();
            if query.is_empty() {
                return Ok(
                    "Provide a node label or id (accepted keys: label, node_id, id).".into(),
                );
            }
            let i = match serve_find::resolve(g, &ctx.index, &query) {
                Ok(i) => i,
                Err(e) => return Ok(e),
            };
            let filter = args["relation_filter"]
                .as_str()
                .unwrap_or("")
                .to_lowercase();
            let mut lines = vec![format!("Neighbors of {}:", label_text(g, i))];
            for incoming in [false, true] {
                for nb in neighbors(g, i, incoming) {
                    let (a, b) = if incoming { (nb, i) } else { (i, nb) };
                    let Some(e) = g
                        .edges_between(&g.nodes[a].id, &g.nodes[b].id)
                        .first()
                        .copied()
                    else {
                        continue;
                    };
                    let get = |k| e.attributes.get(k).map(string).unwrap_or_default();
                    let relation = get("relation");
                    if !filter.is_empty() && !relation.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let loc = e
                        .attributes
                        .get("source_location")
                        .filter(|v| crate::build::truthy(v))
                        .map(string)
                        .unwrap_or_default();
                    let at = if loc.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " at={}:{}",
                            sanitize(&text(
                                e.attributes.get("source_file").unwrap_or(&Value::Null)
                            )),
                            sanitize(&loc)
                        )
                    };
                    lines.push(format!(
                        "  {} {} [{}] [{}]{at}",
                        if incoming { "<--" } else { "-->" },
                        label_text(g, nb),
                        sanitize(&relation),
                        sanitize(&get("confidence"))
                    ));
                }
            }
            Ok(cut_lines(
                &lines,
                budget,
                "Narrow with relation_filter or use get_node for a specific symbol",
            ))
        }
        "get_community" => {
            let cid = args["community_id"]
                .as_i64()
                .ok_or("community_id is required")?;
            let Some((_, ns)) = ctx
                .communities
                .iter()
                .find(|(id, _)| *id == cid)
                .filter(|(_, ns)| !ns.is_empty())
            else {
                return Ok(format!("Community {cid} not found."));
            };
            let mut lines = vec![format!(
                "{} ({} nodes):",
                community_header(cid, g.nodes[ns[0]].attributes.get("community_name")),
                ns.len()
            )];
            lines.extend(ns.iter().map(|i| {
                format!(
                    "  {} [{}]",
                    label_text(g, *i),
                    sanitize(&text(field(g, *i, "source_file")))
                )
            }));
            Ok(cut_lines(
                &lines,
                budget,
                "Raise token_budget or use get_node for specific members",
            ))
        }
        "god_nodes" => {
            let ns = crate::analyze::god_nodes(
                g,
                args["top_n"].as_i64().unwrap_or(10),
                args["exclude_hubs_percentile"].as_f64(),
            )?;
            let mut lines = vec!["God nodes (most connected):".into()];
            lines.extend(ns.iter().enumerate().map(|(i, n)| {
                format!(
                    "  {}. {} - {} edges",
                    i + 1,
                    text(&n["label"]),
                    string(&n["degree"])
                )
            }));
            Ok(lines.join("\n"))
        }
        "graph_stats" => Ok(graph_stats(ctx, false, path)),
        "shortest_path" => serve_find::shortest_path(g, &ctx.index, args),
        "list_prs" | "get_pr_impact" | "triage_prs" => {
            crate::serve_prs::call(g, name, args, &crate::prs::NativeCommands::default())
        }
        _ => Ok(format!("Unknown tool: {name}")),
    }
}
pub struct Server {
    pub default_graph: PathBuf,
    pub cache: GraphContextCache,
    /// `serve --watch`: the default graph is served from memory and kept current.
    pub live: Option<Arc<crate::serve_live::LiveGraph>>,
}
impl Server {
    pub fn new(path: &Path) -> Self {
        let default_graph = crate::extractors::resolution::resolved(path);
        let cache = GraphContextCache::new(max_contexts());
        let _ = cache.load(&default_graph, true);
        Self {
            default_graph,
            cache,
            live: None,
        }
    }
    /// A server whose default graph is the resident, self-updating `live` graph.
    pub fn with_live(live: Arc<crate::serve_live::LiveGraph>) -> Self {
        Self {
            default_graph: live.graph_path().to_path_buf(),
            cache: GraphContextCache::new(max_contexts()),
            live: Some(live),
        }
    }
    pub fn select(&self, project: Option<&str>) -> Result<(PathBuf, Arc<GraphContext>)> {
        let path = project
            .filter(|s| !s.is_empty())
            .map(|p| {
                Path::new(p)
                    .join(paths::OUT_DIR.as_str())
                    .join("graph.json")
            })
            .unwrap_or_else(|| self.default_graph.clone());
        let path = crate::extractors::resolution::resolved(&path);
        if let Some(live) = self.live.as_ref().filter(|_| path == self.default_graph) {
            return Ok((path, live.context()));
        }
        let ctx = self.cache.load(&path, path == self.default_graph)?;
        Ok((path, ctx))
    }
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        let (path, ctx) = self.select(args["project_path"].as_str())?;
        if name == "retrieve_context" && crate::retrieve::enabled() {
            return crate::retrieve::call(&ctx.graph, &ctx.index, args, Some(&path));
        }
        call_graph_tool(&ctx, name, args, Some(&path))
    }
    pub fn resource(&self, uri: &str) -> Result<String> {
        let (path, ctx) = self.select(None)?;
        let g = &ctx.graph;
        let comms = ctx
            .communities
            .iter()
            .map(|(cid, ns)| (*cid, ns.iter().map(|i| g.nodes[*i].id.clone()).collect()))
            .collect::<Vec<_>>();
        match uri {
            "graphicle-graph://report" => Ok(fs::read_to_string(
                path.parent().unwrap().join("GRAPH_REPORT.md"),
            )
            .unwrap_or_else(|_| {
                format!(
                    "GRAPH_REPORT.md not found. Run {} extract first.",
                    crate::cli::program()
                )
            })),
            "graphicle-graph://stats" => Ok(graph_stats(&ctx, false, Some(&path))),
            "graphicle-graph://audit" => Ok(graph_stats(&ctx, true, Some(&path))),
            "graphicle-graph://god-nodes" => {
                call_graph_tool(&ctx, "god_nodes", &json!({"top_n":10}), Some(&path))
            }
            "graphicle-graph://surprises" => {
                let s = crate::analyze::surprising_connections(g, &comms, 10)?;
                if s.is_empty() {
                    return Ok("No surprising connections found.".into());
                }
                let mut lines = vec!["Surprising cross-community connections:".into()];
                lines.extend(s.iter().map(|s| {
                    format!(
                        "  {} <-> {} [{}]",
                        text(&s["source"]),
                        text(&s["target"]),
                        text(&s["relation"])
                    )
                }));
                Ok(lines.join("\n"))
            }
            "graphicle-graph://questions" => {
                let labels = fs::read(path.parent().unwrap().join(".graphicle_labels.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_else(|| {
                        ctx.communities
                            .iter()
                            .map(|(cid, _)| (cid.to_string(), json!(format!("Community {cid}"))))
                            .collect()
                    });
                let qs = crate::analyze::suggest_questions(g, &comms, &labels, 10)?;
                if qs.is_empty() {
                    return Ok("No suggested questions available.".into());
                }
                let mut lines = vec!["Suggested questions:".into()];
                lines.extend(qs.iter().map(|q| {
                    format!(
                        "  - {}",
                        if q.is_object() {
                            text(&q["question"])
                        } else {
                            string(q)
                        }
                    )
                }));
                Ok(lines.join("\n"))
            }
            _ => Err(format!("Unknown resource: {uri}").into()),
        }
    }
    pub fn rpc(&self, r: &Value) -> Option<Value> {
        if !r.is_object()
            || r["jsonrpc"] != "2.0"
            || !r["method"].is_string()
            || r.get("id")
                .is_some_and(|id| !(id.is_string() || id.is_number() || id.is_null()))
            || r.get("params").is_some_and(|params| !params.is_object())
        {
            return Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}),
            );
        }
        let id = r.get("id")?.clone();
        let method = r["method"].as_str().unwrap_or("");
        let p = &r["params"];
        let result:Result<Value>=match method{
            "initialize"=>Ok(json!({"protocolVersion":protocol_version(&p["protocolVersion"]),"capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"graphicle-graph","version":crate::VERSION}})),"ping"=>Ok(json!({})),"tools/list"=>Ok(json!({"tools":tools_list()})),"resources/list"=>Ok(json!({"resources":resources_list()})),"resources/templates/list"=>Ok(json!({"resourceTemplates":[]})),
            "tools/call"=>{let name=p["name"].as_str().unwrap_or("");match self.call_tool(name,&p["arguments"]){Ok(t)=>Ok(json!({"content":[{"type":"text","text":t}]})),Err(e)=>Ok(json!({"isError":(["list_prs","get_pr_impact","triage_prs"].contains(&name)),"content":[{"type":"text","text":format!("Error executing {name}: {e}")}]}))}},
            "resources/read"=>self.resource(p["uri"].as_str().unwrap_or("")).map(|s|json!({"contents":[{"uri":p["uri"],"mimeType":if p["uri"].as_str().is_some_and(|s|s.starts_with("graphicle-graph://report")){"text/markdown"}else{"text/plain"},"text":s}]})),
            _=>return Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Method not found: {method}")}}))
        };
        Some(match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":e}}),
        })
    }
}
/// MCP protocol versions this server speaks.
const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The protocol version an `initialize` reply names: the client's when this
/// server speaks it, else this server's own, as the MCP lifecycle asks (and
/// as Graphify's server, on the MCP Python SDK, answers). A version the
/// server does not know is never echoed back as agreed.
fn protocol_version(requested: &Value) -> &'static str {
    requested
        .as_str()
        .and_then(|asked| PROTOCOL_VERSIONS.iter().find(|known| **known == asked))
        .copied()
        .unwrap_or("2025-06-18")
}
pub fn stdio(path: &Path) -> Result<()> {
    serve_stdio(&Server::new(path))
}
/// MCP over stdio for an already constructed server (`serve --watch`).
pub fn serve_stdio(server: &Server) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    stdio_with_io(
        server,
        &mut stdin.lock(),
        &mut out,
        crate::protocol::MAX_MESSAGE_BYTES,
    )
}
pub(crate) fn stdio_with_io(
    server: &Server,
    input: &mut impl BufRead,
    out: &mut impl Write,
    limit: usize,
) -> Result<()> {
    while let Some(frame) = crate::protocol::read_frame(input, limit)? {
        let reply = match frame {
            Ok(line) if std::str::from_utf8(&line).is_ok_and(|s| s.trim().is_empty()) => continue,
            Ok(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(v) => server.rpc(&v),
                Err(e) => Some(
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}}),
                ),
            },
            Err(error) => {
                Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":error}}))
            }
        };
        if let Some(reply) = reply {
            let bytes = crate::protocol::encode_json(&reply).or_else(|error| {
                crate::protocol::encode_json(
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":error}}),
                )
            })?;
            if let Err(error) = out
                .write_all(&bytes)
                .and_then(|()| out.write_all(b"\n"))
                .and_then(|()| out.flush())
            {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
    }
    Ok(())
}
