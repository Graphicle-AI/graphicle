// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py, graphify/ids.py, graphify/serve.py
//! Resident graphs shared across requests.
//!
//! Every graph operation on the JSON boundary used to carry its whole
//! node-link document, so a 20,000-node `affected.hits` spent nearly all of
//! its time parsing JSON that the previous request had already parsed. A
//! [`GraphHandle`] keeps one loaded [`Graph`] behind an `Arc`; in-process
//! callers hold the handle directly and boundary callers refer to it by the
//! `graph_ref` string that `graph.open` returns.
//!
//! Handles are immutable. Requests that ask for mutations (`actions`,
//! `predecessor_order`) work on a private copy, so a shared graph can never
//! observe another request's edits. The registry is bounded and evicts the
//! least recently used graph; `GRAPHICLE_MAX_GRAPH_REFS` overrides the default
//! of eight resident graphs.
use crate::{Error, Result, graph::Graph};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    ops::Deref,
    path::Path,
    sync::{Arc, LazyLock, Mutex, Weak},
    time::Instant,
};

/// Cheap-to-clone shared ownership of a loaded graph.
#[derive(Clone, Debug)]
pub struct GraphHandle(Arc<Graph>);

impl GraphHandle {
    pub fn new(mut graph: Graph) -> Self {
        for node in graph.nodes_mut() {
            node.attributes.compact();
        }
        for edge in graph.edges_mut() {
            edge.attributes.compact();
        }
        Self(Arc::new(graph))
    }

    /// Load a node-link document; `edges_key` names its edge array
    /// (`links` or `edges`).
    pub fn from_node_link_json(bytes: &[u8], edges_key: &str) -> Result<Self> {
        let graph = match std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| Graph::from_node_link_json(text, edges_key))
        {
            Some(graph) => graph,
            None => Graph::from_node_link_owned(
                serde_json::from_slice(bytes).map_err(Error::from)?,
                edges_key,
            )?,
        };
        Ok(Self::new(graph))
    }

    /// Load a `graph.json` the way the `affected` command does: directed, with an
    /// `edges` array accepted in place of `links`.
    pub fn from_path(path: &Path) -> Result<Self> {
        Ok(Self::new(crate::affected::load_graph(path)?))
    }

    /// The graph itself when this is the last handle, otherwise a copy.
    pub fn into_graph(self) -> Graph {
        Arc::unwrap_or_clone(self.0)
    }

    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Identity of the shared graph instance, for caches keyed by handle.
    pub fn ptr_id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Weak ownership for caches: it preserves allocation identity without
    /// keeping a closed graph's contents resident.
    pub fn downgrade(&self) -> Weak<Graph> {
        Arc::downgrade(&self.0)
    }
}

impl Deref for GraphHandle {
    type Target = Graph;
    fn deref(&self) -> &Graph {
        &self.0
    }
}

impl From<Graph> for GraphHandle {
    fn from(graph: Graph) -> Self {
        Self::new(graph)
    }
}

/// What the registry knows about one resident graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphInfo {
    pub graph_ref: String,
    pub nodes: usize,
    pub edges: usize,
    pub directed: bool,
    pub multigraph: bool,
    /// Requests served from this handle, including the opening request.
    pub uses: u64,
}

struct Entry {
    info: GraphInfo,
    handle: GraphHandle,
    last_used: Instant,
}

struct State {
    entries: Vec<Entry>,
    next: u64,
}

/// Bounded, least-recently-used registry of resident graphs.
pub struct GraphRegistry {
    state: Mutex<State>,
    capacity: usize,
}

pub const DEFAULT_CAPACITY: usize = 8;

impl GraphRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                entries: Vec::new(),
                next: 1,
            }),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means another request panicked mid-update; the
        // entries themselves are always consistent between statements.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a graph. A caller-chosen `name` replaces any graph already
    /// registered under it; otherwise a fresh `g<n>` reference is minted.
    pub fn open(&self, handle: GraphHandle, name: Option<&str>) -> GraphInfo {
        let mut state = self.lock();
        let mut removed = Vec::new();
        let graph_ref = match name {
            Some(name) => {
                state.entries.retain(|entry| {
                    if entry.info.graph_ref == name {
                        removed.push(entry.info.graph_ref.clone());
                        false
                    } else {
                        true
                    }
                });
                name.to_owned()
            }
            None => loop {
                let reference = format!("g{}", state.next);
                state.next = state.next.wrapping_add(1);
                // Caller-chosen names share this namespace. Never return a
                // reference that would resolve to an already resident graph.
                if !state
                    .entries
                    .iter()
                    .any(|entry| entry.info.graph_ref == reference)
                {
                    break reference;
                }
            },
        };
        let info = GraphInfo {
            graph_ref,
            nodes: handle.node_count(),
            edges: handle.edge_count(),
            directed: handle.is_directed(),
            multigraph: handle.is_multigraph(),
            uses: 1,
        };
        state.entries.push(Entry {
            info: info.clone(),
            handle,
            last_used: Instant::now(),
        });
        while state.entries.len() > self.capacity {
            let oldest = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
                .unwrap_or(0);
            removed.push(state.entries.swap_remove(oldest).info.graph_ref);
        }
        drop(state);
        for name in removed {
            notify_closed(&name);
        }
        info
    }

    /// Inspect a resident handle without changing its usage count or LRU
    /// position. Cache maintenance must not count as a user query.
    pub fn peek(&self, graph_ref: &str) -> Option<GraphHandle> {
        self.lock()
            .entries
            .iter()
            .find(|entry| entry.info.graph_ref == graph_ref)
            .map(|entry| entry.handle.clone())
    }

    /// Look a reference up, counting the use.
    pub fn get(&self, graph_ref: &str) -> Result<GraphHandle> {
        let mut state = self.lock();
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| entry.info.graph_ref == graph_ref)
            .ok_or_else(|| Error::invalid(format!("Unknown graph_ref: {graph_ref}")))?;
        entry.info.uses += 1;
        entry.last_used = Instant::now();
        Ok(entry.handle.clone())
    }

    /// Drop a reference; `true` when it was registered.
    pub fn close(&self, graph_ref: &str) -> bool {
        let closed = {
            let mut state = self.lock();
            let before = state.entries.len();
            state
                .entries
                .retain(|entry| entry.info.graph_ref != graph_ref);
            state.entries.len() != before
        };
        if closed {
            notify_closed(graph_ref);
        }
        closed
    }

    pub fn list(&self) -> Vec<GraphInfo> {
        let state = self.lock();
        state
            .entries
            .iter()
            .map(|entry| entry.info.clone())
            .collect()
    }

    pub fn clear(&self) {
        let entries = std::mem::take(&mut self.lock().entries);
        for entry in entries {
            notify_closed(&entry.info.graph_ref);
        }
    }
}

fn configured_capacity() -> usize {
    std::env::var("GRAPHICLE_MAX_GRAPH_REFS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|&value: &usize| value > 0)
        .unwrap_or(DEFAULT_CAPACITY)
}

/// The process-wide registry behind `graph_ref` on the JSON boundary.
pub fn registry() -> &'static GraphRegistry {
    static REGISTRY: LazyLock<GraphRegistry> =
        LazyLock::new(|| GraphRegistry::new(configured_capacity()));
    &REGISTRY
}

/// Callback run with the reference name when a graph reference is closed.
pub type CloseHook = fn(&str);

fn close_hooks() -> &'static Mutex<Vec<CloseHook>> {
    static HOOKS: LazyLock<Mutex<Vec<CloseHook>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    &HOOKS
}

fn notify_closed(graph_ref: &str) {
    // Callbacks may inspect the registry or register another callback. Do not
    // hold either mutex while running code owned by an embedding application.
    let hooks = close_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for hook in hooks {
        hook(graph_ref);
    }
}

/// Run `hook` with the reference name whenever [`GraphRegistry::close`]
/// drops a reference, including replacement, eviction and clearing, so code
/// that caches state per `graph_ref` can release it. Registering the same
/// function twice has no further effect.
pub fn on_close(hook: CloseHook) {
    let mut hooks = close_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !hooks
        .iter()
        .any(|existing| std::ptr::fn_addr_eq(*existing, hook))
    {
        hooks.push(hook);
    }
}

/// `graph.open` / `graph.close` / `graph.list` on the JSON boundary.
///
/// `graph.open` takes either an inline node-link `graph` (edge array named by
/// `edges_key`, default `links`) or a `path` to a `graph.json`, plus an
/// optional `name`. When the caller streamed the payload already, `loaded`
/// carries the graph and the request's `graph` field is a placeholder.
pub(crate) fn dispatch(request: &Value, loaded: Option<Graph>) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "graph.open" => {
            let name = match request.get("name") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) if !name.is_empty() => Some(name.as_str()),
                Some(_) => return Err(Error::invalid("Expected name to be a non-empty string")),
            };
            let handle = match (loaded, request.get("graph"), request.get("path")) {
                (Some(graph), _, None) => GraphHandle::new(graph),
                (None, Some(graph), None) if !graph.is_null() => {
                    GraphHandle::new(Graph::from_node_link(graph, edges_key(request)?)?)
                }
                (None, None | Some(Value::Null), Some(Value::String(path))) => {
                    GraphHandle::from_path(Path::new(path))?
                }
                (_, Some(graph), Some(_)) if !graph.is_null() => {
                    return Err(Error::invalid("Provide either graph or path, not both"));
                }
                (Some(_), _, Some(_)) => {
                    return Err(Error::invalid("Provide either graph or path, not both"));
                }
                (_, _, Some(_)) => return Err(Error::invalid("Expected path to be a string")),
                (None, _, None) => return Err(Error::invalid("Expected graph or path")),
            };
            Ok(json!(registry().open(handle, name)))
        }
        "graph.close" => Ok(json!({"closed": registry().close(reference(request)?)})),
        "graph.list" => Ok(json!({"graphs": registry().list()})),
        _ => Err(Error::UnsupportedOperation(
            "Unsupported graph session operation".into(),
        )),
    }
}

/// The edge array name a `graph.open` request wants, `links` by default.
#[doc(hidden)]
pub fn edges_key(request: &Value) -> Result<&'static str> {
    match request.get("edges_key") {
        None | Some(Value::Null) => Ok("links"),
        Some(Value::String(key)) if key == "links" => Ok("links"),
        Some(Value::String(key)) if key == "edges" => Ok("edges"),
        Some(_) => Err(Error::invalid("Expected edges_key to be links or edges")),
    }
}

pub(crate) fn reference(request: &Value) -> Result<&str> {
    match request.get("graph_ref") {
        Some(Value::String(reference)) if !reference.is_empty() => Ok(reference),
        _ => Err(Error::invalid(
            "Expected graph_ref to be a non-empty string",
        )),
    }
}

/// Resolve the `graph_ref` of a graph-consuming request, if it carries one.
///
/// A request may name a resident graph or carry an inline payload, never both.
pub(crate) fn referenced(request: &Value, has_payload: bool) -> Result<Option<GraphHandle>> {
    match request.get("graph_ref") {
        None | Some(Value::Null) => Ok(None),
        Some(_) if has_payload => Err(Error::invalid(
            "Provide either graph or graph_ref, not both",
        )),
        Some(_) => registry().get(reference(request)?).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hooks_cover_all_removals_and_can_register_hooks() {
        static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn noop(_: &str) {}
        fn record(name: &str) {
            if name.starts_with("review_lifecycle_") {
                assert!(close_hooks().try_lock().is_ok(), "callback mutex held");
                on_close(noop);
                EVENTS.lock().unwrap().push(name.into());
            }
        }
        on_close(record);
        let registry = GraphRegistry::new(1);
        let open = |name| registry.open(GraphHandle::new(Graph::new(true, false)), Some(name));
        open("review_lifecycle_a");
        open("review_lifecycle_a"); // replacement
        open("review_lifecycle_b"); // eviction
        registry.clear();
        open("review_lifecycle_c");
        assert!(registry.close("review_lifecycle_c"));
        assert!(!registry.close("review_lifecycle_missing"));
        assert_eq!(
            *EVENTS.lock().unwrap(),
            vec![
                "review_lifecycle_a",
                "review_lifecycle_a",
                "review_lifecycle_b",
                "review_lifecycle_c",
            ]
        );
    }

    #[test]
    fn inspecting_a_handle_does_not_extend_its_lru_lifetime() {
        let registry = GraphRegistry::new(2);
        let first = registry.open(GraphHandle::new(Graph::new(true, false)), Some("first"));
        registry.open(GraphHandle::new(Graph::new(true, false)), Some("second"));
        let original = registry.peek("first").unwrap();
        let weak = original.downgrade();
        assert!(weak.ptr_eq(&registry.peek("first").unwrap().downgrade()));
        assert_eq!(registry.list()[0].uses, first.uses);
        registry.open(GraphHandle::new(Graph::new(true, false)), Some("third"));
        assert!(registry.peek("first").is_none());
        assert!(registry.peek("second").is_some());
        drop(original);
        assert!(weak.upgrade().is_none());
    }
    use serde_json::Map;

    fn graph(nodes: usize) -> Graph {
        let mut graph = Graph::new(true, false);
        for index in 0..nodes {
            graph
                .add_node(json!(format!("n{index}")), Map::new())
                .unwrap();
        }
        for index in 1..nodes {
            graph
                .add_edge(
                    json!(format!("n{index}")),
                    json!("n0"),
                    None,
                    Map::from_iter([("relation".to_string(), json!("calls"))]),
                )
                .unwrap();
        }
        graph
    }

    #[test]
    fn handles_share_one_graph_and_copy_only_when_shared() {
        let handle = GraphHandle::new(graph(3));
        let second = handle.clone();
        assert_eq!(handle.strong_count(), 2);
        assert_eq!(second.node_count(), 3);
        drop(second);
        // Last handle: the graph moves out without a copy.
        assert_eq!(handle.into_graph().edge_count(), 2);
    }

    #[test]
    fn registry_mints_references_counts_uses_and_evicts_the_least_recent() {
        let registry = GraphRegistry::new(2);
        let first = registry.open(GraphHandle::new(graph(1)), None);
        let second = registry.open(GraphHandle::new(graph(2)), None);
        assert_eq!(
            (first.graph_ref.as_str(), second.graph_ref.as_str()),
            ("g1", "g2")
        );
        assert_eq!(registry.get("g1").unwrap().node_count(), 1);
        assert_eq!(registry.list()[0].uses, 2);
        // g2 is now the least recently used and goes first.
        let third = registry.open(GraphHandle::new(graph(3)), None);
        assert_eq!(third.graph_ref, "g3");
        let refs: Vec<_> = registry.list().into_iter().map(|g| g.graph_ref).collect();
        assert_eq!(refs, ["g1", "g3"]);
        assert!(registry.get("g2").is_err());
        assert!(registry.close("g1"));
        assert!(!registry.close("g1"));
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn named_references_replace_and_keep_their_name() {
        let registry = GraphRegistry::new(4);
        assert_eq!(
            registry
                .open(GraphHandle::new(graph(1)), Some("main"))
                .graph_ref,
            "main"
        );
        registry.open(GraphHandle::new(graph(5)), Some("main"));
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("main").unwrap().node_count(), 5);
    }

    #[test]
    fn automatic_references_skip_live_names_and_close_independently() {
        let registry = GraphRegistry::new(8);
        registry.open(GraphHandle::new(graph(1)), Some("g1"));
        registry.open(GraphHandle::new(graph(2)), Some("g2"));
        let first = registry.open(GraphHandle::new(graph(3)), None);
        assert_eq!(first.graph_ref, "g3");
        registry.open(GraphHandle::new(graph(4)), Some("g4"));
        let second = registry.open(GraphHandle::new(graph(5)), None);
        assert_eq!(second.graph_ref, "g5");
        assert_eq!(registry.list().len(), 5);
        for (reference, nodes) in [("g1", 1), ("g2", 2), ("g3", 3), ("g4", 4), ("g5", 5)] {
            assert_eq!(registry.get(reference).unwrap().node_count(), nodes);
        }
        assert!(registry.close(&first.graph_ref));
        assert!(registry.get(&first.graph_ref).is_err());
        assert_eq!(registry.get("g1").unwrap().node_count(), 1);
        assert_eq!(registry.get(&second.graph_ref).unwrap().node_count(), 5);
        // Explicit replacement still replaces exactly one named entry.
        registry.open(GraphHandle::new(graph(6)), Some("g1"));
        assert_eq!(registry.get("g1").unwrap().node_count(), 6);
        assert_eq!(registry.list().len(), 4);
    }

    #[test]
    fn open_from_json_bytes_matches_the_dynamic_loader() {
        let text = r#"{"directed": true, "multigraph": false, "graph": {}, "nodes": [{"id": "a", "label": "A"}, {"id": "b"}], "links": [{"source": "b", "target": "a", "relation": "calls"}]}"#;
        let handle = GraphHandle::from_node_link_json(text.as_bytes(), "links").unwrap();
        let dynamic = Graph::from_node_link(&serde_json::from_str(text).unwrap(), "links").unwrap();
        assert_eq!(handle.to_node_link("links"), dynamic.to_node_link("links"));
        assert!(GraphHandle::from_node_link_json(b"not json", "links").is_err());
    }

    fn payload() -> Value {
        json!({
            "directed": true,
            "nodes": [
                {"id": "app.py", "label": "app.py"},
                {"id": "handler", "label": "handler"},
                {"id": "helper", "label": "helper"},
                {"id": "util", "label": "util"},
                {"id": "orphan", "label": "orphan"}
            ],
            "links": [
                {"source": "app.py", "target": "handler", "relation": "contains"},
                {"source": "handler", "target": "helper", "relation": "calls"},
                {"source": "helper", "target": "util", "relation": "calls"}
            ]
        })
    }

    fn open(name: &str, edges_key: &str) -> Value {
        let mut payload = payload();
        if edges_key != "links" {
            let links = payload["links"].take();
            payload[edges_key] = links;
        }
        let request = json!({
            "operation": "graph.open", "name": name, "edges_key": edges_key, "graph": payload,
        });
        let opened = crate::dispatch_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(opened["graph_ref"], json!(name));
        assert_eq!(opened["nodes"], json!(5));
        assert_eq!(opened["edges"], json!(3));
        opened
    }

    fn both(request: Value) -> (Result<Value>, Result<Value>) {
        let streamed = crate::dispatch_slice(&serde_json::to_vec(&request).unwrap());
        (streamed, crate::dispatch_owned(request))
    }

    #[test]
    fn referenced_graphs_answer_like_inline_payloads() {
        open("session-a", "links");
        for query in [
            json!({"operation": "affected.hits", "seed": "util", "depth": 3}),
            json!({"operation": "affected.resolve", "query": "handler"}),
            json!({"operation": "affected.format", "query": "util", "depth": 2}),
            json!({"operation": "affected.label", "seed": "helper"}),
            json!({"operation": "cluster.partition", "backend": "leiden"}),
            json!({"operation": "cluster.run", "backend": "louvain"}),
        ] {
            let mut inline = query.clone();
            let edges_key = if query["operation"].as_str().unwrap().starts_with("cluster") {
                "edges"
            } else {
                "links"
            };
            let mut payload = payload();
            if edges_key == "edges" {
                let links = payload["links"].take();
                payload["edges"] = links;
            }
            inline["graph"] = payload;
            let expected = crate::dispatch_owned(inline).unwrap();
            let mut by_ref = query.clone();
            by_ref["graph_ref"] = json!("session-a");
            let (streamed, owned) = both(by_ref);
            assert_eq!(streamed.unwrap(), expected, "{query}");
            assert_eq!(owned.unwrap(), expected, "{query}");
        }
        assert!(registry().close("session-a"));
    }

    #[test]
    fn opening_streams_the_payload_and_accepts_other_edge_keys() {
        let by_stream = open("session-stream", "edges");
        let handle = registry().get("session-stream").unwrap();
        let dynamic = Graph::from_node_link(&payload(), "links").unwrap();
        assert_eq!(handle.to_node_link("links"), dynamic.to_node_link("links"));
        assert_eq!(by_stream["directed"], json!(true));
        let listed = crate::dispatch(&json!({"operation": "graph.list"})).unwrap();
        assert!(
            listed["graphs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|info| info["graph_ref"] == "session-stream")
        );
        assert_eq!(
            crate::dispatch(&json!({"operation": "graph.close", "graph_ref": "session-stream"}))
                .unwrap(),
            json!({"closed": true})
        );
        assert_eq!(
            crate::dispatch(&json!({"operation": "graph.close", "graph_ref": "session-stream"}))
                .unwrap(),
            json!({"closed": false})
        );
    }

    #[test]
    fn mutating_requests_leave_the_resident_graph_untouched() {
        open("session-cow", "links");
        let request = json!({
            "operation": "affected.hits", "seed": "orphan", "depth": 2, "graph_ref": "session-cow",
            "actions": [{"kind": "edge", "source": "util", "target": "orphan",
                         "attributes": {"relation": "calls"}}],
        });
        let (streamed, owned) = both(request);
        let hits = streamed.unwrap();
        assert_eq!(hits, owned.unwrap());
        assert_eq!(hits[0]["node_id"], json!("util"));
        assert_eq!(registry().get("session-cow").unwrap().edge_count(), 3);
        let plain = crate::dispatch(&json!({
            "operation": "affected.hits", "seed": "orphan", "graph_ref": "session-cow",
        }))
        .unwrap();
        assert_eq!(plain, json!([]));
        assert!(registry().close("session-cow"));
    }

    #[test]
    fn reference_errors_match_on_both_paths() {
        open("session-err", "links");
        let cases = [
            (
                json!({"operation": "affected.hits", "seed": "util", "graph_ref": "no-such-graph"}),
                "Unknown graph_ref",
            ),
            (
                json!({"operation": "affected.hits", "seed": "util", "graph_ref": "session-err",
                       "graph": payload()}),
                "graph or graph_ref",
            ),
            (
                json!({"operation": "graph.open", "graph": payload(), "edges_key": "nodes"}),
                "edges_key",
            ),
            (
                json!({"operation": "graph.open", "graph": payload(), "path": "x.json"}),
                "graph or path",
            ),
            (json!({"operation": "graph.open"}), "graph or path"),
        ];
        for (request, needle) in cases {
            let (streamed, owned) = both(request.clone());
            let streamed = streamed.unwrap_err().to_string();
            let owned = owned.unwrap_err().to_string();
            assert_eq!(streamed, owned, "{request}");
            assert!(streamed.contains(needle), "{streamed} lacks {needle}");
        }
        assert!(registry().close("session-err"));
    }
}
