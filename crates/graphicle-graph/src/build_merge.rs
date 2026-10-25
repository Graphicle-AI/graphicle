// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py
//! Incremental replacement, deletion pruning and loss accounting for graph builds.
use crate::{
    Result,
    build::{self, BuildOptions, Diagnostics},
    build_normalize::*,
    dedup::TiebreakClient,
    fsutil,
    graph::Graph,
    paths,
    python_compat::{self, scalar_key},
    security,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize)]
pub struct LoadedGraph {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub hyperedges: Vec<Value>,
    pub directed: bool,
}
pub fn load_existing_graph(path: &Path) -> Result<Option<LoadedGraph>> {
    if !path.exists() {
        return Ok(None);
    }
    security::check_graph_file_size_cap_with(path, security::max_graph_file_bytes())?;
    let raw = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "Cannot read {} for incremental merge: {e}. Delete the file and run a full rebuild.",
            fsutil::path_string(path)
        )
    })?;
    let data: Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Cannot read {} for incremental merge: {e}. Delete the file and run a full rebuild.",
            fsutil::path_string(path)
        )
    })?;
    if !data.is_object() {
        return Err("Expected stored graph object".into());
    }
    let list = |k: &str| -> Result<Vec<Value>> {
        match data.get(k) {
            None => Ok(Vec::new()),
            Some(v) => Ok(v
                .as_array()
                .cloned()
                .ok_or_else(|| format!("Stored {k} must be an array"))?),
        }
    };
    let mut nodes = list("nodes")?;
    let mut edges = list(if data.get("links").is_some() {
        "links"
    } else {
        "edges"
    })?;
    for item in nodes.iter_mut().chain(edges.iter_mut()) {
        backfill_origin(item);
    }
    Ok(Some(LoadedGraph {
        nodes,
        edges,
        hyperedges: list("hyperedges")?,
        directed: truthy(&data["directed"]),
    }))
}
fn effective_root(path: &Path, root: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(root) = root {
        return fsutil::resolve_missing(root)
            .map(Some)
            .map_err(crate::Error::from);
    }
    let output = Path::new(&*paths::OUT_DIR)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    Ok(infer_merge_root(path, &output))
}
struct PruneState {
    root: Option<PathBuf>,
    set: BTreeMap<String, String>,
    absolute: BTreeMap<String, String>,
    matched: BTreeSet<String>,
    /// `abs_identity` of each source string under `root`: resolving it walks
    /// the filesystem, and every node and edge of a merge is looked up.
    identities: std::cell::RefCell<HashMap<String, Option<String>>>,
}
impl PruneState {
    fn new(
        sources: &[String],
        root: Option<PathBuf>,
        new_sources: &BTreeSet<String>,
    ) -> Result<Self> {
        let (set, absolute) = build_prune_sets(sources, root.as_deref(), new_sources)?;
        Ok(Self {
            root,
            set,
            absolute,
            matched: BTreeSet::new(),
            identities: Default::default(),
        })
    }
    fn live(&self) -> bool {
        !self.set.is_empty() || !self.absolute.is_empty()
    }
    fn lookup(&self, sf: &Value) -> Result<Option<String>> {
        if !truthy(sf) {
            return Ok(None);
        }
        let sf = sf.as_str().ok_or("Expected source_file string")?;
        if let Some(hit) = self.set.get(sf) {
            return Ok(Some(hit.clone()));
        }
        let norm = norm_path(sf, self.root.as_deref());
        if let Some(hit) = self.set.get(&norm) {
            return Ok(Some(hit.clone()));
        }
        if self.absolute.is_empty() {
            return Ok(None);
        }
        let known = self.identities.borrow().get(sf).cloned();
        let identity = match known {
            Some(identity) => identity,
            None => {
                let identity = abs_identity(&json!(sf), self.root.as_deref())?;
                self.identities
                    .borrow_mut()
                    .insert(sf.to_owned(), identity.clone());
                identity
            }
        };
        Ok(identity.and_then(|a| self.absolute.get(&a).cloned()))
    }
    fn hits(&mut self, sf: &Value) -> Result<bool> {
        if let Some(hit) = self.lookup(sf)? {
            self.matched.insert(hit);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn infer(
        &mut self,
        sources: &[String],
        stored: &BTreeSet<String>,
        new_sources: &BTreeSet<String>,
    ) -> Result<()> {
        if !self.live() {
            return Ok(());
        }
        for sf in stored {
            if self.hits(&json!(sf))? {
                return Ok(());
            }
        }
        if let Some(derived) = derive_prune_root(sources, stored) {
            let derived = PathBuf::from(derived);
            if self.root.as_ref() != Some(&derived) {
                self.root = Some(derived);
                self.identities.get_mut().clear();
                (self.set, self.absolute) =
                    build_prune_sets(sources, self.root.as_deref(), new_sources)?;
            }
        }
        Ok(())
    }
}
fn source_owned(
    item: &Value,
    ast: &BTreeSet<String>,
    semantic: &BTreeSet<String>,
    root: Option<&Path>,
) -> bool {
    let own = if is_ast_tier(item) { ast } else { semantic };
    let Some(sf) = item["source_file"].as_str() else {
        return false;
    };
    own.contains(sf) || own.contains(&norm_path(sf, root))
}
fn stored_sources(sequences: &[&[Value]]) -> BTreeSet<String> {
    sequences
        .iter()
        .flat_map(|seq| seq.iter())
        .filter_map(|v| v["source_file"].as_str())
        .map(str::to_owned)
        .collect()
}
fn semantic_shrink(prior: &[Value], fresh: &[Value], root: Option<&Path>) -> Map<String, Value> {
    let counts = |nodes: &[Value]| {
        let mut out = BTreeMap::<String, usize>::new();
        for node in nodes {
            if node.is_object()
                && !is_ast_tier(node)
                && let Some(sf) = node["source_file"].as_str().filter(|s| !s.is_empty())
            {
                *out.entry(norm_path(sf, root)).or_default() += 1;
            }
        }
        out
    };
    let prior = counts(prior);
    let fresh = counts(fresh);
    fresh
        .into_iter()
        .filter_map(|(sf, n)| {
            let previous = prior.get(&sf).copied().unwrap_or(0);
            (previous > 1 && n < previous).then_some((sf, json!([previous, n])))
        })
        .collect()
}
pub fn merge_raw_extraction(
    new: &mut Value,
    path: &Path,
    prune_sources: &[String],
    root: Option<&Path>,
    ast_sources: Option<&[String]>,
) -> Result<()> {
    let Some(existing) = load_existing_graph(path)? else {
        return Ok(());
    };
    let root = effective_root(path, root)?;
    let (ast, sem) =
        tier_replacement_sources(std::slice::from_ref(new), root.as_deref(), ast_sources);
    let new_sources = ast.union(&sem).cloned().collect();
    let mut prune = PruneState::new(prune_sources, root.clone(), &new_sources)?;
    prune.infer(
        prune_sources,
        &stored_sources(&[&existing.nodes, &existing.edges, &existing.hyperedges]),
        &new_sources,
    )?;
    let shrink = if sem.is_empty() {
        Map::new()
    } else {
        semantic_shrink(
            &existing.nodes,
            new["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]),
            root.as_deref(),
        )
    };
    for (key, items) in [
        ("nodes", existing.nodes),
        ("edges", existing.edges),
        ("hyperedges", existing.hyperedges),
    ] {
        let mut kept = Vec::new();
        for item in items {
            if !item.is_object()
                || source_owned(&item, &ast, &sem, root.as_deref())
                || prune.lookup(&item["source_file"])?.is_some()
            {
                continue;
            }
            kept.push(item);
        }
        if key != "hyperedges" || !kept.is_empty() || truthy(&new["hyperedges"]) {
            if let Some(current) = new.get(key) {
                kept.extend(
                    current
                        .as_array()
                        .ok_or_else(|| format!("Expected {key} array"))?
                        .iter()
                        .cloned(),
                );
            }
            new[key] = json!(kept);
        }
    }
    if !shrink.is_empty() {
        new["_unverified_semantic_shrink"] = json!(shrink);
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeOptions {
    pub graph_path: Option<PathBuf>,
    pub prune_sources: Vec<String>,
    pub directed: Option<bool>,
    pub dedup: bool,
    pub dedup_llm_backend: Option<String>,
    pub root: Option<PathBuf>,
    pub ast_sources: Option<Vec<String>>,
}
impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            graph_path: None,
            prune_sources: Vec::new(),
            directed: None,
            dedup: true,
            dedup_llm_backend: None,
            root: None,
            ast_sources: None,
        }
    }
}
fn isolated(graph: &Graph, i: usize) -> bool {
    graph.out_edges(i).is_empty() && graph.in_edges(i).is_empty()
}
/// Ids of the source-less `nodes` that no edge of `edges` touches (#4399).
fn isolated_sourceless(nodes: &[Value], edges: &[Value]) -> BTreeSet<python_compat::ScalarKey> {
    let mut candidates: BTreeSet<_> = nodes
        .iter()
        .filter(|n| n.is_object() && !n.get("source_file").is_some_and(truthy))
        .filter_map(|n| scalar_key(n.get("id").unwrap_or(&Value::Null)))
        .collect();
    if candidates.is_empty() {
        return candidates;
    }
    for edge in edges.iter().filter(|e| e.is_object()) {
        for end in ["source", "target"] {
            if let Some(key) = scalar_key(edge.get(end).unwrap_or(&Value::Null)) {
                candidates.remove(&key);
            }
        }
    }
    candidates
}
/// Removes the source-less nodes a merge left without an edge (upstream
/// `_sweep_orphans`, #4247/#4399) and returns their ids. Spared: nodes already
/// isolated in the stored graph or emitted without an edge by the fresh chunks
/// (`spare`), and hyperedge members (referenced from the graph attributes, so
/// degree 0 does not mean they connect nothing). Only machine-minted
/// placeholders (code stubs and external import stubs) go when re-extraction
/// strands them; another source-less node goes only when this prune isolated
/// it (`pruned_before` holds the ones isolated before the prune).
fn sweep_orphans(
    graph: &mut Graph,
    spare: &BTreeSet<python_compat::ScalarKey>,
    pruned_before: Option<&BTreeSet<python_compat::ScalarKey>>,
) -> Result<Vec<Value>> {
    let he_members: BTreeSet<_> = graph
        .attributes
        .get("hyperedges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|he| he.is_object())
        .flat_map(|he| {
            he.get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(scalar_key)
        .collect();
    let orphaned = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| {
            let attrs = &n.attributes;
            if attrs.get("source_file").is_some_and(truthy) || !isolated(graph, *i) {
                return false;
            }
            let key = scalar_key(&n.id);
            if key
                .as_ref()
                .is_some_and(|k| spare.contains(k) || he_members.contains(k))
            {
                return false;
            }
            let external_stub = attrs.get("external") == Some(&Value::Bool(true))
                && attrs.get("type").and_then(Value::as_str) == Some("external");
            attrs.get("file_type").and_then(Value::as_str) == Some("code")
                || external_stub
                || pruned_before.is_some_and(|before| !key.is_some_and(|k| before.contains(&k)))
        })
        .map(|(_, n)| n.id.clone())
        .collect::<Vec<_>>();
    graph.remove_nodes_from(&orphaned)?;
    Ok(orphaned)
}
pub fn build_merge(
    chunks: &[Value],
    options: &MergeOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    build_merge_owned(chunks.to_vec(), options, messages, client)
}
/// [`build_merge`] over chunks the caller no longer needs: they move into the
/// build instead of being copied (as do the stored records it keeps). Output
/// is identical to [`build_merge`].
pub fn build_merge_owned(
    chunks: Vec<Value>,
    options: &MergeOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    let path = options
        .graph_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(paths::default_graph_json()));
    let existing = load_existing_graph(&path)?;
    let had_graph = existing.is_some();
    let existing = existing.unwrap_or(LoadedGraph {
        nodes: Vec::new(),
        edges: Vec::new(),
        hyperedges: Vec::new(),
        directed: false,
    });
    let directed = options.directed.unwrap_or(existing.directed);
    let root = effective_root(&path, options.root.as_deref())?;
    let (ast, sem) =
        tier_replacement_sources(&chunks, root.as_deref(), options.ast_sources.as_deref());
    let new_sources = ast.union(&sem).cloned().collect::<BTreeSet<_>>();
    let disk_nodes = existing.nodes;
    let disk_n = disk_nodes.len();
    // Only saved nodes a prune or the orphan sweep removes may excuse a
    // smaller graph (#3774).
    let disk_ids = disk_nodes
        .iter()
        .filter(|n| n.is_object() && !n["id"].is_null())
        .filter_map(|n| scalar_key(&n["id"]))
        .collect::<BTreeSet<_>>();
    // Source-less nodes already isolated in the stored graph (#4399), taken
    // before the re-extracted sources' old edges go, so a placeholder this run
    // strands is not mistaken for a pre-existing isolate; and those the fresh
    // chunks emit without an edge, which a from-scratch build would keep.
    let mut spare_isolated = isolated_sourceless(&disk_nodes, &existing.edges);
    for chunk in &chunks {
        spare_isolated.extend(isolated_sourceless(
            chunk["nodes"].as_array().map_or(&[][..], Vec::as_slice),
            chunk["edges"].as_array().map_or(&[][..], Vec::as_slice),
        ));
    }
    let fresh_nodes = || {
        chunks
            .iter()
            .flat_map(|c| c["nodes"].as_array().into_iter().flatten())
    };
    let shrink = if had_graph && !sem.is_empty() {
        semantic_shrink(
            &disk_nodes,
            &fresh_nodes().cloned().collect::<Vec<_>>(),
            root.as_deref(),
        )
    } else {
        Map::new()
    };
    // The stored nodes are read again only by the no-dedup drop check below
    // (and their sources by the prune inference); otherwise they move.
    let disk_sources = stored_sources(&[&disk_nodes]);
    let disk_copy = (had_graph && !options.dedup).then(|| disk_nodes.clone());
    let mut existing_nodes = disk_nodes;
    let mut existing_edges = existing.edges;
    if !new_sources.is_empty() {
        existing_nodes.retain(|n| !source_owned(n, &ast, &sem, root.as_deref()));
        existing_edges.retain(|e| !source_owned(e, &ast, &sem, root.as_deref()));
        let replaced = disk_n - existing_nodes.len();
        if replaced > 0 {
            messages.stderr.push(format!(
                "[graphicle] Replaced {replaced} node(s) from re-extracted source file(s)."
            ));
        }
    }
    let mut prune = PruneState::new(&options.prune_sources, root.clone(), &new_sources)?;
    // `stored_sources` of the stored nodes, the kept edges and the hyperedges.
    let mut stored = disk_sources;
    stored.extend(stored_sources(&[&existing_edges, &existing.hyperedges]));
    prune.infer(&options.prune_sources, &stored, &new_sources)?;
    // Keyed by (id, source_file): ids are chosen per extraction, so two files
    // can emit the same one; re-extracting one must not drop the other's (#3981).
    let he_file = |he: &Value| {
        Some(norm_path(
            he["source_file"].as_str().unwrap_or(""),
            root.as_deref(),
        ))
        .filter(|s| !s.is_empty())
    };
    let new_he_keys: BTreeSet<_> = chunks
        .iter()
        .flat_map(|c| c["hyperedges"].as_array().into_iter().flatten())
        .filter(|he| he.is_object() && truthy(&he["id"]))
        .filter_map(|he| Some((scalar_key(&he["id"])?, he_file(he))))
        .collect();
    let mut carried = Vec::new();
    for he in existing.hyperedges {
        if !he.is_object() {
            continue;
        }
        let sf = he["source_file"].as_str().unwrap_or("");
        if sem.contains(sf)
            || sem.contains(&norm_path(sf, root.as_deref()))
            || prune.hits(&he["source_file"])?
        {
            continue;
        }
        if truthy(&he["id"])
            && scalar_key(&he["id"]).is_some_and(|id| new_he_keys.contains(&(id, he_file(&he))))
        {
            continue;
        }
        carried.push(he);
    }
    if !options.prune_sources.is_empty() {
        let fresh_ids: BTreeSet<_> = fresh_nodes().filter_map(|n| scalar_key(&n["id"])).collect();
        let mut kept = Vec::new();
        for n in existing_nodes {
            if scalar_key(&n["id"]).is_some_and(|id| fresh_ids.contains(&id))
                && prune.hits(&n["source_file"])?
            {
                continue;
            }
            kept.push(n);
        }
        existing_nodes = kept;
    }
    let protected = if had_graph {
        existing_nodes
            .iter()
            .filter_map(|n| n["id"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut all_chunks = Vec::new();
    if had_graph {
        all_chunks
            .push(json!({"nodes":existing_nodes,"edges":existing_edges,"hyperedges":carried}));
    }
    all_chunks.extend(chunks);
    let mut graph = build::build_owned(
        all_chunks,
        &BuildOptions {
            directed,
            dedup: options.dedup,
            dedup_llm_backend: options.dedup_llm_backend.clone(),
            root: root.clone(),
            protected_ids: protected,
            multigraph: false,
        },
        messages,
        client,
    )?;
    if !options.prune_sources.is_empty() {
        let isolated_before: BTreeSet<_> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| {
                !n.attributes.get("source_file").is_some_and(truthy) && isolated(&graph, *i)
            })
            .filter_map(|(_, n)| scalar_key(&n.id))
            .collect();
        let mut remove = Vec::new();
        for node in &graph.nodes {
            if prune.hits(node.attributes.get("source_file").unwrap_or(&Value::Null))? {
                remove.push(node.id.clone());
            }
        }
        let mut n_nodes = remove.len();
        graph.remove_nodes_from(&remove)?;
        let mut edges = Vec::new();
        for (u, v, e) in graph.iter_edges() {
            if prune.hits(e.attributes.get("source_file").unwrap_or(&Value::Null))? {
                edges.push((graph.nodes[u].id.clone(), graph.nodes[v].id.clone(), None));
            }
        }
        graph.remove_edges_from(&edges);
        let orphaned = sweep_orphans(&mut graph, &spare_isolated, Some(&isolated_before))?;
        n_nodes += orphaned.len();
        // A stub minted in this run and then orphaned was never in graph.json,
        // so it cannot excuse a dedup shrink; the message below still counts it.
        let saved = remove
            .iter()
            .chain(&orphaned)
            .filter(|id| scalar_key(id).is_some_and(|k| disk_ids.contains(&k)))
            .count();
        graph.attributes[build::PRUNED_NODE_COUNT] = json!(saved);
        let n_files = prune.matched.len();
        if n_nodes > 0 {
            messages.stderr.push(format!("[graphicle] Pruned {n_nodes} node(s) from {n_files} deleted or excluded source file(s)."));
        }
        if !edges.is_empty() {
            messages.stderr.push(format!(
                "[graphicle] Pruned {} edge(s) from deleted or excluded source file(s).",
                edges.len()
            ));
        }
        if n_nodes == 0 && edges.is_empty() {
            if prune.live() && prune.matched.is_empty() {
                let sample = options
                    .prune_sources
                    .iter()
                    .find(|p| !p.is_empty())
                    .map(|p| json!(norm_path(p, prune.root.as_deref())))
                    .unwrap_or(Value::Null);
                let stored = graph
                    .nodes
                    .iter()
                    .filter_map(|n| n.attributes.get("source_file").filter(|v| truthy(v)))
                    .map(python_compat::string)
                    .min()
                    .map_or(Value::Null, |s| json!(s));
                messages.stderr.push(format!("[graphicle] WARNING: {} prune source(s) matched no nodes or edges — nothing was removed. Prune entry {} does not correspond to any stored source_file (e.g. {}). If these files should have been pruned, pass root= to build_merge so absolute paths relativize to the graph's source_file keys. (#2446)",options.prune_sources.len(),python_compat::repr(&sample),python_compat::repr(&stored)));
            } else {
                messages.stderr.push(format!("[graphicle] {} source file(s) deleted or excluded since last run — no matching nodes or edges in graph, already clean.",options.prune_sources.len()));
            }
        }
    } else {
        // Re-extraction strands placeholders too, e.g. per-file stubs
        // re-pointed at a shared node (#4247) or a type whose last reference
        // was edited away (#4399).
        let swept = sweep_orphans(&mut graph, &spare_isolated, None)?;
        if !swept.is_empty() {
            // The same #3774 excuse as the prune path: only saved nodes count.
            let saved = swept
                .iter()
                .filter(|id| scalar_key(id).is_some_and(|k| disk_ids.contains(&k)))
                .count();
            graph.attributes[build::PRUNED_NODE_COUNT] = json!(saved);
            messages.stderr.push(format!(
                "[graphicle] Removed {} source-less node(s) left with no edges by re-extraction.",
                swept.len()
            ));
        }
    }
    if let Some(disk_nodes) = &disk_copy {
        let mut unexplained = Vec::new();
        for node in disk_nodes {
            if !node.is_object()
                || node["id"].is_null()
                || scalar_key(&node["id"]).is_none()
                || graph.node_index(&node["id"]).is_some()
            {
                continue;
            }
            let twin = node["id"]
                .as_str()
                .filter(|id| !id.ends_with("_doc") && node["file_type"] == "document")
                .and_then(|id| graph.node_index(&json!(format!("{id}_doc"))))
                .is_some_and(|i| {
                    graph.nodes[i].attributes.get("source_file") == node.get("source_file")
                });
            if twin {
                continue;
            }
            if !truthy(&node["source_file"])
                || source_owned(node, &ast, &sem, root.as_deref())
                || prune.hits(&node["source_file"])?
            {
                continue;
            }
            unexplained.push(node);
        }
        if !unexplained.is_empty() {
            return Err(format!(
                "graphicle: build_merge would drop {} node(s) from sources that were neither re-extracted nor pruned this run (e.g. {}); graph would go {disk_n} → {} nodes. Pass prune_sources explicitly if you intend to remove them. (#479)",
                unexplained.len(),
                python_compat::repr(&unexplained[0]["id"]),
                graph.nodes.len()
            ).into());
        }
    }
    if !shrink.is_empty() {
        graph.attributes["_unverified_semantic_shrink"] = json!(shrink);
    }
    Ok(graph)
}
