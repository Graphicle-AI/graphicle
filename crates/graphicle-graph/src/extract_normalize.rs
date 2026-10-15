// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Portable identities and cross-file re-export forwarding for corpus assembly.
use crate::{
    cache,
    extract::per_file::PerFile,
    extract_merge::{file_node_id, remap_objc_fields},
    extractors::{
        base::make_id,
        parse_cache::{self, par_for_each_mut},
        resolution::{field_mut, resolved, s},
    },
    resolver_registry::{Rec, rs},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
pub fn portable_outside(path: &Path, root: &Path) -> String {
    let rel = cache::relative(path, root)
        .unwrap_or_else(|_| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .replace('\\', "/");
    if rel.split('/').take_while(|p| *p == "..").count() > 3 {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        rel
    }
}
pub fn apply_ids<P: PerFile>(
    nodes: &mut [Value],
    edges: &mut [Value],
    per: &mut [P],
    map: &BTreeMap<String, String>,
) {
    if map.is_empty() {
        return;
    }
    // Same mapping, hashed: every node, edge and raw call of the corpus is
    // looked up here, twice per assembly.
    let fast: FxHashMap<&str, &str> = map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    // One attribute lookup per item, rewriting the string in place.
    let remap = |item: &mut Value, key: &str| {
        if let Some(Value::String(current)) = field_mut(item, key)
            && let Some(id) = fast.get(current.as_str())
        {
            *current = (*id).to_string();
        }
    };
    // The same rewrite on a node or raw-call record of either form: the
    // value is replaced in its place.
    let remap_record = |item: &mut P::R, key: &str| {
        if let Some(Value::String(current)) = item.rec_get(key)
            && let Some(id) = fast.get(current.as_str())
        {
            item.rec_set_str(key, id);
        }
    };
    let remap_edges = |edges: &mut [Value]| {
        for e in edges {
            remap(e, "source");
            remap(e, "target");
        }
    };
    // Every item is rewritten on its own, so the lists are split across the
    // run's workers.
    par_for_each_mut(nodes, |n| remap(n, "id"));
    par_for_each_mut(edges, |e| {
        remap(e, "source");
        remap(e, "target");
    });
    par_for_each_mut(per, |r| {
        if let Some(ns) = r.nodes_mut() {
            for n in ns {
                remap_record(n, "id");
            }
        }
        if let Some(es) = r.key_mut("edges").and_then(Value::as_array_mut) {
            remap_edges(es);
        }
        if let Some(rs) = r.raw_calls_mut() {
            for rc in rs {
                remap_record(rc, "caller_nid");
            }
        }
        if let Some(es) = r.key_mut("swift_extensions").and_then(Value::as_array_mut) {
            for e in es {
                remap(e, "nid");
            }
        }
    });
    remap_objc_fields(per, map);
}
/// Contested file-level keys (0.9.91, #4449): a key is contested when a node
/// of a file that did not mint it carries it as one of that file's own symbol
/// ids (the id starts with a stem prefix of its file). Returns each contested
/// key with the files holding such a node; every other key renames as before.
fn contested_keys(
    nodes: &[Value],
    map: &BTreeMap<String, String>,
    owners: &BTreeMap<String, BTreeSet<PathBuf>>,
    forms: &BTreeMap<PathBuf, (String, Vec<String>)>,
) -> BTreeMap<String, BTreeSet<PathBuf>> {
    let mut held: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    for n in nodes {
        let (key, sf) = (s(n, "id"), s(n, "source_file"));
        let Some(minted) = owners.get(key) else {
            continue;
        };
        if sf.is_empty() || !map.contains_key(key) {
            continue;
        }
        let file = resolved(Path::new(sf));
        if minted.contains(&file) {
            continue;
        }
        if let Some((_, prefixes)) = forms.get(&file)
            && prefixes.iter().any(|p| {
                key.strip_prefix(p.as_str())
                    .is_some_and(|tail| tail.starts_with('_'))
            })
        {
            held.entry(key.to_string()).or_default().insert(file);
        }
    }
    held
}
/// The file-level id remap when some key is contested (0.9.91, #4449): a
/// contested key stays on an item whose file holds its own node with that id
/// and did not mint the key; the owner's file, and a file that only refers to
/// the owner, rename as before.
struct OwnedIds<'a> {
    map: &'a BTreeMap<String, String>,
    owners: &'a BTreeMap<String, BTreeSet<PathBuf>>,
    held: &'a BTreeMap<String, BTreeSet<PathBuf>>,
}
impl OwnedIds<'_> {
    /// `_remapped(nid, owner_sf)`: the new id, or None to keep `nid`.
    fn remapped(&self, nid: &str, owner_sf: &str) -> Option<&String> {
        let new = self.map.get(nid)?;
        let Some(holders) = self.held.get(nid) else {
            return Some(new);
        };
        if owner_sf.is_empty() {
            return Some(new);
        }
        let file = resolved(Path::new(owner_sf));
        if self.owners.get(nid).is_some_and(|m| m.contains(&file)) || !holders.contains(&file) {
            Some(new)
        } else {
            None
        }
    }
    fn rewrite(&self, item: &mut Value, key: &str, owner_sf: &str) {
        if let Some(Value::String(current)) = field_mut(item, key)
            && let Some(new) = self.remapped(current, owner_sf)
        {
            *current = new.clone();
        }
    }
    /// [`Self::rewrite`] on a node or raw-call record of either form: the
    /// value is replaced in its place.
    fn rewrite_record<R: Rec>(&self, item: &mut R, key: &str, owner_sf: &str) {
        if let Some(Value::String(current)) = item.rec_get(key)
            && let Some(new) = self.remapped(current, owner_sf)
        {
            item.rec_set_str(key, new);
        }
    }
    fn node(&self, n: &mut Value) {
        let sf = s(n, "source_file").to_string();
        self.rewrite(n, "id", &sf);
    }
    fn node_record<R: Rec>(&self, n: &mut R) {
        let sf = rs(n, "source_file").to_string();
        self.rewrite_record(n, "id", &sf);
    }
    fn edge(&self, e: &mut Value) {
        let sf = s(e, "source_file").to_string();
        let tf = match s(e, "target_file") {
            "" => sf.clone(),
            tf => tf.to_string(),
        };
        self.rewrite(e, "source", &sf);
        self.rewrite(e, "target", &tf);
    }
    fn raw_call<R: Rec>(&self, rc: &mut R) {
        let sf = rs(rc, "source_file").to_string();
        self.rewrite_record(rc, "caller_nid", &sf);
    }
    /// The remap over the corpus lists and the per-file results; the raw
    /// calls are rewritten where they sit, in the per-file results.
    fn apply<P: PerFile>(
        &self,
        nodes: &mut [Value],
        edges: &mut [Value],
        per: &mut [P],
        paths: &[PathBuf],
    ) {
        nodes.iter_mut().for_each(|n| self.node(n));
        edges.iter_mut().for_each(|e| self.edge(e));
        for (r, path) in per.iter_mut().zip(paths) {
            if let Some(ns) = r.nodes_mut() {
                ns.iter_mut().for_each(|n| self.node_record(n));
            }
            if let Some(es) = r.key_mut("edges").and_then(Value::as_array_mut) {
                es.iter_mut().for_each(|e| self.edge(e));
            }
            if let Some(calls) = r.raw_calls_mut() {
                calls.iter_mut().for_each(|rc| self.raw_call(rc));
            }
            let path = path.to_string_lossy();
            if let Some(es) = r.key_mut("swift_extensions").and_then(Value::as_array_mut) {
                for e in es {
                    self.rewrite(e, "nid", &path);
                }
            }
            // ObjC field tables are keyed by class id (#3150): this file's
            // keys follow the remap unless the key stays with this file.
            if let Some(tables) = r.at("objc_field_types")["tables"].as_object() {
                let renamed = tables
                    .iter()
                    .map(|(k, v)| {
                        let key = match self.map.get(k) {
                            Some(new)
                                if !self.held.contains_key(k)
                                    || self.remapped(k, &path).is_some() =>
                            {
                                new.clone()
                            }
                            _ => k.clone(),
                        };
                        (key, v.clone())
                    })
                    .collect::<serde_json::Map<_, _>>();
                r.key_mut("objc_field_types").expect("read just above")["tables"] =
                    Value::Object(renamed);
            }
        }
    }
}
pub fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(o) => o
            .iter()
            .map(|(k, v)| (k.clone(), canonical(v)))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect::<serde_json::Map<_, _>>()
            .into(),
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}
/// Compact JSON of `value` with object keys in sorted order: the text
/// `canonical(value).to_string()` produces, written directly without building
/// the intermediate maps. `skip` drops one top-level key.
pub(crate) fn write_canonical(value: &Value, skip: Option<&str>, out: &mut Vec<u8>) {
    match value {
        Value::Object(o) => {
            let mut keys = o
                .keys()
                .filter(|k| skip != Some(k.as_str()))
                .collect::<Vec<_>>();
            keys.sort_unstable();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, k).expect("string serializes");
                out.push(b':');
                write_canonical(&o[k], None, out);
            }
            out.push(b'}');
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(v, None, out);
            }
            out.push(b']');
        }
        _ => serde_json::to_writer(&mut *out, value).expect("scalar serializes"),
    }
}
/// Compact canonical JSON bytes of `value` (sorted object keys), byte-for-byte
/// equal to `canonical(value).to_string()` without materialising the sorted maps.
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    write_canonical(value, None, &mut out);
    out
}
/// [`canonical_bytes`] of `value` after sorting each of its top-level arrays
/// named in `sorted` by the canonical bytes of their elements (what sorting
/// them with `sort_by_cached_key(canonical_bytes)` and then encoding gives).
/// The element encodings are computed once, on `workers` threads, and reused
/// for the document: an array's canonical bytes are its elements' joined by
/// commas, and elements with equal keys encode identically, so their order
/// among themselves cannot show.
pub fn canonical_bytes_sorting(value: &Value, sorted: &[&str], workers: usize) -> Vec<u8> {
    let Value::Object(o) = value else {
        return canonical_bytes(value);
    };
    let mut keys = o.keys().collect::<Vec<_>>();
    keys.sort_unstable();
    let mut out = Vec::with_capacity(1 << 16);
    out.push(b'{');
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, k).expect("string serializes");
        out.push(b':');
        match &o[k] {
            Value::Array(items) if sorted.contains(&k.as_str()) => {
                let mut encoded = crate::par::map(items, workers, canonical_bytes);
                encoded.sort_unstable();
                out.push(b'[');
                for (j, e) in encoded.iter().enumerate() {
                    if j > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(e);
                }
                out.push(b']');
            }
            other => write_canonical(other, None, &mut out),
        }
    }
    out.push(b'}');
    out
}
fn edge_key(e: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    write_canonical(e, Some("target_file"), &mut out);
    out
}
/// The raw calls are rewritten where they sit, in the per-file results.
pub fn normalize_ids<P: PerFile>(
    paths: &[PathBuf],
    nodes: &mut [Value],
    edges: &mut Vec<Value>,
    per: &mut [P],
    root: &Path,
) {
    let mut map = BTreeMap::new();
    // Input files that minted each old file-level id key, resolved (0.9.91,
    // #4449). A key names its owner's file node; a symbol of another file
    // that merely spells the same string (`const sql` in x.mjs next to
    // x.sql) must not follow it.
    let mut owners: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut prefix: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
    let mut forms: BTreeMap<PathBuf, (String, Vec<String>)> = BTreeMap::new();
    let mut remap_paths = paths.to_vec();
    let mut seen = paths.iter().map(|p| resolved(p)).collect::<BTreeSet<_>>();
    for e in edges.iter() {
        let tf = s(e, "target_file");
        if tf.is_empty() {
            continue;
        }
        let original = PathBuf::from(tf);
        let full = resolved(&original);
        if !seen.insert(full.clone()) {
            continue;
        }
        if !full.starts_with(root) {
            if full.is_file() {
                let new = make_id(["ext", portable_outside(&full, root).as_str()]);
                for p in [&full, &original] {
                    let old = make_id([p.to_string_lossy().as_ref()]);
                    map.insert(old.clone(), new.clone());
                    map.entry(format!("{old}__entry"))
                        .or_insert(format!("{new}__entry"));
                }
            }
            continue;
        }
        if full.is_file() {
            remap_paths.push(full.clone());
            if original != full {
                seen.insert(original.clone());
                remap_paths.push(original);
            }
        }
    }
    for path in remap_paths {
        let full = resolved(&path);
        let Ok(rel) = path.strip_prefix(root).or_else(|_| full.strip_prefix(root)) else {
            continue;
        };
        let new = file_node_id(rel);
        let old = make_id([path.to_string_lossy().as_ref()]);
        let abs = make_id([full.to_string_lossy().as_ref()]);
        if old != new {
            map.insert(old.clone(), new.clone());
            owners.entry(old.clone()).or_default().insert(full.clone());
        }
        if abs != new {
            map.insert(abs.clone(), new.clone());
            owners.entry(abs.clone()).or_default().insert(full.clone());
        }
        let old_pref = file_node_id(&path);
        let abs_pref = file_node_id(&full);
        let mut prefs = vec![];
        if old_pref != new {
            prefs.push((old_pref.clone(), new.clone()));
        }
        if abs_pref != new && abs_pref != old_pref {
            prefs.push((abs_pref.clone(), new.clone()));
        }
        for (o, p) in [(&old, &old_pref), (&abs, &abs_pref)] {
            if let Some(tail) = o.strip_prefix(p) {
                let a = format!("{o}__entry");
                let b = format!("{new}{tail}__entry");
                if a != b {
                    map.entry(a).or_insert(b);
                }
            }
        }
        if !prefs.is_empty() {
            prefix.insert(full.clone(), prefs);
        }
        forms.insert(full, (new.clone(), vec![abs_pref, old_pref, new]));
    }
    let held = contested_keys(nodes, &map, &owners, &forms);
    if held.is_empty() {
        apply_ids(nodes, edges, per, &map);
    } else {
        let owned = OwnedIds {
            map: &map,
            owners: &owners,
            held: &held,
        };
        owned.apply(nodes, edges, per, paths);
    }
    let mut aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    // Nodes arrive grouped by file: resolve each source_file once and keep the
    // `{prefix}_` forms of its entry ready instead of formatting them per node.
    let mut prefixed: FxHashMap<&str, Option<Vec<(String, String)>>> = FxHashMap::default();
    for n in nodes.iter() {
        let sf = s(n, "source_file");
        if sf.is_empty() || s(n, "type") == "package" {
            continue;
        }
        let entry = prefixed.entry(sf).or_insert_with(|| {
            prefix.get(&resolved(Path::new(sf))).map(|entry| {
                entry
                    .iter()
                    .map(|(old, new)| (format!("{old}_"), format!("{new}_")))
                    .collect()
            })
        });
        let Some(entry) = entry else {
            continue;
        };
        let id = s(n, "id");
        let mut canonical_id = None;
        for (old, new) in entry.iter() {
            if let Some(tail) = id.strip_prefix(old.as_str()) {
                let target = format!("{new}{tail}");
                if target != id {
                    symbols.insert(id.into(), target.clone());
                }
                canonical_id = Some(target);
                break;
            }
            if id.starts_with(new.as_str()) {
                canonical_id = Some(id.into());
                break;
            }
        }
        let Some(id): Option<String> = canonical_id else {
            continue;
        };
        for (old, new) in entry.iter() {
            if let Some(tail) = id.strip_prefix(new.as_str()) {
                let target = format!("{old}{tail}");
                if target != id {
                    aliases.entry(target).or_default().insert(id.clone());
                }
            }
        }
    }
    apply_ids(nodes, edges, per, &symbols);
    if !aliases.is_empty() {
        // A twin is an `imports` edge whose target was replaced by the one
        // alias of that target. Its key holds that relation and target, so
        // only edges with exactly those two strings can share it: counting
        // just them gives the same answers as counting every edge, without
        // a key per edge of the corpus.
        let twin_targets = aliases
            .values()
            .filter(|c| c.len() == 1)
            .filter_map(|c| c.iter().next())
            .map(String::as_str)
            .collect::<FxHashSet<_>>();
        let candidates = edges
            .iter()
            .filter(|e| s(e, "relation") == "imports" && twin_targets.contains(s(e, "target")))
            .collect::<Vec<_>>();
        drop(twin_targets);
        let mut counts = FxHashMap::<Vec<u8>, usize>::with_capacity_and_hasher(
            candidates.len(),
            Default::default(),
        );
        for key in crate::par::map(&candidates, parse_cache::workers(), |e| edge_key(e)) {
            *counts.entry(key).or_default() += 1;
        }
        drop(candidates);
        let owned = nodes.iter().map(|n| s(n, "id")).collect::<BTreeSet<_>>();
        edges.retain_mut(|e| {
            let cs = aliases.get(s(e, "target"));
            if s(e, "relation") == "re_exports" {
                if !owned.contains(s(e, "target"))
                    && let Some(c) = cs.filter(|c| c.len() == 1)
                {
                    e["target"] = json!(c.iter().next().unwrap());
                }
                return true;
            }
            if s(e, "relation") == "imports"
                && let Some(cs) = cs.filter(|c| c.len() == 1)
            {
                let mut twin = e.clone();
                twin["target"] = json!(cs.iter().next().unwrap());
                if let Some(count) = counts.get_mut(&edge_key(&twin)).filter(|c| **c > 0) {
                    if owned.contains(s(e, "target")) {
                        *count -= 1;
                    }
                    return false;
                }
            }
            true
        });
    }
    let owned = nodes
        .iter()
        .map(|n| s(n, "id").to_string())
        .collect::<BTreeSet<_>>();
    let decompose = |target: &str, tf: &str| -> Option<(String, String)> {
        let (canonical, prefixes) = forms.get(&resolved(Path::new(tf)))?;
        for p in prefixes {
            if !p.is_empty()
                && let Some(tail) = target.strip_prefix(&format!("{p}_"))
            {
                return Some((canonical.clone(), tail.into()));
            }
        }
        None
    };
    let mut chain: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for e in edges.iter() {
        if s(e, "relation") == "re_exports"
            && !s(e, "target_file").is_empty()
            && owned.contains(s(e, "target"))
            && let Some((_, tail)) = decompose(s(e, "target"), s(e, "target_file"))
        {
            chain
                .entry((s(e, "source").into(), tail))
                .or_default()
                .insert(s(e, "target").into());
        }
    }
    let mut pending = edges
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            ["re_exports", "imports"].contains(&s(e, "relation"))
                && !s(e, "target_file").is_empty()
                && !owned.contains(s(e, "target"))
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    for _ in 0..8 {
        let mut progress = false;
        pending.retain(|i| {
            let e = &mut edges[*i];
            let Some((prefix, tail)) = decompose(s(e, "target"), s(e, "target_file")) else {
                return true;
            };
            let Some(target) = chain
                .get(&(prefix, tail.clone()))
                .filter(|c| c.len() == 1)
                .and_then(|c| c.iter().next())
                .cloned()
            else {
                return true;
            };
            e["target"] = json!(target);
            if s(e, "relation") == "re_exports" {
                chain
                    .entry((s(e, "source").into(), tail))
                    .or_default()
                    .insert(target);
            }
            progress = true;
            false
        });
        if !progress {
            break;
        }
    }
    for i in pending {
        let e = &mut edges[i];
        if let Some((prefix, tail)) = decompose(s(e, "target"), s(e, "target_file")) {
            e["target"] = json!(format!("{prefix}_{tail}"));
        }
    }
}
/// Copy the corpus nodes back into the per-file records they came from.
/// `before` holds the `(id, source_file)` of every node in `after` as it was
/// when the per-file records last matched it.
pub fn sync_per_nodes<P: PerFile>(per: &mut [P], before: &[(String, String)], after: &[Value]) {
    // Later duplicates win, exactly as with the ordered map this replaces.
    let mut changes: FxHashMap<(&str, &str), &Value> =
        FxHashMap::with_capacity_and_hasher(before.len(), Default::default());
    for ((id, sf), b) in before.iter().zip(after) {
        changes.insert((id.as_str(), sf.as_str()), b);
    }
    for r in per {
        if let Some(ns) = r.nodes_mut() {
            for n in ns {
                if let Some(updated) = changes.get(&(rs(n, "id"), rs(n, "source_file")))
                    && !n.rec_eq_value(updated)
                {
                    *n = P::node_from(updated);
                }
            }
        }
    }
}
/// The portable form, canonical file id and id spellings of one absolute
/// source path ([`finish`]).
type SourceEntry = (String, String, BTreeSet<String>);
/// Make `item`'s absolute `source_file` and `definition_file` portable and
/// collect the id spellings of its file into `map` (first spelling wins).
fn relativize<R: Rec>(
    item: &mut R,
    owned: &FxHashSet<String>,
    map: &mut FxHashMap<String, String>,
    source_files: &mut FxHashMap<String, SourceEntry>,
    sf_entry: &impl Fn(&str) -> SourceEntry,
) {
    let sf = rs(item, "source_file");
    if !sf.is_empty() && Path::new(sf).is_absolute() {
        let (source, canonical, keys) = source_files
            .entry(sf.to_string())
            .or_insert_with_key(|sf| sf_entry(sf));
        if item.rec_get("id").is_some() {
            for key in keys.iter() {
                if key == canonical
                    || map.contains_key(key)
                    || (owned.contains(key) && rs(item, "id") != key)
                {
                    continue;
                }
                map.insert(key.clone(), canonical.clone());
            }
        }
        // Present: `rs` just read it.
        item.rec_set_str("source_file", source);
    }
    let df = rs(item, "definition_file").to_string();
    if !df.is_empty() && Path::new(&df).is_absolute() {
        let source = &source_files
            .entry(df)
            .or_insert_with_key(|df| sf_entry(df))
            .0;
        if item.rec_get("definition_file").is_some() {
            item.rec_set_str("definition_file", source);
        }
    }
}
pub fn finish<E: Rec>(nodes: &mut [Value], edges: &mut [E], root: &Path) {
    let owned = nodes
        .iter()
        .map(|n| s(n, "id").to_string())
        .collect::<FxHashSet<_>>();
    let mut map = FxHashMap::default();
    let mut source_files = FxHashMap::default();
    let sf_entry = |sf: &str| {
        let p = Path::new(sf);
        let (source, id) = match p.strip_prefix(root) {
            Ok(rel) => (rel.to_string_lossy().replace('\\', "/"), file_node_id(rel)),
            Err(_) => {
                let portable = portable_outside(p, root);
                let id = make_id(["ext", portable.as_str()]);
                (portable, id)
            }
        };
        let full = resolved(p);
        let keys = BTreeSet::from([
            make_id([sf]),
            make_id([full.to_string_lossy().as_ref()]),
            file_node_id(p),
            file_node_id(&full),
        ]);
        (source, id, keys)
    };
    // Nodes first, then edges: the first spelling of an id wins in `map`.
    for item in nodes.iter_mut() {
        relativize(item, &owned, &mut map, &mut source_files, &sf_entry);
    }
    for item in edges.iter_mut() {
        relativize(item, &owned, &mut map, &mut source_files, &sf_entry);
    }
    // `None` means the id is already canonical, so the value is left untouched.
    let canon = |id: &str| -> Option<String> {
        if let Some(new) = map.get(id) {
            return Some(new.clone());
        }
        if let Some(prefix) = id.strip_suffix("__entry")
            && let Some(new) = map.get(prefix)
        {
            return Some(format!("{new}__entry"));
        }
        if !owned.contains(id) {
            for (i, _) in id.rmatch_indices('_') {
                if i > 0
                    && let Some(prefix) = map.get(&id[..i])
                {
                    return Some(format!("{prefix}{}", &id[i..]));
                }
            }
        }
        None
    };
    let rewrite = |item: &mut Value, key: &str| {
        if let Some(Value::String(current)) = field_mut(item, key)
            && !current.is_empty()
            && let Some(new) = canon(current)
        {
            *current = new;
        }
    };
    par_for_each_mut(nodes, |n| {
        rewrite(n, "id");
        if let Some(o) = n.as_object_mut() {
            o.shift_remove("origin_file");
        }
        append_origin(n);
    });
    par_for_each_mut(edges, |e| {
        for key in ["source", "target"] {
            if let Some(new) = e
                .rec_get(key)
                .and_then(Value::as_str)
                .filter(|current| !current.is_empty())
                .and_then(&canon)
            {
                e.rec_set_str(key, &new);
            }
        }
        e.rec_shift_remove("local_alias");
        e.rec_shift_remove("_python_plain_import");
        append_origin(e);
    });
}
/// `item["_origin"] = "ast"`, growing a full map by exactly one slot instead
/// of letting the index-map double (a record grows by one slot anyway).
fn append_origin<R: Rec>(item: &mut R) {
    item.rec_reserve_key("_origin");
    item.rec_set_str("_origin", "ast");
}
