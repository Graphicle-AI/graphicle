// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py
//! Schema recovery, portable source identities, and legacy graph identifiers.
use crate::{
    Result, dedup, dedup_strings,
    extractors::{
        base,
        resolution::{field, map_field},
    },
    fsutil, ids, paths,
    python_compat::{self, scalar_key},
    record::Record,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
};
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
pub fn is_ast_tier(item: &Value) -> bool {
    if let Some(origin) = field(item, "_origin").filter(|v| !v.is_null()) {
        return origin == "ast";
    }
    field(item, "source_location")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            s.strip_prefix('L')
                .and_then(|s| s.chars().next())
                .is_some_and(dedup_strings::decimal)
        })
}
/// `_is_external_stub` (0.9.80): a node minted for an import of an external
/// module. It is derived from the import edges that point at it and minted
/// again on every write, so it belongs to no extraction tier.
pub fn is_external_stub(item: &Value) -> bool {
    field(item, "external") == Some(&Value::Bool(true))
        && field(item, "type").and_then(Value::as_str) == Some("external")
        && !field(item, "source_file").is_some_and(truthy)
}
/// `_backfill_origin` (0.9.80, #2334): stamp a loaded legacy item's tier via
/// the [`is_ast_tier`] shape fallback. An external stub is never stamped and
/// loses a stamp an earlier run gave it: read as `semantic` it outlived its
/// last import edge.
pub fn backfill_origin(item: &mut Value) {
    if !item.is_object() {
        return;
    }
    if is_external_stub(item) {
        if let Some(object) = item.as_object_mut() {
            object.shift_remove("_origin");
        }
        return;
    }
    if item.get("_origin").is_none() {
        let origin = if is_ast_tier(item) { "ast" } else { "semantic" };
        item["_origin"] = json!(origin);
    }
}
/// `_execution_wins` (0.9.80, #3159): two import edges of one file pair that
/// collapse to one stay `type_only` only if both are; otherwise the marker of
/// one side would stand for a runtime import on the other and hide its cycle.
pub fn execution_wins(a: &mut Value, b: &mut Value) {
    if field(a, "type_only").is_some_and(truthy) != field(b, "type_only").is_some_and(truthy) {
        for edge in [a, b] {
            if let Some(object) = edge.as_object_mut() {
                object.shift_remove("type_only");
            }
        }
    }
}
/// `_lost_saved_semantic_nodes` (0.9.87/0.9.88, #4115/#2229/#4389): saved
/// semantic nodes whose ids are absent from the new graph. A remapped legacy
/// id (stem rewrite, `_doc` twin fold) is the same node, an external stub is
/// rebuilt from this run's import edges, a node of a `deleted_sources` file
/// may go, and a semantic ghost folded into its AST twin at the same
/// `(source_file, label)` survives as that twin.
pub fn lost_saved_semantic_nodes(
    saved_nodes: &[Value],
    new_nodes: &[Value],
    deleted_sources: &[String],
) -> Vec<Value> {
    let norm = |source: &str| source.replace('\\', "/");
    let deleted: HashSet<String> = deleted_sources
        .iter()
        .filter(|source| !source.is_empty())
        .map(|source| norm(source))
        .collect();
    let new_ids: BTreeSet<python_compat::ScalarKey> = new_nodes
        .iter()
        .filter(|node| node.is_object())
        .filter_map(|node| scalar_key(field(node, "id").unwrap_or(&Value::Null)))
        .collect();
    let label_of = |node: &Value| {
        field(node, "label")
            .map(python_compat::string)
            .unwrap_or_default()
            .trim_matches(dedup_strings::whitespace)
            .to_owned()
    };
    let mut ast_locations = HashSet::new();
    let mut ast_collisions = HashSet::new();
    for node in new_nodes {
        if !node.is_object() || field(node, "_origin").is_none_or(|origin| origin != "ast") {
            continue;
        }
        let Some(source) = field(node, "source_file")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = label_of(node);
        if label.is_empty() {
            continue;
        }
        let key = (norm(source), label);
        if !ast_locations.insert(key.clone()) {
            ast_collisions.insert(key);
        }
    }
    let stem_remap = semantic_id_remap(saved_nodes, None);
    let rewritten: Vec<Value> = saved_nodes
        .iter()
        .map(
            |node| match node["id"].as_str().and_then(|id| stem_remap.get(id)) {
                Some(new) if node.is_object() => {
                    let mut node = node.clone();
                    node["id"] = json!(new);
                    node
                }
                _ => node.clone(),
            },
        )
        .collect();
    let doc_remap = doc_twin_remap(&rewritten);
    let renamed_present = |id: &str| {
        let renamed = stem_remap.get(id).map_or(id, String::as_str);
        let renamed = doc_remap.get(renamed).map_or(renamed, String::as_str);
        new_ids.contains(&python_compat::ScalarKey::Text(renamed.to_owned()))
    };
    let mut lost = Vec::new();
    for node in saved_nodes {
        if !node.is_object() || is_ast_tier(node) {
            continue;
        }
        if field(node, "external") == Some(&Value::Bool(true))
            || field(node, "type").and_then(Value::as_str) == Some("external")
        {
            continue;
        }
        let id = field(node, "id").unwrap_or(&Value::Null);
        if scalar_key(id).is_some_and(|key| new_ids.contains(&key)) {
            continue;
        }
        if id.as_str().is_some_and(renamed_present) {
            continue;
        }
        let source = field(node, "source_file")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(norm);
        let label = label_of(node);
        if let Some(source) = &source
            && !label.is_empty()
        {
            let key = (source.clone(), label);
            if ast_locations.contains(&key) && !ast_collisions.contains(&key) {
                continue;
            }
        }
        if source
            .as_ref()
            .is_some_and(|source| deleted.contains(source))
        {
            continue;
        }
        lost.push(node.clone());
    }
    lost
}
/// Fold legacy node field aliases onto canonical keys (#2194, 0.9.88 #4391):
/// `name` -> `label`, and the first non-blank of `file`, `path`, `filepath`
/// -> `source_file`. A canonical field that already holds a value wins.
pub fn fold_node_aliases(node: &mut Value) {
    let Some(n) = node.as_object_mut() else {
        return;
    };
    if !map_field(n, "label").is_some_and(truthy)
        && map_field(n, "name")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    {
        let value = n.shift_remove("name").unwrap();
        n.insert("label".into(), value);
    }
    if !map_field(n, "source_file").is_some_and(truthy) {
        for alias in ["file", "path", "filepath"] {
            if map_field(n, alias)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim_matches(dedup_strings::whitespace).is_empty())
            {
                let value = n.shift_remove(alias).unwrap();
                n.insert("source_file".into(), value);
                break;
            }
        }
    }
}
pub fn fold_edge_aliases(edge: &mut Value) {
    if let Some(e) = edge.as_object_mut() {
        fold_edge_record_aliases(e);
    }
}
/// [`fold_edge_aliases`] on an edge record of either form.
pub(crate) fn fold_edge_record_aliases<R: Record>(e: &mut R) {
    for alias in ["type", "relationship"] {
        if !e.get("relation").is_some_and(truthy)
            && e.get(alias)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        {
            let v = e.take_key(alias).unwrap();
            e.put("relation", v);
        }
    }
    if let Some(v) = e.get("confidence").filter(|v| v.is_number()).cloned() {
        if e.get("confidence_score").is_none_or(Value::is_null) {
            e.put("confidence_score", json!(v.as_f64().unwrap_or(1.0)));
        }
        e.put("confidence", json!("INFERRED"));
    }
    if !e.get("confidence").is_some_and(truthy)
        && e.get("confidence_score").is_some_and(|v| !v.is_null())
    {
        e.put("confidence", json!("INFERRED"));
    }
}
/// The edge part of [`coerce_non_string_ids`], on an edge record of either
/// form: a numeric endpoint becomes its text, in place.
pub(crate) fn coerce_edge_record_ids<R: Record>(e: &mut R) {
    for key in ["source", "target", "from", "to"] {
        if let Some(v) = e.get(key).filter(|v| v.is_number()) {
            let text = json!(python_compat::string(v));
            e.put(key, text);
        }
    }
}
pub fn coerce_id(value: &Value) -> Value {
    if value.is_number() {
        json!(python_compat::string(value))
    } else {
        value.clone()
    }
}
/// Rewrite `value` in place if it is a number; strings need no copy.
fn coerce_in_place(value: &mut Value) {
    if value.is_number() {
        *value = json!(python_compat::string(value));
    }
}
pub fn coerce_non_string_ids(extraction: &mut Value) {
    for n in extraction
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(v) = n.get_mut("id") {
            coerce_in_place(v);
        }
    }
    for e in extraction
        .get_mut("edges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        // Ids are usually already strings; peek at the keys present instead
        // of probing four names per edge.
        let Some(object) = e.as_object_mut() else {
            continue;
        };
        for (key, v) in object.iter_mut() {
            if matches!(key.as_str(), "source" | "target" | "from" | "to") {
                coerce_in_place(v);
            }
        }
    }
    for he in extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for v in he
            .get_mut("nodes")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            coerce_in_place(v);
        }
    }
}
pub fn norm_source_file(value: &Value, root: Option<&Path>) -> Result<Value> {
    if !truthy(value) {
        return Ok(value.clone());
    }
    let mut p = value
        .as_str()
        .ok_or("Expected source_file string")?
        .replace('\\', "/");
    if paths::is_absolute_any_platform(Some(&p))
        && let Some(root) = root
    {
        if let Ok(relative) = Path::new(&p).strip_prefix(root) {
            p = fsutil::path_string(relative);
        } else if let (Ok(path), Ok(root)) = (
            fsutil::resolve_missing(Path::new(&p)),
            fsutil::resolve_missing(root),
        ) && let Ok(relative) = path.strip_prefix(root)
        {
            p = fsutil::path_string(relative);
        }
    }
    Ok(json!(p))
}
pub fn norm_path(value: &str, root: Option<&Path>) -> String {
    norm_source_file(&json!(value), root)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| value.into())
}
pub fn abs_identity(value: &Value, root: Option<&Path>) -> Result<Option<String>> {
    if !truthy(value) {
        return Ok(None);
    }
    let p = value
        .as_str()
        .ok_or("Expected source_file string")?
        .replace('\\', "/");
    let path = if !paths::is_absolute_any_platform(Some(&p)) {
        root.map_or_else(|| PathBuf::from(&p), |r| r.join(&p))
    } else {
        PathBuf::from(&p)
    };
    Ok(Some(fsutil::path_string(
        &fsutil::resolve_missing(&path).unwrap_or(path),
    )))
}
pub fn shortest_unique_suffix(source: &str, sources: &BTreeSet<String>) -> String {
    let normalized = source.replace('\\', "/");
    let parts: Vec<_> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    let others: Vec<Vec<String>> = sources
        .iter()
        .filter(|s| *s != source)
        .map(|s| {
            s.replace('\\', "/")
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect();
    for k in 1..=parts.len() {
        let suffix = &parts[parts.len() - k..];
        if others
            .iter()
            .all(|o| o[o.len().saturating_sub(k)..] != *suffix)
        {
            return suffix.join("/");
        }
    }
    parts.join("/")
}
pub fn file_label_reassignments(items: &[(Value, Value, Value)]) -> Vec<(Value, String)> {
    let mut groups: Vec<Vec<(Value, String)>> = Vec::new();
    let mut indices = BTreeMap::new();
    for (key, label, sf) in items {
        if crate::build_labels::is_file_node_label(label, sf) {
            let sf = python_compat::string(sf);
            let norm = sf.replace('\\', "/");
            let base = norm.rsplit('/').next().unwrap_or("").to_owned();
            let next = groups.len();
            let g = *indices.entry(base).or_insert_with(|| {
                groups.push(Vec::new());
                next
            });
            groups[g].push((key.clone(), sf));
        }
    }
    let mut out = Vec::new();
    for group in groups {
        let distinct: BTreeSet<_> = group.iter().map(|(_, s)| s.clone()).collect();
        if distinct.len() < 2 {
            continue;
        }
        let suffixes = shortest_unique_suffixes(&distinct);
        for (key, sf) in group {
            let suffix = suffixes[&sf].clone();
            out.push((key, suffix));
        }
    }
    out
}
/// [`shortest_unique_suffix`] of every member of `sources` against the rest,
/// splitting each path once instead of once per comparison (a large group,
/// such as every `__init__.py` of a corpus, is otherwise quadratic in splits).
fn shortest_unique_suffixes(sources: &BTreeSet<String>) -> BTreeMap<&String, String> {
    let split = sources
        .iter()
        .map(|s| {
            s.replace('\\', "/")
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let suffix_of = |i: usize| {
        let parts = &split[i];
        for k in 1..=parts.len() {
            let suffix = &parts[parts.len() - k..];
            if split
                .iter()
                .enumerate()
                .all(|(j, o)| j == i || o[o.len().saturating_sub(k)..] != *suffix)
            {
                return suffix.join("/");
            }
        }
        parts.join("/")
    };
    let indices = (0..split.len()).collect::<Vec<_>>();
    let workers = if split.len() >= 64 {
        crate::par::workers(None)
    } else {
        1
    };
    let suffixes = crate::par::map(&indices, workers, |&i| suffix_of(i));
    sources.iter().zip(suffixes).collect()
}
pub fn disambiguate_file_labels_in_nodes(nodes: &mut [Value]) {
    let items: Vec<_> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.is_object())
        .map(|(i, n)| (json!(i), n["label"].clone(), n["source_file"].clone()))
        .collect();
    for (i, label) in file_label_reassignments(&items) {
        nodes[i.as_u64().unwrap() as usize]["label"] = json!(label);
    }
}
pub fn infer_merge_root(path: &Path, output_name: &str) -> Option<PathBuf> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let marker = parent.join(".graphicle_root");
    if marker.exists()
        && let Ok(text) = std::fs::read_to_string(&marker)
    {
        let text = text
            .trim_start_matches('\u{feff}')
            .trim_matches(dedup_strings::whitespace);
        if !text.is_empty() && Path::new(text).is_dir() {
            return fsutil::resolve_missing(Path::new(text)).ok();
        }
    }
    if parent.file_name().is_some_and(|n| n == output_name)
        || marker.exists()
        || parent.join("manifest.json").exists()
    {
        return fsutil::resolve_missing(parent.parent().unwrap_or(Path::new("."))).ok();
    }
    None
}
pub fn build_prune_sets(
    sources: &[String],
    root: Option<&Path>,
    new_sources: &BTreeSet<String>,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut set = BTreeMap::new();
    let mut abs = BTreeMap::new();
    for p in sources {
        if p.is_empty() {
            continue;
        }
        set.entry(p.clone()).or_insert_with(|| p.clone());
        let norm = norm_path(p, root);
        if !norm.is_empty() {
            set.entry(norm).or_insert_with(|| p.clone());
        }
        if let Some(a) = abs_identity(&json!(p), root)? {
            abs.entry(a).or_insert_with(|| p.clone());
        }
    }
    for s in new_sources {
        set.remove(s);
        if let Some(a) = abs_identity(&json!(s), root)? {
            abs.remove(&a);
        }
    }
    Ok((set, abs))
}
pub fn derive_prune_root(sources: &[String], stored: &BTreeSet<String>) -> Option<String> {
    let relative: Vec<_> = stored
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.replace('\\', "/"))
        .filter(|s| !paths::is_absolute_any_platform(Some(s)))
        .collect();
    let mut roots = BTreeSet::new();
    for p in sources {
        let p = p.replace('\\', "/");
        if !paths::is_absolute_any_platform(Some(&p)) {
            continue;
        }
        let hits: BTreeSet<_> = relative
            .iter()
            .filter(|s| p.ends_with(&format!("/{s}")))
            .map(|s| p[..p.len() - s.len() - 1].to_owned())
            .collect();
        if hits.len() > 1 {
            return None;
        }
        roots.extend(hits);
    }
    if roots.len() == 1 {
        roots.into_iter().next()
    } else {
        None
    }
}
pub fn dedupe_nodes(nodes: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut indices = BTreeMap::new();
    for n in nodes {
        let nid = &n["id"];
        if nid.is_null() {
            continue;
        }
        let key = scalar_key(nid).ok_or("Unhashable node ID")?;
        if let Some(&i) = indices.get(&key) {
            out[i] = n.clone();
        } else {
            indices.insert(key, out.len());
            out.push(n.clone());
        }
    }
    Ok(out)
}
/// Collapse exact parallel edges by `(source, target, relation)`, keeping the
/// first occurrence; its `type_only` marker survives only if every folded
/// duplicate carries one too ([`execution_wins`], 0.9.80).
pub fn dedupe_edges(edges: &[Value]) -> Result<Vec<Value>> {
    let mut seen = BTreeMap::new();
    let mut out: Vec<Value> = Vec::new();
    for e in edges {
        let key = ["source", "target", "relation"]
            .iter()
            .map(|k| scalar_key(&e[k]).ok_or("Unhashable edge identifier"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(&index) = seen.get(&key) {
            execution_wins(&mut out[index], &mut e.clone());
            continue;
        }
        seen.insert(key, out.len());
        out.push(e.clone());
    }
    Ok(out)
}
pub fn old_file_stems(path: &Path) -> Vec<String> {
    let stem = base::file_stem(Path::new(path.file_name().unwrap_or_default()));
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    let mut out = Vec::new();
    if !parent.is_empty() && parent != "." {
        out.push(ids::make_id([format!("{parent}.{stem}")]));
    }
    out.push(ids::make_id([stem]));
    let mut seen = HashSet::new();
    out.retain(|s| !s.is_empty() && seen.insert(s.clone()));
    out
}
pub fn semantic_id_remap(nodes: &[Value], root: Option<&Path>) -> BTreeMap<String, String> {
    let mut remap = BTreeMap::new();
    for node in nodes {
        if !node.is_object() || is_ast_tier(node) {
            continue;
        }
        let Some(nid) = node["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !truthy(&node["source_file"]) {
            continue;
        }
        let source = python_compat::string(&node["source_file"]);
        let normalized = norm_path(&source, root);
        let rel = Path::new(&normalized);
        if paths::is_absolute_any_platform(Some(&normalized)) || rel.file_name().is_none() {
            continue;
        }
        let stem = ids::make_id([base::file_stem(rel)]);
        if stem.is_empty() {
            continue;
        }
        let normalized_id = ids::normalize_id(nid);
        if normalized_id == stem || normalized_id.starts_with(&format!("{stem}_")) {
            continue;
        }
        let mut forms = old_file_stems(rel);
        let raw = source.replace('\\', "/");
        if raw != normalized && paths::is_absolute_any_platform(Some(&raw)) {
            let old = ids::make_id([base::file_stem(Path::new(&raw))]);
            if !old.is_empty() && old != stem && !forms.contains(&old) {
                forms.insert(0, old);
            }
        }
        for old in forms {
            if old == stem {
                continue;
            }
            let new = if normalized_id == old {
                Some(stem.clone())
            } else {
                normalized_id
                    .strip_prefix(&format!("{old}_"))
                    .map(|entity| ids::make_id([stem.as_str(), entity]))
            };
            if let Some(new) = new {
                if new != nid {
                    remap.insert(nid.into(), new);
                }
                break;
            }
        }
    }
    remap
}
pub fn has_global_id(node: &Value) -> bool {
    matches!(
        node["metadata"]["mcp_kind"].as_str(),
        Some("mcp_command" | "mcp_package" | "env_var")
    )
}
pub fn graph_has_legacy_ids(nodes: &[Value], root: Option<&Path>, sample: i64) -> bool {
    let mut checked = 0;
    for node in nodes {
        if !node.is_object() || node["source_location"] != "L1" || has_global_id(node) {
            continue;
        }
        let Some(nid) = node["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !truthy(&node["source_file"]) {
            continue;
        }
        let source = norm_path(&python_compat::string(&node["source_file"]), root);
        let rel = Path::new(&source);
        if paths::is_absolute_any_platform(Some(&source)) || rel.file_name().is_none() {
            continue;
        }
        let stem = ids::make_id([base::file_stem(rel)]);
        if stem.is_empty() {
            continue;
        }
        let norm = ids::normalize_id(nid);
        if norm != stem
            && !norm.starts_with(&format!("{stem}_"))
            && old_file_stems(rel)
                .iter()
                .any(|old| old != &stem && (norm == *old || norm.starts_with(&format!("{old}_"))))
        {
            return true;
        }
        checked += 1;
        if checked >= sample {
            break;
        }
    }
    false
}
pub fn doc_twin_remap(nodes: &[Value]) -> BTreeMap<String, String> {
    let by_id: BTreeMap<_, _> = nodes
        .iter()
        .filter(|n| n.is_object() && truthy(&n["id"]))
        .map(|n| (python_compat::string(&n["id"]), n))
        .collect();
    let mut remap = BTreeMap::new();
    for (nid, node) in &by_id {
        let Some(bare_id) = nid.strip_suffix("_doc") else {
            continue;
        };
        let Some(bare) = by_id.get(bare_id) else {
            continue;
        };
        if truthy(&node["source_file"])
            && bare["source_file"] == node["source_file"]
            && node["file_type"] == "document"
            && bare["file_type"] == "document"
        {
            remap.insert(bare_id.into(), nid.clone());
        }
    }
    remap
}
pub fn deduplicate_by_label(
    nodes: &[Value],
    edges: &[Value],
) -> Result<(Vec<Value>, Vec<Value>, Vec<String>)> {
    let mut canonical = Vec::<Value>::new();
    let mut indices = BTreeMap::new();
    let mut remap = BTreeMap::<String, String>::new();
    let suffix = |s: &str| {
        let s = s.strip_suffix('\n').unwrap_or(s);
        s.rsplit_once("_c")
            .is_some_and(|(_, tail)| !tail.is_empty() && tail.chars().all(dedup_strings::decimal))
    };
    for n in nodes {
        let key = dedup::norm(n.get("label").unwrap_or(&n["id"]));
        if key.is_empty() {
            continue;
        }
        let nid = n["id"].as_str().ok_or("Expected node ID")?;
        if let Some(&i) = indices.get(&key) {
            let old: &Value = &canonical[i];
            let oldid = old["id"].as_str().ok_or("Expected node ID")?;
            let win = if suffix(nid) && !suffix(oldid) {
                false
            } else if suffix(oldid) && !suffix(nid) {
                true
            } else {
                nid.chars().count() < oldid.chars().count()
            };
            if win {
                remap.insert(oldid.into(), nid.into());
                canonical[i] = n.clone();
            } else {
                remap.insert(nid.into(), oldid.into());
            }
        } else {
            indices.insert(key, canonical.len());
            canonical.push(n.clone());
        }
    }
    if remap.is_empty() {
        return Ok((nodes.to_vec(), edges.to_vec(), Vec::new()));
    }
    let mut out = Vec::new();
    for edge in edges {
        let mut e = edge.clone();
        for key in ["source", "target"] {
            let raw = e[key].as_str().ok_or("Expected edge endpoint")?;
            if let Some(new) = remap.get(raw) {
                e[key] = json!(new);
            }
        }
        if e["source"] != e["target"] {
            out.push(e);
        }
    }
    Ok((
        canonical,
        out,
        vec![format!(
            "[graphicle] Deduplicated {} duplicate node(s) by label.",
            remap.len()
        )],
    ))
}
pub fn tier_replacement_sources(
    chunks: &[Value],
    root: Option<&Path>,
    ast_sources: Option<&[String]>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut explicit: BTreeSet<String> = ast_sources
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    for chunk in chunks {
        for source in chunk["extracted_sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| truthy(v))
        {
            explicit.insert(python_compat::string(source));
        }
    }
    let mut ast = BTreeSet::new();
    let mut semantic = BTreeSet::new();
    let explicit_present = !explicit.is_empty();
    let add = |set: &mut BTreeSet<String>, source: String| {
        set.insert(source.clone());
        let norm = norm_path(&source, root);
        if !norm.is_empty() {
            set.insert(norm);
        }
    };
    for sf in explicit {
        add(&mut ast, sf);
    }
    for chunk in chunks {
        for n in chunk["nodes"].as_array().into_iter().flatten() {
            if !n.is_object() || !truthy(&n["source_file"]) {
                continue;
            }
            if is_ast_tier(n) {
                if !explicit_present {
                    add(&mut ast, python_compat::string(&n["source_file"]));
                }
            } else {
                add(&mut semantic, python_compat::string(&n["source_file"]));
            }
        }
    }
    (ast, semantic)
}

#[cfg(test)]
mod suffix_tests {
    use super::{shortest_unique_suffix, shortest_unique_suffixes};
    use std::collections::BTreeSet;

    /// The grouped computation gives each member what the one-by-one
    /// comparison gives, including nested, repeated, Windows-separated and
    /// fully identical tails, on groups large enough to run in parallel.
    #[test]
    fn grouped_suffixes_match_one_by_one() {
        let mut groups: Vec<BTreeSet<String>> = vec![
            ["a/b/c.py", "d/b/c.py", "c.py", "x\\b\\c.py", "b/c.py"]
                .into_iter()
                .map(String::from)
                .collect(),
            [
                "pkg/__init__.py",
                "pkg/sub/__init__.py",
                "other/pkg/__init__.py",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        ];
        groups.push(
            (0..150)
                .map(|i| format!("r{}/m{}/s{}/__init__.py", i % 7, i % 11, i))
                .chain(["__init__.py".into(), "//__init__.py".into()])
                .collect(),
        );
        for group in groups {
            let grouped = shortest_unique_suffixes(&group);
            for source in &group {
                assert_eq!(
                    grouped[source],
                    shortest_unique_suffix(source, &group),
                    "{source}"
                );
            }
        }
    }
}
