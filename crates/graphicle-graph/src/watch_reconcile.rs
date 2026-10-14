// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
    collections::{BTreeMap, BTreeSet, HashMap},
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
    mut result: Value,
    o: &mut ReconcileOptions<'_>,
    mut ignored: impl FnMut(&Path, bool) -> bool,
    messages: &mut Vec<String>,
) -> Result<Value> {
    // Upstream backfills every stored node/edge's `_origin` before filtering
    // (`_backfill_origin`). `is_ast_tier` reads `_origin` first and falls back
    // to the same heuristic that produced it, so backfilling only the records
    // that survive the filters ([`tier_is_ast`] reads a stub as unstamped)
    // yields identical output without cloning the whole graph.
    let tag_origin = build::backfill_origin;
    let sp = StoredSourcePaths::new(existing, o.out, o.project_root, o.watch_root);
    let newids = result["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| text(n, "id"))
        .collect::<BTreeSet<_>>();
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
    let mut node_evicted = o.deleted_identities.clone();
    let mut edge_evicted = node_evicted.clone();
    let mut hyper_evicted = node_evicted.clone();
    if !o.full {
        node_evicted.extend(rebuilt.iter().cloned());
    }
    let mut excluded = BTreeSet::new();
    let mut newly = BTreeSet::new();
    let (mut excluded_count, mut newly_count) = (0, 0);
    // Liveness and ignore status are per source file, not per node; memoise
    // them so a graph with thousands of nodes stats each file once.
    let mut file_state: HashMap<String, (bool, bool)> = HashMap::new();
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
            o.deleted_paths.insert(normalized);
        }
        if !id.is_empty() {
            node_evicted.insert(id.clone());
            edge_evicted.insert(id.clone());
            hyper_evicted.insert(id);
        }
    }
    if !newly.is_empty() {
        messages.push(format!(
            "pruned {newly_count} node(s) from {} newly-ignored file(s)",
            newly.len()
        ));
    }
    if !excluded.is_empty() {
        messages.push(format!("fail-closed: kept {excluded_count} node(s) from {} file(s) that left the scan corpus but still exist on disk and match no current ignore rule",excluded.len()));
    }
    let mut preserved = existing_nodes
        .iter()
        .filter(|n| {
            !newids.contains(&text(n, "id"))
                && !(tier_is_ast(n)
                    && ((text(n, "source_file").is_empty() && (o.full || o.code_files.is_empty()))
                        || (o.full && sp.evicted(n, &rebuilt) && !sp.evicted(n, &excluded))))
                && (!sp.evicted(n, &node_evicted) || sp.evicted(n, &excluded))
        })
        .cloned()
        .collect::<Vec<_>>();
    let allids = newids
        .into_iter()
        .chain(preserved.iter().map(|n| text(n, "id")))
        .collect::<BTreeSet<_>>();
    let edge_bucket = if existing.get("links").is_some() {
        "links"
    } else {
        "edges"
    };
    let mut edges = existing[edge_bucket]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| {
                    allids.contains(&text(e, "source"))
                        && allids.contains(&text(e, "target"))
                        && (!sp.evicted(e, &edge_evicted) || sp.evicted(e, &excluded))
                        && !(tier_is_ast(e) && sp.evicted(e, &rebuilt) && !sp.evicted(e, &excluded))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for item in preserved.iter_mut().chain(edges.iter_mut()) {
        tag_origin(item);
    }
    let edges = edges;
    let mut edges = reconcile_markdown(
        &mut result,
        edges,
        &preserved,
        o.code_files,
        o.extract_targets,
        ReconcileContext {
            full: o.full,
            root: o.project_root,
            paths: &sp,
        },
    );
    let newhyper = result["hyperedges"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| text(e, "id"))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>();
    let mut hyper = existing["hyperedges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| {
            !newhyper.contains(&text(e, "id"))
                && (!sp.evicted(e, &hyper_evicted) || sp.evicted(e, &excluded))
                && e.get("nodes")
                    .or(e.get("members"))
                    .or(e.get("node_ids"))
                    .and_then(Value::as_array)
                    .is_none_or(|a| {
                        a.iter()
                            .all(|n| n.as_str().is_some_and(|s| allids.contains(s)))
                    })
        })
        .cloned()
        .collect::<Vec<_>>();
    // An external import stub exists only so an import edge has a declared
    // endpoint (#2873), and every write mints the ones still needed: one whose
    // last edge is gone is dropped, as a fresh build would not have it (0.9.80).
    let mut referenced = BTreeSet::new();
    for edge in result["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(&edges)
    {
        for endpoint in ["source", "target"] {
            if let Some(key) = python_compat::scalar_key(edge.get(endpoint).unwrap_or(&Value::Null))
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
    for v in preserved.iter_mut().chain(&mut edges).chain(&mut hyper) {
        sp.rebase_preserved(v);
    }
    let mut ns = take_items(&mut result, "nodes");
    ns.extend(preserved);
    let mut es = take_items(&mut result, "edges");
    es.extend(edges);
    let mut hs = take_items(&mut result, "hyperedges");
    hs.extend(hyper);
    // Moved in rather than copied through the serializer, in `json!`'s key order.
    let mut out = serde_json::Map::new();
    out.insert("nodes".into(), Value::Array(ns));
    out.insert("edges".into(), Value::Array(es));
    out.insert("hyperedges".into(), Value::Array(hs));
    out.insert("input_tokens".into(), json!(0));
    out.insert("output_tokens".into(), json!(0));
    Ok(Value::Object(out))
}
