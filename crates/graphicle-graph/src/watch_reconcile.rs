// Adapted from Graphify 0.9.102 (59b5ab11), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/watch.py
//! Preserve semantic and unaffected AST records during incremental rebuilds.

#[derive(Clone, Copy)]
pub struct ReconcileContext<'a> {
    pub full: bool,
    pub root: &'a Path,
    pub paths: &'a StoredSourcePaths,
}
use crate::{
    Result, build, extract, extract_merge::file_node_id, extractors::base::make_id, python_compat,
    watch_paths::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};
fn pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}
pub fn reconcile_markdown(
    result: &mut Value,
    mut preserved_edges: Vec<Value>,
    preserved_nodes: &[Value],
    code: &[PathBuf],
    targets: &[PathBuf],
    context: ReconcileContext<'_>,
) -> Vec<Value> {
    let ReconcileContext { full, root, paths } = context;

    let _scan_root = crate::extractors::markdown::enter_scan_root(Some(root));
    // Only the nodes are read below; the edges move out now and back at the
    // end (the slot keeps its place in the object).
    let mut edges = take_items(result, "edges");
    let markdown = (if full { code } else { targets })
        .iter()
        .filter(|p| {
            crate::markdown_resolution::MARKDOWN_MENTION_SUFFIXES
                .contains(&crate::extract_languages::suffix(p).as_str())
        })
        .collect::<Vec<_>>();
    // With no Markdown inputs, no owner is parsed and every existing edge
    // survives. Avoid indexing all nodes and edges just to return them.
    if markdown.is_empty() {
        result["edges"] = Value::Array(edges);
        return preserved_edges;
    }
    // Borrow the current and preserved nodes instead of cloning them: this
    // function only reads them and rewrites `result["edges"]` at the end.
    let all = result["nodes"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .chain(preserved_nodes.iter())
        .collect::<Vec<&Value>>();
    let byid = all
        .iter()
        .map(|n| (text(n, "id"), *n))
        .collect::<BTreeMap<_, _>>();
    let mut bysource = BTreeMap::<String, Vec<&Value>>::new();
    for n in &all {
        let sf = text(n, "source_file");
        if !sf.is_empty() {
            bysource.entry(paths.normalize(&sf)).or_default().push(n);
        }
    }
    let reps = bysource
        .iter()
        .map(|(sf, ns)| {
            let canon = file_node_id(Path::new(sf));
            let groups = [
                ns.iter()
                    .filter(|n| n["id"] == canon)
                    .copied()
                    .collect::<Vec<_>>(),
                ns.iter()
                    .filter(|n| n["node_kind"] == "page")
                    .copied()
                    .collect(),
                ns.iter()
                    .filter(|n| n["file_type"] == "document" && n["_origin"] != "ast")
                    .copied()
                    .collect(),
            ];
            (
                sf.clone(),
                groups
                    .iter()
                    .find(|g| g.len() == 1)
                    .map(|g| text(g[0], "id")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut parsed = BTreeSet::new();
    let mut authored = BTreeSet::new();
    let mut raw_pairs = BTreeSet::new();
    let mut unresolved = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut resolved_pairs = BTreeMap::new();
    let mut desired = vec![];
    let node_sources = all
        .iter()
        .filter(|n| !text(n, "source_file").is_empty())
        .map(|n| (text(n, "id"), paths.normalize(&text(n, "source_file"))))
        .collect::<BTreeMap<_, _>>();
    for (p, extraction) in markdown
        .iter()
        .copied()
        .zip(extract_markdown_all(&markdown, root))
    {
        let sf = paths.normalize(&p.strip_prefix(root).unwrap_or(p).to_string_lossy());
        let Ok(extraction) = extraction else {
            continue;
        };
        if extraction.get("error").is_some() {
            continue;
        }
        parsed.insert(sf.clone());
        for e in items(&extraction, "edges") {
            if e["relation"] != "references" {
                continue;
            }
            let tf = text(&e, "target_file");
            if tf.is_empty() {
                let loc = text(&e, "source_location");
                if !loc.is_empty() {
                    unresolved
                        .entry((sf.clone(), loc))
                        .or_default()
                        .insert(text(&e, "target"));
                }
                continue;
            }
            let target = Path::new(&tf);
            let tf = paths.normalize(
                &target
                    .strip_prefix(root)
                    .unwrap_or(target)
                    .to_string_lossy(),
            );
            authored.insert((sf.clone(), tf.clone()));
            let raw = pair(&file_node_id(Path::new(&sf)), &file_node_id(Path::new(&tf)));
            raw_pairs.insert(raw.clone());
            if let (Some(Some(a)), Some(Some(b))) = (reps.get(&sf), reps.get(&tf))
                && a != b
            {
                resolved_pairs.insert(raw, pair(a, b));
                desired.push((a.clone(), b.clone(), e, sf.clone(), tf));
            }
        }
    }
    let desired_reps = desired
        .iter()
        .map(|(a, b, _, sf, tf)| (pair(sf, tf), pair(a, b)))
        .collect::<BTreeMap<_, _>>();
    let keep = |e: &Value| {
        if !build::is_ast_tier(e) || e["relation"] != "references" {
            return true;
        }
        if byid.get(&text(e, "target")).is_some_and(|n| {
            n["file_type"] == "code" && !crate::markdown_resolution::is_file_node(n)
        }) {
            return true;
        }
        let owner = paths.normalize(&text(e, "source_file"));
        if !parsed.contains(&owner) {
            return true;
        }
        if let Some(unknown) = unresolved.get(&(owner.clone(), text(e, "source_location"))) {
            for endpoint in ["source", "target"] {
                if let Some(sf) = node_sources
                    .get(&text(e, endpoint))
                    .filter(|sf| *sf != &owner)
                {
                    let candidate = root
                        .join(Path::new(&owner).parent().unwrap_or(Path::new(".")))
                        .join(Path::new(sf).file_name().unwrap_or_default());
                    if unknown.contains(&make_id([candidate.to_string_lossy().as_ref()])) {
                        return true;
                    }
                }
            }
        }
        let raw = pair(&text(e, "source"), &text(e, "target"));
        if raw_pairs.contains(&raw) {
            return resolved_pairs.get(&raw).is_none_or(|p| p == &raw);
        }
        let endpoints = ["source", "target"]
            .into_iter()
            .filter_map(|k| node_sources.get(&text(e, k)))
            .filter(|sf| **sf != owner)
            .cloned()
            .collect::<BTreeSet<_>>();
        let target = if endpoints.len() == 1 {
            endpoints.into_iter().next().unwrap()
        } else {
            let tf = text(e, "target_file");
            if tf.is_empty() {
                return true;
            }
            let p = Path::new(&tf);
            paths.normalize(&p.strip_prefix(root).unwrap_or(p).to_string_lossy())
        };
        if !authored.contains(&(owner.clone(), target.clone())) {
            return false;
        }
        desired_reps
            .get(&pair(&owner, &target))
            .is_none_or(|p| p == &raw)
    };
    edges.retain(&keep);
    preserved_edges.retain(keep);
    // Pruning above is necessary even with no replacement links. The
    // duplicate-pair index is only needed when a link will be inserted.
    if desired.is_empty() {
        result["edges"] = Value::Array(edges);
        return preserved_edges;
    }
    let mut pairs = edges
        .iter()
        .chain(&preserved_edges)
        .map(|e| pair(&text(e, "source"), &text(e, "target")))
        .collect::<BTreeSet<_>>();
    for (a, b, mut e, sf, _) in desired {
        if pairs.insert(pair(&a, &b)) {
            e.as_object_mut().unwrap().shift_remove("target_file");
            e["source"] = json!(a);
            e["target"] = json!(b);
            e["source_file"] = json!(sf);
            e["_origin"] = json!("ast");
            edges.push(e);
        }
    }
    result["edges"] = Value::Array(edges);
    preserved_edges
}
/// The array under `key`, moved out of `v` (empty when absent), leaving null.
fn take_items(v: &mut Value, key: &str) -> Vec<Value> {
    match v.get_mut(key).map(Value::take) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}
/// Re-extract the Markdown files a reconcile consults, in input order.
///
/// Each file is independent (the extractor only reads the file, the shared
/// link index behind a mutex and the thread-local scan root), so larger sets
/// are parsed on worker threads; the returned order always matches `files`.
fn extract_markdown_all(files: &[&PathBuf], root: &Path) -> Vec<Result<Value>> {
    let workers = std::env::var("GRAPHICLE_MAX_WORKERS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
        })
        .clamp(1, files.len().max(1));
    if workers < 2 || files.len() < 4 {
        return files
            .iter()
            .map(|p| crate::extractors::markdown::extract_markdown(p))
            .collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (next, slots) = (&next, &slots);
            scope.spawn(move || {
                let _scan_root = crate::extractors::markdown::enter_scan_root(Some(root));
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = files.get(i) else {
                        break;
                    };
                    let result = crate::extractors::markdown::extract_markdown(p);
                    slots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((i, result));
                }
            });
        }
    });
    let mut slots = slots.into_inner().unwrap_or_else(|e| e.into_inner());
    slots.sort_by_key(|(i, _)| *i);
    slots.into_iter().map(|(_, r)| r).collect()
}
/// `_referenced_unscanned_identities` (0.9.78, #4161): missing source paths
/// that were never scanned but an unchanged file still points at. An
/// extractor can mint a node for a file outside the checkout (an unresolved
/// dynamic import target, a project a `.sln` names); its `source_file` is that
/// path, but it is the referring file's output. Deletion evidence is the
/// previous scan: a path the manifest lists was a real file and is evicted as
/// before. A never-scanned path keeps its nodes while a referrer that is not
/// re-extracted this run still points at them. A path whose nodes carry a
/// `source_location`, or that is the `source_file` of any edge, was extracted
/// itself and is evicted even if the manifest is stale. Without a readable
/// manifest that lists today's files nothing changes.
pub fn referenced_unscanned_identities(
    existing: &Value,
    paths: &StoredSourcePaths,
    out: &Path,
    project_root: &Path,
    live_sources: &BTreeSet<String>,
    rebuilt_sources: &BTreeSet<String>,
) -> BTreeSet<String> {
    let Some(scanned) = std::fs::read(out.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return BTreeSet::new();
    };
    let Some(scanned) = scanned.as_object() else {
        return BTreeSet::new();
    };
    let scanned_before = scanned
        .keys()
        .map(|key| paths.absolute_identity(key, project_root))
        .collect::<BTreeSet<_>>();
    if scanned_before.is_disjoint(live_sources) {
        // A manifest that lists none of today's files was anchored elsewhere
        // (or is unusable): it is no evidence either way.
        return BTreeSet::new();
    }
    let mut owner = HashMap::<String, String>::new();
    let mut produced_by_itself = BTreeSet::new();
    for node in existing["nodes"].as_array().into_iter().flatten() {
        let source_file = text(node, "source_file");
        let id = text(node, "id");
        if source_file.is_empty() || id.is_empty() || !tier_is_ast(node) {
            continue;
        }
        let identity = paths.identity(&source_file);
        if !identity.is_empty()
            && !live_sources.contains(&identity)
            && !scanned_before.contains(&identity)
        {
            if build::truthy(&node["source_location"]) {
                produced_by_itself.insert(identity.clone());
            }
            owner.insert(id, identity);
        }
    }
    if owner.is_empty() {
        return BTreeSet::new();
    }
    let missing = owner.values().cloned().collect::<BTreeSet<_>>();
    let mut referenced = BTreeSet::new();
    let edges = existing
        .get("links")
        .or_else(|| existing.get("edges"))
        .and_then(Value::as_array);
    for edge in edges.into_iter().flatten() {
        let edge_source = paths.identity(&text(edge, "source_file"));
        if missing.contains(&edge_source) {
            // The missing path emitted edges itself, so it was extracted.
            produced_by_itself.insert(edge_source);
            continue;
        }
        if !live_sources.contains(&edge_source) || rebuilt_sources.contains(&edge_source) {
            continue;
        }
        for endpoint in ["source", "target"] {
            if let Some(identity) = edge[endpoint].as_str().and_then(|id| owner.get(id)) {
                referenced.insert(identity.clone());
            }
        }
    }
    referenced
        .difference(&produced_by_itself)
        .cloned()
        .collect()
}
/// `_is_ast_tier` of a stored item after `_backfill_origin` (0.9.80): an
/// external import stub carries no tier, so a stamp an earlier run gave it
/// is not read.
fn tier_is_ast(item: &Value) -> bool {
    if build::is_external_stub(item) && item.get("_origin").is_some() {
        let mut bare = item.clone();
        if let Some(object) = bare.as_object_mut() {
            object.shift_remove("_origin");
        }
        return build::is_ast_tier(&bare);
    }
    build::is_ast_tier(item)
}
pub struct ReconcileOptions<'a> {
    pub out: &'a Path,
    pub project_root: &'a Path,
    pub watch_root: &'a Path,
    pub code_files: &'a [PathBuf],
    pub extract_targets: &'a [PathBuf],
    pub full: bool,
    pub deleted_paths: &'a mut BTreeSet<String>,
    pub deleted_identities: &'a BTreeSet<String>,
}
pub fn reconcile(
    existing: &Value,
    result: Value,
    o: &mut ReconcileOptions<'_>,
    ignored: impl FnMut(&Path, bool) -> bool,
    messages: &mut Vec<String>,
) -> Result<Value> {
    let plan = plan(existing, o, ignored);
    finish(plan, result, o, messages)
}
/// What a reconcile reads from the stored graph, taken by [`plan`] so the
/// stored document can be released before the new extraction is made: the
/// eviction decisions (the deleted paths and their messages) and, in stored
/// order, the records that survive unless the new extraction replaces them.
/// [`finish`] applies them with the new extraction.
pub struct Plan {
    sp: StoredSourcePaths,
    deleted_paths: Vec<String>,
    messages: Vec<String>,
    /// Stored nodes that pass every filter but the new extraction's ids.
    nodes: Vec<Value>,
    /// Stored edges that pass every filter but their endpoints' presence.
    edges: Vec<Value>,
    /// Stored hyperedges that pass every filter but the new hyperedge ids
    /// and their members' presence.
    hyper: Vec<Value>,
    /// `(source, target, relation)` of evicted stored edges whose
    /// `reverse_import` stamp moves onto the fresh edge (#4839).
    carried_reverse: BTreeSet<(String, String, String)>,
    /// Ids of stored semantic nodes with independent evidence that a fresh
    /// AST node may not take over (#4896).
    protected: HashSet<String>,
    /// Stored non-AST ids of the `<id>__ast_<hash>` form.
    stored_non_ast_aliases: HashSet<String>,
}
/// The part of [`reconcile`] that reads only the stored graph, the file
/// system and the ignore rules, never the new extraction: every filter is a
/// pure predicate of one record, so the ones that need the extraction are
/// applied by [`finish`] to what passes these, in the same order.
pub fn plan(
    existing: &Value,
    o: &ReconcileOptions<'_>,
    mut ignored: impl FnMut(&Path, bool) -> bool,
) -> Plan {
    let sp = StoredSourcePaths::new(existing, o.out, o.project_root, o.watch_root)
        .with_scan(o.code_files);
    let current = o
        .code_files
        .iter()
        .map(|p| sp.absolute_identity(&p.to_string_lossy(), o.project_root))
        .collect::<BTreeSet<_>>();
    let rebuilt = o
        .extract_targets
        .iter()
        .map(|p| sp.absolute_identity(&p.to_string_lossy(), o.project_root))
        .collect::<BTreeSet<_>>();
    // Re-extraction owns only a rebuilt source's AST-tier nodes (the node
    // filter below); adding rebuilt sources to this provenance-blind eviction
    // would drop the semantic nodes of the very file an incremental update
    // re-extracts (Graphify 0.9.101, #4896).
    let mut node_evicted = o.deleted_identities.clone();
    let mut edge_evicted = node_evicted.clone();
    let mut hyper_evicted = node_evicted.clone();
    let mut excluded = BTreeSet::new();
    let mut newly = BTreeSet::new();
    let (mut excluded_count, mut newly_count) = (0, 0);
    // Liveness and ignore status are per source file, not per node; memoise
    // them so a graph with thousands of nodes stats each file once.
    let mut file_state: HashMap<String, (bool, bool)> = HashMap::new();
    let mut deleted_paths = vec![];
    // Read once, on the first missing source that could be a placeholder.
    let mut unscanned_refs: Option<BTreeSet<String>> = None;
    let mut referenced_unscanned = |identity: &str| {
        unscanned_refs
            .get_or_insert_with(|| {
                referenced_unscanned_identities(
                    existing,
                    &sp,
                    o.out,
                    o.project_root,
                    &current,
                    &rebuilt,
                )
            })
            .contains(identity)
    };
    let existing_nodes: &[Value] = existing["nodes"].as_array().map_or(&[], Vec::as_slice);
    for n in existing_nodes {
        let sf = text(n, "source_file");
        if sf.is_empty() || remote(&sf) || !sp.in_watch(&sf) {
            continue;
        }
        let id = sp.identity(&sf);
        let (alive, is_ignored) = match file_state.get(&id) {
            Some(&state) => state,
            None => {
                let alive = Path::new(&id).exists();
                let is_ignored = alive && ignored(Path::new(&id), o.full);
                file_state.insert(id.clone(), (alive, is_ignored));
                (alive, is_ignored)
            }
        };
        if extract::get_extractor(Path::new(&sf)).is_none() {
            if !alive && referenced_unscanned(&id) {
                continue; // never scanned: a referrer's placeholder, not a deletion
            }
            if !alive || is_ignored {
                if is_ignored {
                    newly.insert(id.clone());
                    newly_count += 1;
                }
            } else {
                continue;
            }
        } else if !current.contains(&id) {
            if alive && !is_ignored {
                excluded.insert(id);
                excluded_count += 1;
                continue;
            }
            if !alive && referenced_unscanned(&id) {
                continue; // never scanned: a referrer's placeholder, not a deletion
            }
            if is_ignored {
                newly.insert(id.clone());
                newly_count += 1;
            }
        } else {
            continue;
        }
        let normalized = sp.normalize(&sf);
        if !normalized.is_empty() {
            deleted_paths.push(normalized);
        }
        if !id.is_empty() {
            node_evicted.insert(id.clone());
            edge_evicted.insert(id.clone());
            hyper_evicted.insert(id);
        }
    }
    // Edges and hyperedges may own a source with no node; positive exclusion
    // evidence evicts those records too (Graphify 0.9.101, #4912).
    let mut seen_record_sources: HashSet<String> = HashSet::new();
    let links = existing
        .get("links")
        .or_else(|| existing.get("edges"))
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let hyperedges = existing["hyperedges"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    for item in links.iter().chain(hyperedges) {
        let sf = text(item, "source_file");
        if !seen_record_sources.insert(sf.clone()) {
            continue;
        }
        if sf.is_empty() || remote(&sf) || !sp.in_watch(&sf) {
            continue;
        }
        let id = sp.identity(&sf);
        if id.is_empty() || current.contains(&id) {
            continue;
        }
        let (alive, is_ignored) = match file_state.get(&id) {
            Some(&state) => state,
            None => {
                let alive = Path::new(&id).exists();
                let is_ignored = alive && ignored(Path::new(&id), o.full);
                file_state.insert(id.clone(), (alive, is_ignored));
                (alive, is_ignored)
            }
        };
        if alive && is_ignored {
            edge_evicted.insert(id.clone());
            hyper_evicted.insert(id.clone());
            newly.insert(id);
        }
    }
    // Stored semantic nodes with evidence of their own that an AST id
    // collision must not replace (Graphify 0.9.101, #4896, #4927), and the
    // stored non-AST ids an `__ast_` successor could clash with.
    let mut last_by_id: HashMap<&str, &Value> = HashMap::new();
    for n in existing_nodes {
        if let Some(id) = n.get("id").and_then(Value::as_str) {
            last_by_id.insert(id, n);
        }
    }
    let mut protected = HashSet::new();
    let mut stored_non_ast_aliases = HashSet::new();
    for (id, n) in last_by_id {
        if build::is_ast_tier(n) {
            continue;
        }
        if id.contains("__ast_") {
            stored_non_ast_aliases.insert(id.to_owned());
        }
        if n.as_object()
            .is_some_and(crate::build_normalize::has_independent_semantic_evidence)
            && !sp.evicted(n, &node_evicted)
        {
            protected.insert(id.to_owned());
        }
    }
    let mut messages = vec![];
    if !newly.is_empty() {
        messages.push(format!(
            "pruned {newly_count} node(s) from {} newly-ignored file(s)",
            newly.len()
        ));
    }
    if !excluded.is_empty() {
        messages.push(format!("fail-closed: kept {excluded_count} node(s) from {} file(s) that left the scan corpus but still exist on disk and match no current ignore rule",excluded.len()));
    }
    let nodes = existing_nodes
        .iter()
        .filter(|n| {
            !(tier_is_ast(n)
                && ((text(n, "source_file").is_empty() && (o.full || o.code_files.is_empty()))
                    || (sp.evicted(n, &rebuilt) && !sp.evicted(n, &excluded))))
                && (!sp.evicted(n, &node_evicted) || sp.evicted(n, &excluded))
        })
        .cloned()
        .collect::<Vec<_>>();
    let edge_bucket = if existing.get("links").is_some() {
        "links"
    } else {
        "edges"
    };
    let edges = existing[edge_bucket]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| {
                    (!sp.evicted(e, &edge_evicted) || sp.evicted(e, &excluded))
                        && !(tier_is_ast(e) && sp.evicted(e, &rebuilt) && !sp.evicted(e, &excluded))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let hyper = existing["hyperedges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| !sp.evicted(e, &hyper_evicted) || sp.evicted(e, &excluded))
        .cloned()
        .collect::<Vec<_>>();
    // `reverse_import` stands for the target file's import back, which the
    // undirected build dropped in favour of this edge (Graphify 0.9.98,
    // #4839). That import is the target file's, so it goes when an edge from
    // that file would: the stamp is cleared then (re-extracting the file
    // stamps it again), and moved onto the fresh edge when only this edge's
    // own file was re-extracted.
    let mut edges = edges;
    let mut carried_reverse = BTreeSet::new();
    let stored_edges: &[Value] = existing[edge_bucket].as_array().map_or(&[], Vec::as_slice);
    let stamped = |e: &Value| e.get("reverse_import").is_some_and(build::truthy);
    if stored_edges.iter().any(stamped) {
        let node_files: HashMap<String, Value> = existing_nodes
            .iter()
            .map(|n| (text(n, "id"), n.get("source_file").cloned().unwrap_or(Value::Null)))
            .collect();
        let edge_kept = |e: &Value| {
            (!sp.evicted(e, &edge_evicted) || sp.evicted(e, &excluded))
                && !(tier_is_ast(e) && sp.evicted(e, &rebuilt) && !sp.evicted(e, &excluded))
        };
        let reverse_leg_evicted = |e: &Value| {
            let mut leg = e.clone();
            leg["source_file"] = node_files
                .get(&text(e, "target"))
                .cloned()
                .unwrap_or(Value::Null);
            !edge_kept(&leg)
        };
        for e in edges.iter_mut() {
            if stamped(e)
                && reverse_leg_evicted(e)
                && let Some(object) = e.as_object_mut()
            {
                object.shift_remove("reverse_import");
            }
        }
        for e in stored_edges {
            if stamped(e) && !edge_kept(e) && !reverse_leg_evicted(e) {
                carried_reverse.insert((text(e, "source"), text(e, "target"), text(e, "relation")));
            }
        }
    }
    Plan {
        sp,
        deleted_paths,
        messages,
        nodes,
        edges,
        hyper,
        carried_reverse,
        protected,
        stored_non_ast_aliases,
    }
}
/// The `_origin` a stored node has once upstream's `_backfill_origin` ran:
/// semantic unless stamped otherwise, AST-tier or an external stub.
fn stored_origin_is_semantic(n: &Value) -> bool {
    match n.get("_origin") {
        Some(origin) => origin == "semantic",
        None => !build::is_external_stub(n) && !build::is_ast_tier(n),
    }
}
/// The `__ast_` successor of every fresh node whose id a protected stored
/// semantic node holds, or why the collision cannot be rekeyed safely.
fn ast_collision_successors(
    result: &Value,
    protected: &HashSet<String>,
    stored_non_ast_aliases: &HashSet<String>,
) -> std::result::Result<BTreeMap<String, String>, String> {
    use sha2::{Digest, Sha256};
    let fresh_nodes: &[Value] = result["nodes"].as_array().map_or(&[], Vec::as_slice);
    let mut collisions = BTreeMap::new();
    let mut fresh_ids: Option<HashSet<&str>> = None;
    for fresh in fresh_nodes {
        let Some(nid) = fresh.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !protected.contains(nid) {
            continue;
        }
        if fresh.get("_origin").and_then(Value::as_str) != Some("ast") {
            return Err(format!(
                "Cannot safely reconcile same-ID semantic and non-AST node: {nid}"
            ));
        }
        let digest = format!("{:x}", Sha256::digest(nid.as_bytes()));
        let alias = format!("{nid}__ast_{}", &digest[..16]);
        let ids = fresh_ids.get_or_insert_with(|| {
            fresh_nodes
                .iter()
                .filter_map(|n| n.get("id").and_then(Value::as_str))
                .collect()
        });
        if ids.contains(alias.as_str()) || stored_non_ast_aliases.contains(&alias) {
            return Err(format!("AST ID collision cannot be safely rekeyed: {nid}"));
        }
        collisions.insert(nid.to_owned(), alias);
    }
    Ok(collisions)
}
/// Moves the fresh nodes, edges and hyperedge members onto their successors.
fn rekey_fresh(result: &mut Value, collisions: &BTreeMap<String, String>) {
    for node in result["nodes"].as_array_mut().into_iter().flatten() {
        if let Some(alias) = node.get("id").and_then(Value::as_str).and_then(|id| collisions.get(id)) {
            node["id"] = json!(alias);
        }
    }
    for edge in result["edges"].as_array_mut().into_iter().flatten() {
        for field in ["source", "target"] {
            if let Some(alias) = edge.get(field).and_then(Value::as_str).and_then(|id| collisions.get(id)) {
                edge[field] = json!(alias);
            }
        }
    }
    for hyperedge in result["hyperedges"].as_array_mut().into_iter().flatten() {
        for field in ["nodes", "members", "node_ids"] {
            if let Some(members) = hyperedge.get_mut(field).and_then(Value::as_array_mut) {
                for member in members.iter_mut() {
                    if let Some(alias) = member.as_str().and_then(|id| collisions.get(id)) {
                        *member = json!(alias);
                    }
                }
            }
        }
    }
}
/// [`reconcile`] of a [`plan`] with the new extraction `result`.
pub fn finish(
    plan: Plan,
    mut result: Value,
    o: &mut ReconcileOptions<'_>,
    messages: &mut Vec<String>,
) -> Result<Value> {
    // Upstream backfills every stored node/edge's `_origin` before filtering
    // (`_backfill_origin`). `is_ast_tier` reads `_origin` first and falls back
    // to the same heuristic that produced it, so backfilling only the records
    // that survive the filters ([`tier_is_ast`] reads a stub as unstamped)
    // yields identical output without cloning the whole graph.
    let tag_origin = build::backfill_origin;
    let Plan {
        sp,
        deleted_paths,
        messages: planned,
        nodes,
        edges,
        hyper,
        carried_reverse,
        protected,
        stored_non_ast_aliases,
    } = plan;
    if !carried_reverse.is_empty() {
        for edge in result["edges"].as_array_mut().into_iter().flatten() {
            let key = (text(edge, "source"), text(edge, "target"), text(edge, "relation"));
            if carried_reverse.contains(&key) {
                edge["reverse_import"] = json!(true);
            }
        }
    }
    o.deleted_paths.extend(deleted_paths);
    messages.extend(planned);
    // An extractor upgrade can mint an AST node with exactly the id of a
    // saved, independently evidenced semantic node (Graphify 0.9.101, #4896,
    // #4927). The semantic id stays; the fresh AST node moves to
    // `<id>__ast_<sha256(id)[:16]>`, the same successor for every record of
    // that id, and only the fresh edges and hyperedges follow it.
    if !protected.is_empty() {
        match ast_collision_successors(&result, &protected, &stored_non_ast_aliases) {
            Err(err) => {
                // Upstream falls back to the fresh extraction alone; the
                // write guard still checks it against the stored graph.
                messages.push(format!(
                    "reconcile of existing graph failed (ValueError: {err}); proceeding with fresh extraction only."
                ));
                return Ok(result);
            }
            Ok(collisions) if !collisions.is_empty() => {
                rekey_fresh(&mut result, &collisions);
                messages.push(format!(
                    "preserved {} independent semantic node(s) from AST ID collision",
                    collisions.len()
                ));
            }
            Ok(_) => {}
        }
    }
    // With nothing left to keep from the stored graph (a full rebuild of a
    // code graph), the new ids and endpoints that only filter those records
    // are not collected.
    let keeps = !(nodes.is_empty() && edges.is_empty() && hyper.is_empty());
    let mut preserved = vec![];
    let mut kept_edges = vec![];
    let mut allids = BTreeSet::new();
    let mut newids = BTreeSet::new();
    if keeps {
        newids = result["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|n| text(n, "id"))
            .collect::<BTreeSet<_>>();
        // A sourced semantic node keeps its place beside a fresh AST node of
        // the same id; the namespacing below separates or drops it
        // (Graphify 0.9.101).
        preserved = nodes
            .into_iter()
            .filter(|n| {
                !newids.contains(&text(n, "id"))
                    || (stored_origin_is_semantic(n)
                        && n.get("source_file").is_some_and(build::truthy))
            })
            .collect::<Vec<_>>();
        allids = newids
            .iter()
            .cloned()
            .chain(preserved.iter().map(|n| text(n, "id")))
            .collect::<BTreeSet<_>>();
        kept_edges = edges
            .into_iter()
            .filter(|e| allids.contains(&text(e, "source")) && allids.contains(&text(e, "target")))
            .collect::<Vec<_>>();
    }
    for item in preserved.iter_mut().chain(kept_edges.iter_mut()) {
        tag_origin(item);
    }
    let mut edges = reconcile_markdown(
        &mut result,
        kept_edges,
        &preserved,
        o.code_files,
        o.extract_targets,
        ReconcileContext {
            full: o.full,
            root: o.project_root,
            paths: &sp,
        },
    );
    let mut hyper = if hyper.is_empty() {
        hyper
    } else {
        let newhyper = result["hyperedges"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| text(e, "id"))
            .filter(|s| !s.is_empty())
            .collect::<BTreeSet<_>>();
        hyper
            .into_iter()
            .filter(|e| {
                !newhyper.contains(&text(e, "id"))
                    && e.get("nodes")
                        .or(e.get("members"))
                        .or(e.get("node_ids"))
                        .and_then(Value::as_array)
                        .is_none_or(|a| {
                            a.iter()
                                .all(|n| n.as_str().is_some_and(|s| allids.contains(s)))
                        })
            })
            .collect::<Vec<_>>()
    };
    // An external import stub exists only so an import edge has a declared
    // endpoint (#2873), and every write mints the ones still needed: one whose
    // last edge is gone is dropped, as a fresh build would not have it (0.9.80).
    if preserved.iter().any(build::is_external_stub) {
        let mut referenced = BTreeSet::new();
        for edge in result["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(&edges)
        {
            for endpoint in ["source", "target"] {
                if let Some(key) =
                    python_compat::scalar_key(edge.get(endpoint).unwrap_or(&Value::Null))
                {
                    referenced.insert(key);
                }
            }
        }
        for edge in hyper
            .iter()
            .chain(result["hyperedges"].as_array().into_iter().flatten())
        {
            let members = edge
                .get("nodes")
                .or_else(|| edge.get("members"))
                .or_else(|| edge.get("node_ids"));
            for member in members.and_then(Value::as_array).into_iter().flatten() {
                if let Some(key) = python_compat::scalar_key(member) {
                    referenced.insert(key);
                }
            }
        }
        preserved.retain(|node| {
            !build::is_external_stub(node)
                || python_compat::scalar_key(node.get("id").unwrap_or(&Value::Null))
                    .is_some_and(|id| referenced.contains(&id))
        });
    }
    for v in preserved.iter_mut().chain(&mut edges).chain(&mut hyper) {
        sp.rebase_preserved(v);
    }
    let shares_ids = preserved.iter().any(|n| newids.contains(&text(n, "id")))
        || preserved.iter().any(crate::dedup::is_reference_stub);
    let mut ns = take_items(&mut result, "nodes");
    ns.extend(preserved);
    let mut es = take_items(&mut result, "edges");
    es.extend(edges);
    let mut hs = take_items(&mut result, "hyperedges");
    hs.extend(hyper);
    if shares_ids {
        // Saved semantic evidence from another file is kept under a
        // namespaced id; an unattributed or same-source copy must not
        // overwrite a fresh AST fact (Graphify 0.9.101).
        crate::dedup::namespace_saved_semantic_collisions(
            &mut ns,
            &mut es,
            Some(hs.as_mut_slice()),
            o.project_root,
        );
        ns.retain(|n| !newids.contains(&text(n, "id")) || build::is_ast_tier(n));
    }
    // Moved in rather than copied through the serializer, in `json!`'s key order.
    let mut out = serde_json::Map::new();
    out.insert("nodes".into(), Value::Array(ns));
    out.insert("edges".into(), Value::Array(es));
    out.insert("hyperedges".into(), Value::Array(hs));
    out.insert("input_tokens".into(), json!(0));
    out.insert("output_tokens".into(), json!(0));
    Ok(Value::Object(out))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect_ignore::{EncodingWarnings, IgnoreState};
    use std::{fs, process::Command};

    /// A main checkout `app` with `a.py` and a linked worktree
    /// `app-feature` beside it, both under the scan root.
    fn workspace(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let main = tmp.join("app");
        fs::create_dir_all(&main).unwrap();
        let source = main.join("a.py");
        fs::write(&source, "def answer(): return 42\n").unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap()
                .status;
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-qm",
            "fixture",
        ]);
        let linked = tmp.join("app-feature");
        let linked_arg = linked.to_string_lossy().into_owned();
        git(&["worktree", "add", "-q", "-b", "fixture-linked", &linked_arg]);
        let linked_source = linked.join("a.py");
        (source, linked_source, linked)
    }

    fn ignored_under(root: &Path, path: &Path) -> bool {
        let mut warnings = EncodingWarnings::default();
        let mut rules = IgnoreState::new(root, &[], true, &mut warnings).unwrap();
        rules.predicate(path, &mut warnings).unwrap()
    }

    // Graphify 0.9.101, #4912: test_linked_worktree_reconcile.py.
    #[test]
    fn predicate_matches_scan_for_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (_, linked_source, linked) = workspace(&root);
        assert!(ignored_under(&root, &linked_source));
        // Scanning the linked copy itself is intentional.
        assert!(!ignored_under(&linked, &linked_source));
    }

    #[test]
    fn reconcile_prunes_records_owned_by_a_linked_copy() {
        for linked_node in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(tmp.path()).unwrap();
            let (source, linked_source, _) = workspace(&root);
            let control = root.join("unexplained.txt");
            fs::write(&control, "still here").unwrap();
            let (s, l, c) = (
                source.to_string_lossy().into_owned(),
                linked_source.to_string_lossy().into_owned(),
                control.to_string_lossy().into_owned(),
            );
            let fresh = json!([
                {"id": "a", "label": "a", "source_file": s, "_origin": "ast"},
                {"id": "b", "label": "b", "source_file": s, "_origin": "ast"},
            ]);
            let mut nodes = fresh.as_array().unwrap().clone();
            if linked_node {
                nodes.push(json!({"id": "linked", "label": "linked", "source_file": l, "_origin": "ast"}));
            }
            nodes.push(json!({"id": "control", "label": "control", "source_file": c, "_origin": "semantic"}));
            let existing = json!({
                "nodes": nodes,
                "links": [
                    {"source": "a", "target": "b", "relation": "references", "source_file": l, "_origin": "semantic"},
                    {"source": "b", "target": "a", "relation": "references", "source_file": c, "_origin": "semantic"},
                ],
                "hyperedges": [
                    {"id": "linked-edge", "nodes": ["a", "b"], "source_file": l},
                    {"id": "control-edge", "nodes": ["a", "b"], "source_file": c},
                ],
            });
            let out = root.join("graphify-out");
            fs::create_dir_all(&out).unwrap();
            let code = vec![source.clone()];
            let mut deleted = BTreeSet::new();
            let ids = BTreeSet::new();
            let mut warnings = EncodingWarnings::default();
            let mut rules = IgnoreState::new(&root, &[], true, &mut warnings).unwrap();
            let mut messages = vec![];
            let result = reconcile(
                &existing,
                json!({"nodes": fresh, "edges": [], "hyperedges": []}),
                &mut ReconcileOptions {
                    out: &out,
                    project_root: &root,
                    watch_root: &root,
                    code_files: &code,
                    extract_targets: &code,
                    full: true,
                    deleted_paths: &mut deleted,
                    deleted_identities: &ids,
                },
                |p, _| rules.predicate(p, &mut warnings).unwrap_or(false),
                &mut messages,
            )
            .unwrap();
            let node_ids: BTreeSet<_> = result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["id"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(node_ids, BTreeSet::from(["a".into(), "b".into(), "control".into()]));
            let edge_files: Vec<_> = result["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["source_file"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(edge_files, vec!["unexplained.txt".to_owned()]);
            let hyper: Vec<_> = result["hyperedges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["id"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(hyper, vec!["control-edge".to_owned()]);
        }
    }

    fn reconcile_plain(
        root: &Path,
        existing: Value,
        fresh: Value,
        code: &[PathBuf],
        full: bool,
    ) -> (Value, Vec<String>) {
        let out = root.join("graphify-out");
        fs::create_dir_all(&out).unwrap();
        let mut deleted = BTreeSet::new();
        let ids = BTreeSet::new();
        let mut messages = vec![];
        let result = reconcile(
            &existing,
            fresh,
            &mut ReconcileOptions {
                out: &out,
                project_root: root,
                watch_root: root,
                code_files: code,
                extract_targets: code,
                full,
                deleted_paths: &mut deleted,
                deleted_identities: &ids,
            },
            |_, _| false,
            &mut messages,
        )
        .unwrap();
        (result, messages)
    }

    // Graphify 0.9.101, #4896: test_sourceless_ast_collision_keeps_semantic_tier.
    #[test]
    fn sourceless_ast_collision_keeps_semantic_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::write(root.join("notes.md"), "## Record\n").unwrap();
        let existing = json!({
            "nodes": [{"id": "memory", "label": "Memory", "_origin": "semantic",
                       "file_type": "document", "source_file": "notes.md",
                       "rationale": "Reviewed evidence"}],
            "links": [], "hyperedges": [],
        });
        let fresh = json!({"nodes": [{"id": "memory", "_origin": "ast", "label": "Memory",
            "file_type": "code", "source_file": "", "source_location": ""}],
            "edges": [], "hyperedges": []});
        let (result, messages) = reconcile_plain(&root, existing, fresh, &[], true);
        let nodes = result["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2, "{nodes:?}");
        let memory = nodes.iter().find(|n| n["id"] == "memory").unwrap();
        assert_eq!(memory["rationale"], "Reviewed evidence");
        assert_eq!(memory["_origin"], "semantic");
        assert!(nodes.iter().any(|n| n["id"]
            .as_str()
            .unwrap()
            .starts_with("memory__ast_")));
        assert!(messages.iter().any(|m| m.contains("independent semantic node")));
    }

    // Graphify 0.9.101, #4896 and #4927:
    // test_multiple_ast_records_colliding_with_reviewed_semantic_id.
    #[test]
    fn multiple_ast_records_colliding_with_a_reviewed_semantic_id() {
        for full in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(tmp.path()).unwrap();
            fs::write(root.join("notes.md"), "## Reviewed node\n").unwrap();
            let (a, b) = (root.join("a.py"), root.join("b.py"));
            fs::write(&a, "def one(): pass\n").unwrap();
            fs::write(&b, "def two(): pass\n").unwrap();
            let existing = json!({
                "nodes": [{"id": "shared_registry", "label": "Registry", "_origin": "semantic",
                           "file_type": "document", "source_file": "notes.md",
                           "rationale": "Original reviewed registry"}],
                "links": [], "hyperedges": [],
            });
            let records = [&a, &b]
                .iter()
                .map(|p| json!({"id": "shared_registry", "_origin": "ast", "label": "Registry",
                    "file_type": "code", "source_file": p.to_string_lossy(),
                    "source_location": "L1"}))
                .collect::<Vec<_>>();
            let fresh = json!({"nodes": records, "edges": [
                {"source": "shared_registry", "target": "ast_target",
                 "source_file": a.to_string_lossy(), "_origin": "ast"}], "hyperedges": []});
            let (result, _) = reconcile_plain(&root, existing, fresh, &[a.clone(), b.clone()], full);
            let nodes = result["nodes"].as_array().unwrap();
            let semantic = nodes.iter().find(|n| n["id"] == "shared_registry").unwrap();
            assert_eq!(semantic["rationale"], "Original reviewed registry");
            let aliases = nodes
                .iter()
                .filter(|n| n["_origin"] == "ast" && n["label"] == "Registry")
                .map(|n| n["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(aliases.len(), 2, "full {full}");
            assert_eq!(aliases[0], aliases[1]);
            assert!(aliases[0].starts_with("shared_registry__ast_"));
            let edge = &result["edges"][0];
            assert_eq!(edge["source"], json!(aliases[0]));
        }
    }
}
