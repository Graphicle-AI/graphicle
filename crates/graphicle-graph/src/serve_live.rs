// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Opt-in `serve --watch`: the MCP server keeps its graph resident and applies
//! file changes in memory within a fraction of a second, then runs the normal
//! incremental rebuild in the background so `.graphicle/` (graph.json, the
//! report, communities) catches up. Once changes settle, the served graph is
//! the one loaded from that rebuilt graph.json, so the in-memory patch is only
//! ever a short-lived preview of the rebuild's result.
use crate::{
    Result, build,
    detect::{EncodingWarnings, IgnoreState},
    extract,
    graph::Graph,
    serve::{GraphContext, load_graph},
    watch, watch_paths,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Quiet time after the last event before a batch is applied in memory.
const DEBOUNCE: Duration = Duration::from_millis(10);
/// Quiet time before the background incremental rebuild of `.graphicle/`.
const SETTLE: Duration = Duration::from_millis(1200);

#[derive(Default, Debug, Clone)]
pub struct LiveStats {
    pub fast_patches: usize,
    pub exact_rebuilds: usize,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Pending {
    /// Changes not yet covered by a finished background rebuild.
    exact: BTreeSet<PathBuf>,
    /// Changes seen while a background rebuild was running.
    since_start: BTreeSet<PathBuf>,
    running: bool,
    last_change: Option<Instant>,
}

pub struct LiveGraph {
    root: PathBuf,
    project_root: PathBuf,
    out: PathBuf,
    graph_path: PathBuf,
    current: RwLock<Arc<GraphContext>>,
    pending: Mutex<Pending>,
    publish: Mutex<()>,
    ignore: Mutex<IgnoreState>,
    stats: Mutex<LiveStats>,
    /// The resolution context of the served graph (keyed by its address).
    context_cache: Mutex<Option<(usize, ContextCache)>>,
    /// Keep that context between updates (`GRAPHICLE_LIVE_CONTEXT_CACHE=0`
    /// rebuilds it for every update).
    reuse_context: AtomicBool,
}

fn stored_forms(path: &Path, project_root: &Path) -> Vec<String> {
    let mut forms = vec![path.to_string_lossy().into_owned()];
    if let Ok(rel) = path.strip_prefix(project_root) {
        forms.push(rel.to_string_lossy().replace('\\', "/"));
    }
    forms
}

fn attr<'a>(g: &'a Graph, i: usize, key: &str) -> &'a Value {
    g.nodes[i].attributes.get(key).unwrap_or(&Value::Null)
}

/// `build::is_ast_tier` over a node or edge attribute record.
fn ast_tier(attrs: &crate::record::Attributes) -> bool {
    let mut probe = Map::new();
    for key in ["_origin", "source_location"] {
        if let Some(v) = attrs.get(key) {
            probe.insert(key.into(), v.clone());
        }
    }
    build::is_ast_tier(&Value::Object(probe))
}

/// Resolution context from the untouched part of the resident graph: the same
/// AST-tier node and edge fields the incremental rebuild passes to `extract`.
fn context(g: &Graph, touched: &BTreeSet<String>) -> (Vec<Value>, Vec<Value>) {
    context_where(g, |s| !touched.contains(s))
}

/// The resolution context records of every file `keep` accepts.
fn context_where(g: &Graph, keep: impl Fn(&str) -> bool) -> (Vec<Value>, Vec<Value>) {
    let live = |source: &Value| source.as_str().is_some_and(&keep);
    let mut nodes = vec![];
    for (i, node) in g.nodes.iter().enumerate() {
        if node.id.as_str().is_none_or(str::is_empty)
            || !ast_tier(&node.attributes)
            || !live(attr(g, i, "source_file"))
        {
            continue;
        }
        let mut c = json!({"id": node.id});
        for k in ["label", "source_file", "file_type", "type"] {
            c[k] = attr(g, i, k).clone();
        }
        // `--routes`: requests in re-extracted files reach resident routes.
        if attr(g, i, "node_kind") == "route" {
            for k in ["node_kind", "route_method", "route_path"] {
                c[k] = attr(g, i, k).clone();
            }
        }
        for k in [
            "_callable",
            "_callable_class",
            "_elixir_module",
            "_rust_impl_key",
            "_rust_declaration_count",
            "_ts_public_twin",
            "_py_property",
            "_java_ret",
        ] {
            if build::truthy(attr(g, i, k)) {
                c[k] = attr(g, i, k).clone();
            }
        }
        let mut meta = Map::new();
        if let Some(m) = attr(g, i, "metadata").as_object() {
            for k in [
                "ruby_resolution_schema",
                "ruby_method_kind",
                "ruby_lookup_unsafe",
                "python_opaque_bases",
                "ruby_reopened",
                "ruby_external_method_owners",
                "language",
                "kind",
                "module",
                "name",
                "arity",
            ] {
                if let Some(v) = m.get(k) {
                    meta.insert(k.into(), v.clone());
                }
            }
        }
        if !meta.is_empty() {
            c["metadata"] = Value::Object(meta);
        }
        nodes.push(c);
    }
    let mut edges = vec![];
    for (u, v, e) in g.iter_edges() {
        let relation = e.attributes.get("relation").and_then(Value::as_str);
        if !relation.is_some_and(|r| ["contains", "method", "inherits"].contains(&r))
            || !ast_tier(&e.attributes)
            || !live(e.attributes.get("source_file").unwrap_or(&Value::Null))
        {
            continue;
        }
        let mut c = json!({"source": g.nodes[u].id, "target": g.nodes[v].id, "relation": relation,
            "source_file": e.attributes.get("source_file").cloned().unwrap_or(Value::Null)});
        if let Some(m) = e.attributes.get("metadata").and_then(Value::as_object)
            && let Some(sup) = m.get("ruby_superclass_ref").filter(|v| v.is_string())
        {
            c["metadata"] = json!({"ruby_superclass_ref": sup});
            if let Some(scopes) = m
                .get("ruby_lexical_scopes")
                .filter(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
            {
                c["metadata"]["ruby_lexical_scopes"] = scopes.clone();
            }
        }
        edges.push(c);
    }
    (nodes, edges)
}

/// The resident graph with `changed` files re-extracted in memory, or `None`
/// when none of the paths affects the code graph. Nodes of changed files are
/// replaced; edges that unchanged files point into them are kept when their
/// endpoint ids survive, as the incremental rebuild's reconciliation does.
/// A patched graph plus what an index needs to follow it: survivors keep
/// their order (`remap[old]` is their new position) and new nodes are appended
/// from `first_new` on. `merged` marks a new record merged into a survivor.
pub struct Patched {
    pub graph: Graph,
    pub remap: Vec<Option<usize>>,
    pub first_new: usize,
    pub merged: bool,
}

/// The resolution context of a whole resident graph (every file), kept
/// between live updates: an update filters out the files it re-extracts
/// instead of rebuilding the records from the graph, then swaps in the
/// records of those files from the patched graph.
#[derive(Default)]
pub struct ContextCache {
    nodes: Vec<Value>,
    edges: Vec<Value>,
}
impl ContextCache {
    pub fn of(g: &Graph) -> Self {
        let (nodes, edges) = context_where(g, |_| true);
        Self { nodes, edges }
    }
    fn without(&self, touched: &BTreeSet<String>) -> (Vec<Value>, Vec<Value>) {
        let keep = |v: &&Value| {
            v.get("source_file")
                .and_then(Value::as_str)
                .is_some_and(|s| !touched.contains(s))
        };
        (
            self.nodes.iter().filter(keep).cloned().collect(),
            self.edges.iter().filter(keep).cloned().collect(),
        )
    }
    /// The context of `patched`, given that only `touched` files changed.
    /// Edges of unchanged files into nodes that are gone go too.
    fn after(mut self, patched: &Graph, touched: &BTreeSet<String>) -> Self {
        let keep = |v: &Value| {
            v.get("source_file")
                .and_then(Value::as_str)
                .is_some_and(|s| !touched.contains(s))
        };
        self.nodes.retain(keep);
        let ids = patched
            .nodes
            .iter()
            .filter_map(|n| n.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let exists = |v: &Value, k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .is_some_and(|id| ids.contains(id))
        };
        self.edges
            .retain(|e| keep(e) && exists(e, "source") && exists(e, "target"));
        let (nodes, edges) = context_where(patched, |s| touched.contains(s));
        self.nodes.extend(nodes);
        self.edges.extend(edges);
        self
    }
}

pub fn patch(
    g: &Graph,
    watch_root: &Path,
    project_root: &Path,
    changed: &BTreeSet<PathBuf>,
) -> Result<Option<Patched>> {
    patch_with(g, watch_root, project_root, changed, None).map(|p| p.map(|(p, _)| p))
}

/// [`patch`], reading the resolution context from `cache` (built for `g`)
/// and returning the files it re-extracted, so the cache can follow.
pub fn patch_with(
    g: &Graph,
    watch_root: &Path,
    project_root: &Path,
    changed: &BTreeSet<PathBuf>,
    cache: Option<&ContextCache>,
) -> Result<Option<(Patched, BTreeSet<String>)>> {
    let mut touched = BTreeSet::new();
    let mut targets = vec![];
    for path in changed {
        let forms = stored_forms(path, project_root);
        let semantic = g.nodes.iter().enumerate().any(|(i, n)| {
            !ast_tier(&n.attributes)
                && attr(g, i, "source_file")
                    .as_str()
                    .is_some_and(|s| forms.iter().any(|f| f == s))
        });
        if path.is_file() && extract::get_extractor(path).is_some() && !semantic {
            targets.push(path.clone());
        }
        touched.extend(forms);
    }
    let removed = g
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| {
            ast_tier(&n.attributes)
                && attr(g, *i, "source_file")
                    .as_str()
                    .is_some_and(|s| touched.contains(s))
        })
        .map(|(i, _)| i)
        .collect::<BTreeSet<_>>();
    if removed.is_empty() && targets.is_empty() {
        return Ok(None);
    }
    let timing = std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1");
    let mut last = Instant::now();
    let mut mark = |phase: &str| {
        if timing {
            eprintln!(
                "[live timing] {phase}={:.1}ms",
                last.elapsed().as_secs_f64() * 1000.0
            );
            last = Instant::now();
        }
    };
    mark("select");
    let fresh = if targets.is_empty() {
        Graph::new(g.directed, g.multigraph)
    } else {
        let (context_nodes, context_edges) = match cache {
            Some(cache) => cache.without(&touched),
            None => context(g, &touched),
        };
        mark("context");
        let mut extraction = extract::extract(
            &targets,
            &extract::ExtractOptions {
                cache_root: Some(watch_root.to_path_buf()),
                resolution_context_nodes: context_nodes,
                resolution_context_edges: context_edges,
                ..Default::default()
            },
        )?;
        mark("extract");
        if let Some(edges) = extraction.get_mut("edges").and_then(Value::as_array_mut) {
            crate::extractors::evidence::filter_links(edges);
        }
        watch_paths::rebase(&mut extraction, watch_root, project_root);
        watch_paths::relativize(&mut extraction, project_root, Some(watch_root));
        build::build_from_json(
            &mut extraction,
            g.directed,
            None,
            &mut build::Diagnostics::default(),
        )?
    };
    let source_touched = |attrs: &crate::record::Attributes| {
        attrs
            .get("source_file")
            .and_then(Value::as_str)
            .is_some_and(|s| touched.contains(s))
    };
    let mut kept = vec![];
    for (u, v, e) in g.iter_edges() {
        if removed.contains(&u) != removed.contains(&v) && !source_touched(&e.attributes) {
            kept.push((
                g.nodes[u].id.clone(),
                g.nodes[v].id.clone(),
                g.multigraph.then(|| e.key.clone()),
                e.attributes.clone(),
            ));
        }
    }
    mark("build_fresh");
    let mut out = g.clone();
    mark("clone");
    let removed_ids = removed
        .iter()
        .map(|i| g.nodes[*i].id.clone())
        .collect::<Vec<_>>();
    out.remove_nodes_from(&removed_ids)?;
    let mut next = 0;
    let remap = (0..g.nodes.len())
        .map(|i| {
            (!removed.contains(&i)).then(|| {
                next += 1;
                next - 1
            })
        })
        .collect::<Vec<_>>();
    let first_new = out.node_count();
    let mut merged = false;
    for node in fresh.nodes() {
        // Stubs of context nodes must not overwrite the resident records.
        let exists = out.node_index(&node.id).is_some();
        if !exists || source_touched(&node.attributes) {
            merged |= exists;
            out.add_node(node.id.clone(), node.attributes.clone())?;
        }
    }
    let mut add = |u: Value, v: Value, key: Option<Value>, attrs: crate::record::Attributes| {
        if out.node_index(&u).is_some() && out.node_index(&v).is_some() {
            out.add_edge(u, v, key, attrs).map(|_| ())
        } else {
            Ok(())
        }
    };
    for (u, v, e) in fresh.iter_edges() {
        add(
            fresh.nodes[u].id.clone(),
            fresh.nodes[v].id.clone(),
            fresh.multigraph.then(|| e.key.clone()),
            e.attributes.clone(),
        )?;
    }
    for (u, v, key, attrs) in kept {
        add(u, v, key, attrs)?;
    }
    mark("splice");
    Ok(Some((
        Patched {
            graph: out,
            remap,
            first_new,
            merged,
        },
        touched,
    )))
}

impl LiveGraph {
    /// Serve `graph_path` for the repository at `root`.
    pub fn new(graph_path: &Path, root: &Path) -> Result<Arc<Self>> {
        let graph_path = crate::extractors::resolution::resolved(graph_path);
        let root = crate::extractors::resolution::resolved(root);
        let out = graph_path
            .parent()
            .ok_or("graph.json has no parent directory")?
            .to_path_buf();
        // Stored source paths are relative to the directory the graph was built
        // from. `.graphicle_root` records the path it was given: absolute, or
        // relative to that directory. Confirm against the stored paths.
        let marker = std::fs::read_to_string(out.join(".graphicle_root"))
            .ok()
            .map(|s| PathBuf::from(s.trim_start_matches('\u{feff}').trim()));
        let graph = load_graph(&graph_path)?;
        let mut bases = vec![];
        if let Some(marker) = &marker {
            if marker.is_absolute() {
                bases.push(crate::extractors::resolution::resolved(marker));
            } else if root.ends_with(marker) {
                let mut base = root.clone();
                for _ in marker.components() {
                    base.pop();
                }
                bases.push(base);
            }
        }
        bases.push(root.clone());
        bases.push(crate::extractors::resolution::resolved(Path::new(".")));
        let sample = graph
            .nodes
            .iter()
            .filter_map(|n| n.attributes.get("source_file").and_then(Value::as_str))
            .filter(|s| !s.is_empty() && !Path::new(s).is_absolute())
            .take(25)
            .collect::<Vec<_>>();
        let project_root = bases
            .iter()
            .find(|base| {
                !sample.is_empty()
                    && sample.iter().filter(|s| base.join(s).exists()).count() * 2 > sample.len()
            })
            .cloned()
            .unwrap_or_else(|| root.clone());
        let ignore = IgnoreState::new(
            &root,
            &watch::build_excludes(&out),
            watch::build_gitignore(&out),
            &mut EncodingWarnings::default(),
        )?;
        let context = GraphContext::new(graph)?;
        Ok(Arc::new(Self {
            root,
            project_root,
            out,
            graph_path,
            current: RwLock::new(Arc::new(context)),
            pending: Mutex::new(Pending::default()),
            publish: Mutex::new(()),
            ignore: Mutex::new(ignore),
            stats: Mutex::new(LiveStats::default()),
            context_cache: Mutex::new(None),
            reuse_context: AtomicBool::new(
                !std::env::var("GRAPHICLE_LIVE_CONTEXT_CACHE").is_ok_and(|v| v == "0"),
            ),
        }))
    }
    pub fn graph_path(&self) -> &Path {
        &self.graph_path
    }
    pub fn context(&self) -> Arc<GraphContext> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn stats(&self) -> LiveStats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn fail(&self, error: impl ToString) {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error = Some(error.to_string());
    }
    /// Paths an event batch can affect: inside the root, outside `.graphicle/`
    /// and hidden directories, not ignored, and watched by the `watch` command.
    fn relevant(&self, paths: BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
        let mut ignore = self.ignore.lock().unwrap_or_else(|e| e.into_inner());
        let mut warnings = EncodingWarnings::default();
        paths
            .into_iter()
            .map(|p| crate::detect_ignore::absolute_lexical(&p))
            .filter(|p| {
                let Ok(rel) = p.strip_prefix(&self.root) else {
                    return false;
                };
                !p.starts_with(&self.out)
                    && !rel.components().any(|c| {
                        let name = c.as_os_str().to_string_lossy();
                        name.starts_with('.') || name == crate::paths::OUT_DIR_NAME.as_str()
                    })
                    && !ignore.predicate(p, &mut warnings).unwrap_or(false)
                    && (!p.exists() || watch::batch_triggers_rebuild(std::slice::from_ref(p)))
            })
            .collect()
    }
    /// Keep (the default) or rebuild the resolution context between updates.
    #[doc(hidden)]
    pub fn set_reuse_context(&self, on: bool) {
        self.reuse_context.store(on, Ordering::Relaxed);
        *self.context_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    /// Apply `changed` to the resident graph and publish the result.
    pub fn apply(&self, changed: &BTreeSet<PathBuf>) -> Result<bool> {
        let _publish = self.publish.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.context();
        // The resolution context of the served graph, kept between updates
        // (rebuilt once after a background rebuild replaced the graph).
        let key = Arc::as_ptr(&current) as usize;
        let reuse = self.reuse_context.load(Ordering::Relaxed);
        let mut cached = self
            .context_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|(k, _)| reuse && *k == key)
            .map(|(_, c)| c)
            .unwrap_or_else(|| ContextCache::of(&current.graph));
        let Some((patched, touched)) = patch_with(
            &current.graph,
            &self.root,
            &self.project_root,
            changed,
            Some(&cached),
        )?
        else {
            *self.context_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, cached));
            return Ok(false);
        };
        let started = Instant::now();
        let index = if patched.merged {
            crate::serve_search::SearchIndex::new(&patched.graph)
        } else {
            current
                .index
                .patched(&patched.graph, &patched.remap, patched.first_new)
        };
        cached = cached.after(&patched.graph, &touched);
        // A record merged into a surviving node changed that node: start over.
        let keep_cache = !patched.merged;
        let next = Arc::new(GraphContext::with_index(patched.graph, index)?);
        if std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1") {
            eprintln!(
                "[live timing] context_index={:.1}ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        *self.context_cache.lock().unwrap_or_else(|e| e.into_inner()) =
            keep_cache.then_some((Arc::as_ptr(&next) as usize, cached));
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = next;
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fast_patches += 1;
        Ok(true)
    }
    /// Apply a batch in memory and queue it for the background rebuild, as
    /// the event loop does for every batch it sees.
    pub fn observe(&self, changed: &BTreeSet<PathBuf>) -> Result<bool> {
        let applied = self.apply(changed);
        self.note(changed);
        applied
    }
    /// Record a batch for the next background rebuild.
    fn note(&self, changed: &BTreeSet<PathBuf>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.exact.extend(changed.iter().cloned());
        if pending.running {
            pending.since_start.extend(changed.iter().cloned());
        }
        pending.last_change = Some(Instant::now());
    }
    /// Run the incremental rebuild of `.graphicle/` for everything noted so
    /// far, then serve the rebuilt graph.json. Changes that arrived while it
    /// ran are re-applied on top and left for the next rebuild.
    pub fn settle(&self) -> Result<()> {
        match self.take_batch(false) {
            Some(batch) => self.settle_with(batch),
            None => Ok(()),
        }
    }
    /// The pending batch, marking a background rebuild as running. With `due`,
    /// only once changes have been quiet for `SETTLE`.
    fn take_batch(&self, due: bool) -> Option<BTreeSet<PathBuf>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.running
            || pending.exact.is_empty()
            || due && pending.last_change.is_none_or(|t| t.elapsed() < SETTLE)
        {
            return None;
        }
        pending.running = true;
        pending.since_start.clear();
        Some(std::mem::take(&mut pending.exact))
    }
    fn settle_with(&self, batch: BTreeSet<PathBuf>) -> Result<()> {
        let result = (|| {
            watch::rebuild(
                &self.root,
                &watch::RebuildOptions {
                    changed_paths: Some(batch.iter().cloned().collect()),
                    ..Default::default()
                },
            )?;
            let _publish = self.publish.lock().unwrap_or_else(|e| e.into_inner());
            let later = std::mem::take(
                &mut self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .since_start,
            );
            let mut graph = load_graph(&self.graph_path)?;
            if !later.is_empty()
                && let Some(patched) = patch(&graph, &self.root, &self.project_root, &later)?
            {
                graph = patched.graph;
            }
            *self.current.write().unwrap_or_else(|e| e.into_inner()) =
                Arc::new(GraphContext::new(graph)?);
            // The served graph is a new one: its context is rebuilt on demand.
            *self.context_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Ok::<_, crate::Error>(())
        })();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.running = false;
        if result.is_err() {
            pending.exact.extend(batch);
        }
        drop(pending);
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exact_rebuilds += 1;
        result
    }
    /// Watch the repository with OS file events until `stop` is set.
    pub fn run(self: &Arc<Self>, stop: &'static AtomicBool) -> Result<()> {
        use notify::Watcher;
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event
                    && !matches!(event.kind, notify::EventKind::Access(_))
                {
                    for path in event.paths {
                        let _ = tx.send(path);
                    }
                }
            })
            .map_err(|e| crate::Error::from(format!("cannot watch: {e}")))?;
        watcher
            .watch(&self.root, notify::RecursiveMode::Recursive)
            .map_err(|e| {
                crate::Error::from(format!("cannot watch {}: {e}", self.root.display()))
            })?;
        eprintln!("Live: watching {}", self.root.display());
        while !stop.load(Ordering::Relaxed) {
            let mut batch = BTreeSet::new();
            let mut first_seen = None;
            if let Ok(first) = rx.recv_timeout(Duration::from_millis(100)) {
                first_seen = Some(Instant::now());
                batch.insert(first);
                while let Ok(next) = rx.recv_timeout(DEBOUNCE) {
                    batch.insert(next);
                }
            }
            let batch = self.relevant(batch);
            if !batch.is_empty() {
                let started = Instant::now();
                match self.apply(&batch) {
                    Ok(true) => eprintln!(
                        "Live update: {} file(s) in {:.0} ms (batched for {:.0} ms)",
                        batch.len(),
                        started.elapsed().as_secs_f64() * 1000.0,
                        first_seen.map_or(0.0, |t| (started - t).as_secs_f64() * 1000.0)
                    ),
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("Live update failed: {e}");
                        self.fail(e);
                    }
                }
                self.note(&batch);
            }
            if let Some(batch) = self.take_batch(true) {
                let live = self.clone();
                std::thread::spawn(move || {
                    let started = Instant::now();
                    match live.settle_with(batch) {
                        Ok(()) => eprintln!(
                            "Live rebuild: .graphicle updated in {:.0} ms",
                            started.elapsed().as_secs_f64() * 1000.0
                        ),
                        Err(e) => {
                            eprintln!("Live rebuild failed: {e}");
                            live.fail(e);
                        }
                    }
                });
            }
        }
        Ok(())
    }
}
