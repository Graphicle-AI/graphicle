// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py, graphify/extractors/resolution.py
//! Corpus symbol resolution and collision handling from the pinned application.

#[derive(Clone, Copy)]
pub struct ImportEvidence<'a> {
    pub context: &'a str,
    pub line: i64,
    pub path: &'a Path,
    pub target_file: Option<&'a Path>,
    pub alias: Option<&'a str>,
    pub type_only: bool,
}
pub use super::resolution_facts::{PYTHON_STDLIB_MODULE_NAMES, is_python_stdlib_module};
use super::{
    base::{file_stem, make_id},
    models::*,
    resolution_paths::resolve_cached,
};
use crate::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Map, Value, json};
use sha1::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
/// `v[key]` for the small attribute maps of graph items: a linear scan over a
/// handful of keys is cheaper than hashing the key.
pub fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    map_field(v.as_object()?, key)
}
/// Mutable counterpart of [`field`].
pub fn field_mut<'a>(v: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    map_field_mut(v.as_object_mut()?, key)
}
/// [`field`] on the attribute map itself.
pub fn map_field<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.iter().find(|(k, _)| k.as_str() == key).map(|(_, v)| v)
}
/// [`field_mut`] on the attribute map itself.
pub fn map_field_mut<'a>(o: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Value> {
    o.iter_mut()
        .find(|(k, _)| k.as_str() == key)
        .map(|(_, v)| v)
}
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    field(v, key).and_then(Value::as_str).unwrap_or("")
}
pub fn resolved(p: &Path) -> PathBuf {
    resolve_cached(p).unwrap_or_else(|_| p.to_path_buf())
}
pub fn source_key(source: &str, root: &Path) -> String {
    if source.is_empty() {
        return String::new();
    }
    resolved(Path::new(source))
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| source.into())
}
/// [`source_key`] remembered per source string for one pass: the records of a
/// corpus share few files, and each key resolves the path.
struct SourceKeys<'r> {
    root: &'r Path,
    known: FxHashMap<String, String>,
}
impl<'r> SourceKeys<'r> {
    fn new(root: &'r Path) -> Self {
        Self {
            root,
            known: FxHashMap::default(),
        }
    }
    fn key(&mut self, source: &str) -> String {
        if let Some(key) = self.known.get(source) {
            return key.clone();
        }
        let key = source_key(source, self.root);
        self.known.insert(source.to_owned(), key.clone());
        key
    }
    fn node(&mut self, n: &Value) -> String {
        self.key(if s(n, "source_file").is_empty() {
            s(n, "origin_file")
        } else {
            s(n, "source_file")
        })
    }
}
pub fn is_type_like_definition(n: &Value) -> bool {
    let label = s(n, "label").trim();
    s(n, "type") != "namespace"
        && !label.is_empty()
        && !label.ends_with(')')
        && !label.starts_with('.')
        && !label.contains('.')
        && s(n, "file_type") == "code"
}
pub fn js_source_path(source: &str, root: &Path) -> Option<PathBuf> {
    if source.is_empty() {
        None
    } else {
        let p = Path::new(source);
        Some(resolved(&if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }))
    }
}
pub fn disambiguate_colliding_node_ids(
    nodes: &mut [Value],
    edges: &mut [Value],
    raw: &mut [Value],
    root: &Path,
) {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if ["module", "namespace"].contains(&s(n, "type")) || s(n, "id").is_empty() {
            continue;
        }
        groups.entry(s(n, "id").into()).or_default().push(i);
    }
    let mut remap = BTreeMap::new();
    let mut headers = BTreeMap::new();
    let mut source_keys = SourceKeys::new(root);
    for (old, group) in &groups {
        if group.len() < 2 {
            continue;
        }
        let keys = group
            .iter()
            .map(|i| source_keys.node(&nodes[*i]))
            .collect::<BTreeSet<_>>();
        if group.len() < 2 || keys.len() < 2 {
            continue;
        }
        let naive = keys
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|k| {
                let id = make_id([k.as_str(), old.as_str()]);
                (k, id)
            })
            .collect::<BTreeMap<_, _>>();
        for i in group {
            let key = source_keys.node(&nodes[*i]);
            if key.is_empty() {
                continue;
            }
            let naive_id = naive[&key].clone();
            let id = if naive.values().filter(|v| **v == naive_id).count() > 1 {
                let hash = format!("{:x}", sha1::Sha1::digest(key.as_bytes()));
                make_id([key.as_str(), old.as_str(), &hash[..6]])
            } else {
                naive_id
            };
            nodes[*i]["id"] = json!(id);
            remap.insert((old.clone(), key.clone()), id.clone());
            if ["h", "hpp", "hh", "hxx"].contains(
                &Path::new(&key)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .as_str(),
            ) {
                headers.entry(old.clone()).or_insert(id);
            }
        }
    }
    if remap.is_empty() && headers.is_empty() {
        // Nothing to rewrite: only the `target_file` hint is consumed here.
        for e in edges {
            if let Some(o) = e.as_object_mut() {
                o.shift_remove("target_file");
            }
        }
        return;
    }
    for e in edges {
        let key = source_keys.key(s(e, "source_file"));
        let source = (s(e, "source").to_string(), key.clone());
        let target_file = e
            .as_object_mut()
            .and_then(|o| o.shift_remove("target_file"));
        let target_key = if ["imports", "imports_from", "re_exports"].contains(&s(e, "relation")) {
            target_file
                .as_ref()
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| source_keys.key(s))
                .unwrap_or(key)
        } else {
            key
        };
        let target = (s(e, "target").to_string(), target_key);
        if let Some(id) = remap.get(&source) {
            e["source"] = json!(id);
        }
        if ["imports", "imports_from"].contains(&s(e, "relation"))
            && headers.contains_key(s(e, "target"))
        {
            e["target"] = json!(headers[s(e, "target")]);
        } else if let Some(id) = remap.get(&target) {
            e["target"] = json!(id);
        }
    }
    for r in raw {
        let key = (
            s(r, "caller_nid").to_string(),
            source_keys.key(s(r, "source_file")),
        );
        if let Some(id) = remap.get(&key) {
            r["caller_nid"] = json!(id);
        }
    }
}
type Key = (PathBuf, String);
/// Every symbol table here is lookup-only (no iteration reaches the output),
/// so hashing replaces component-wise path ordering.
type Origins = FxHashMap<Key, Key>;
type Symbols = FxHashMap<Key, String>;
type Stars = FxHashMap<PathBuf, Vec<PathBuf>>;
/// Memoised [`resolved`] for the paths one resolution pass keeps revisiting;
/// avoids the global realpath cache lock on every fact.
#[derive(Default)]
struct Resolver(FxHashMap<PathBuf, PathBuf>);
impl Resolver {
    fn get(&mut self, p: &Path) -> PathBuf {
        if let Some(r) = self.0.get(p) {
            return r.clone();
        }
        let r = resolved(p);
        self.0.insert(p.to_path_buf(), r.clone());
        r
    }
    fn source(&mut self, source: &str, root: &Path) -> Option<PathBuf> {
        if source.is_empty() {
            return None;
        }
        let p = Path::new(source);
        Some(if p.is_absolute() {
            self.get(p)
        } else {
            self.get(&root.join(p))
        })
    }
}
fn symbol_node(
    path: &Path,
    name: &str,
    line: i64,
    symbols: &mut Symbols,
    nodes: &mut Vec<Value>,
    resolver: &mut Resolver,
) -> String {
    let key = (resolver.get(path), name.to_string());
    if let Some(id) = symbols.get(&key) {
        return id.clone();
    }
    let id = make_id([file_stem(path).as_str(), name]);
    symbols.insert(key, id.clone());
    nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":path,"source_location":format!("L{line}")}));
    id
}
/// `(source, target, relation, context, emitting file)` of an edge, the
/// identity the pinned application checks before appending a resolved edge.
/// The emitting file (resolved, `None` without one) keeps two files whose ids
/// collide until disambiguation from deduplicating each other's edge (#4143).
/// Packed length-prefixed into one buffer so a corpus-sized set costs one
/// allocation per edge.
type EdgeKey = Vec<u8>;
fn edge_keys(edges: &[Value], rs: &mut Resolver, root: &Path) -> FxHashSet<EdgeKey> {
    let mut keys = FxHashSet::with_capacity_and_hasher(edges.len(), Default::default());
    keys.extend(edges.iter().map(|e| edge_key(e, rs, root)));
    keys
}
fn pack_key(
    source: &str,
    target: &str,
    relation: &str,
    context: &str,
    file: Option<&Path>,
) -> EdgeKey {
    let parts = [source, target, relation, context];
    let file = file.map(|f| f.as_os_str().as_encoded_bytes());
    let mut key = Vec::with_capacity(
        parts.iter().map(|p| p.len() + 4).sum::<usize>() + 5 + file.map_or(0, <[u8]>::len),
    );
    for part in parts {
        key.extend_from_slice(&(part.len() as u32).to_le_bytes());
        key.extend_from_slice(part.as_bytes());
    }
    match file {
        Some(bytes) => {
            key.push(1);
            key.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            key.extend_from_slice(bytes);
        }
        None => key.push(0),
    }
    key
}
fn edge_key(e: &Value, rs: &mut Resolver, root: &Path) -> EdgeKey {
    let file = rs.source(s(e, "source_file"), root);
    pack_key(
        s(e, "source"),
        s(e, "target"),
        s(e, "relation"),
        s(e, "context"),
        file.as_deref(),
    )
}
#[allow(clippy::too_many_arguments)]
fn add_edge(
    edges: &mut Vec<Value>,
    keys: &mut FxHashSet<EdgeKey>,
    rs: &mut Resolver,
    root: &Path,
    src: &str,
    target: &str,
    relation: &str,
    context: ImportEvidence<'_>,
) {
    let ImportEvidence {
        context,
        line,
        path,
        target_file,
        alias,
        type_only,
    } = context;

    let file = rs.source(&path.to_string_lossy(), root);
    if !keys.insert(pack_key(src, target, relation, context, file.as_deref())) {
        return;
    }
    // Sized exactly (plus the `_origin` slot `finish` adds) so the map never
    // doubles past its contents; same key order as the `json!` literal.
    let extra =
        usize::from(target_file.is_some()) + usize::from(alias.is_some()) + usize::from(type_only);
    let mut e = Map::with_capacity(9 + extra);
    e.insert("source".into(), json!(src));
    e.insert("target".into(), json!(target));
    e.insert("relation".into(), json!(relation));
    e.insert("context".into(), json!(context));
    e.insert("confidence".into(), json!("EXTRACTED"));
    e.insert("source_file".into(), json!(path));
    e.insert("source_location".into(), json!(format!("L{line}")));
    e.insert("weight".into(), json!(1.0));
    if let Some(p) = target_file {
        e.insert("target_file".into(), json!(p));
    }
    if let Some(a) = alias {
        e.insert("local_alias".into(), json!(a));
    }
    if type_only {
        e.insert("type_only".into(), json!(true));
    }
    edges.push(Value::Object(e));
}
fn origin(
    key: &Key,
    named: &Origins,
    stars: &Stars,
    symbols: &Symbols,
    members: &FxHashSet<Key>,
    seen: &mut FxHashSet<Key>,
) -> Key {
    if !seen.insert(key.clone()) {
        return key.clone();
    }
    if let Some(next) = named.get(key) {
        return origin(next, named, stars, symbols, members, seen);
    }
    for p in stars.get(&key.0).into_iter().flatten() {
        let k = (p.clone(), key.1.clone());
        if symbols.contains_key(&k) && !members.contains(&k) {
            return k;
        }
        let k = origin(&k, named, stars, symbols, members, seen);
        if symbols.contains_key(&k) && !members.contains(&k) {
            return k;
        }
    }
    key.clone()
}
fn candidates(
    key: &Key,
    exports: &FxHashMap<Key, BTreeSet<Key>>,
    stars: &Stars,
    seen: &FxHashSet<Key>,
) -> (BTreeSet<Key>, bool) {
    if seen.contains(key) {
        return (BTreeSet::new(), true);
    }
    let mut seen = seen.clone();
    seen.insert(key.clone());
    let mut out = BTreeSet::new();
    let mut cyclic = false;
    if let Some(origins) = exports.get(key) {
        for o in origins {
            if o == key {
                out.insert(o.clone());
            } else {
                let (branch, c) = candidates(o, exports, stars, &seen);
                cyclic |= c;
                if branch.is_empty() && !c {
                    out.insert(o.clone());
                }
                out.extend(branch);
            }
        }
    } else {
        for p in stars.get(&key.0).into_iter().flatten() {
            let (branch, c) = candidates(&(p.clone(), key.1.clone()), exports, stars, &seen);
            out.extend(branch);
            cyclic |= c;
        }
    }
    (out, cyclic)
}
pub fn apply_symbol_resolution_facts(
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    root: &Path,
    facts: &SymbolResolutionFacts,
    context_nodes: &[Value],
) {
    let mut rs = Resolver::default();
    let mut keys = edge_keys(edges, &mut rs, root);
    let path_map = paths
        .iter()
        .map(|p| (rs.get(p), p.clone()))
        .collect::<FxHashMap<_, _>>();
    let file_ids = paths
        .iter()
        .map(|p| (rs.get(p), make_id([p.to_string_lossy().as_ref()])))
        .collect::<FxHashMap<_, _>>();
    // `reexport_file_target`: a re-export whose target file is missing gets a
    // portable `unresolved_reexport` id salted with its root-relative path
    // instead of an id minted from the checkout's absolute path (#4257).
    let reexport_target = |target: &Path, rs: &mut Resolver| -> (String, PathBuf) {
        let resolved = rs.get(target);
        let stored = path_map.get(&resolved).cloned().unwrap_or(resolved.clone());
        if !resolved.is_file()
            && let Ok(relative) = resolved.strip_prefix(root)
        {
            let identity = relative.to_string_lossy().replace('\\', "/");
            let salt = format!("{:x}", sha1::Sha1::digest(identity.as_bytes()));
            return (
                make_id([
                    "unresolved_reexport",
                    file_stem(relative).as_str(),
                    &salt[..12],
                ]),
                stored,
            );
        }
        (make_id([stored.to_string_lossy().as_ref()]), stored)
    };
    let mut symbols = Symbols::default();
    let mut members = FxHashSet::default();
    // Unchanged files handed over by an incremental rebuild still own the
    // symbols that fresh imports bind to (#3812). Fresh nodes and fresh files
    // take precedence, so the context is indexed first and only for other files.
    if !context_nodes.is_empty() {
        let fresh_ids = nodes
            .iter()
            .map(|n| s(n, "id"))
            .filter(|id| !id.is_empty())
            .collect::<FxHashSet<_>>();
        let fresh_paths = paths.iter().map(|p| rs.get(p)).collect::<FxHashSet<_>>();
        for n in context_nodes {
            let id = s(n, "id");
            if id.is_empty() || fresh_ids.contains(id) {
                continue;
            }
            let Some(path) = rs.source(s(n, "source_file"), root) else {
                continue;
            };
            if fresh_paths.contains(&path) {
                continue;
            }
            let raw = s(n, "label").trim();
            let label = raw.trim_matches(['(', ')']).trim_start_matches('.');
            if label.is_empty() {
                continue;
            }
            let key = (path, label.to_string());
            if raw.starts_with('.') {
                if symbols.contains_key(&key) {
                    continue;
                }
                members.insert(key.clone());
            } else {
                members.remove(&key);
            }
            symbols.insert(key, id.into());
        }
    }
    for n in nodes.iter() {
        let Some(path) = rs.source(s(n, "source_file"), root) else {
            continue;
        };
        let raw = s(n, "label").trim();
        let label = raw.trim_matches(['(', ')']).trim_start_matches('.');
        if label.is_empty() || s(n, "id").is_empty() {
            continue;
        }
        let key = (path, label.to_string());
        if raw.starts_with('.') {
            if symbols.contains_key(&key) {
                continue;
            }
            members.insert(key.clone());
        } else {
            members.remove(&key);
        }
        symbols.insert(key, s(n, "id").into());
    }
    for d in &facts.declarations {
        symbol_node(&d.file_path, &d.name, d.line, &mut symbols, nodes, &mut rs);
    }
    let mut aliases: Origins = FxHashMap::default();
    for i in &facts.imports {
        aliases.insert(
            (rs.get(&i.file_path), i.local_name.clone()),
            (rs.get(&i.target_path), i.imported_name.clone()),
        );
    }
    loop {
        let mut changed = false;
        for a in &facts.aliases {
            let path = rs.get(&a.file_path);
            let key = (path.clone(), a.alias.clone());
            if aliases.contains_key(&key) {
                continue;
            }
            if let Some(o) = aliases.get(&(path, a.target_name.clone())).cloned() {
                aliases.insert(key, o);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut named = Origins::default();
    let mut exports: FxHashMap<Key, BTreeSet<Key>> = FxHashMap::default();
    let mut stars = Stars::default();
    for star in &facts.star_exports {
        let p = rs.get(&star.file_path);
        let t = rs.get(&star.target_path);
        stars.entry(p.clone()).or_default().push(t.clone());
        if let Some(source) = file_ids.get(&p) {
            let (target_id, target) = reexport_target(&t, &mut rs);
            add_edge(
                edges,
                &mut keys,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: star.line,
                    path: &star.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: star.type_only,
                },
            );
        }
    }
    for n in &facts.namespace_exports {
        let p = rs.get(&n.file_path);
        let t = rs.get(&n.target_path);
        let id = symbol_node(
            &n.file_path,
            &n.exported_name,
            n.line,
            &mut symbols,
            nodes,
            &mut rs,
        );
        let key = (p.clone(), n.exported_name.clone());
        named.insert(key.clone(), key);
        if let Some(source) = file_ids.get(&p) {
            add_edge(
                edges,
                &mut keys,
                &mut rs,
                root,
                source,
                &id,
                "contains",
                ImportEvidence {
                    context: "namespace_export",
                    line: n.line,
                    path: &n.file_path,
                    target_file: None,
                    alias: None,
                    type_only: false,
                },
            );
            let (target_id, target) = reexport_target(&t, &mut rs);
            add_edge(
                edges,
                &mut keys,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: n.line,
                    path: &n.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: n.type_only,
                },
            );
        }
    }
    for e in &facts.exports {
        let p = rs.get(&e.file_path);
        let key = (p.clone(), e.exported_name.clone());
        let o = if let (Some(tp), Some(tn)) = (&e.target_path, &e.target_name) {
            Some((rs.get(tp), tn.clone()))
        } else if let Some(local) = &e.local_name {
            let lk = (p.clone(), local.clone());
            aliases
                .get(&lk)
                .cloned()
                .or_else(|| symbols.contains_key(&lk).then_some(lk))
        } else {
            None
        };
        let Some(o) = o else {
            if let Some(local) = &e.local_name {
                exports.entry(key).or_default().insert((p, local.clone()));
            }
            continue;
        };
        exports.entry(key.clone()).or_default().insert(o.clone());
        named.insert(key, o.clone());
        if o.0 != p
            && let Some(source) = file_ids.get(&p)
        {
            let (target_id, target) = reexport_target(&o.0, &mut rs);
            add_edge(
                edges,
                &mut keys,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: e.line,
                    path: &e.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: e.type_only,
                },
            );
        }
    }
    let owned = nodes.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
    let mut retargeted = false;
    for edge in edges.iter_mut() {
        if s(edge, "relation") != "re_exports" || owned.contains(s(edge, "target")) {
            continue;
        }
        let Some(source) = rs.source(s(edge, "source_file"), root) else {
            continue;
        };
        let target_file = s(edge, "target_file");
        if target_file.is_empty() {
            continue;
        }
        let path = PathBuf::from(target_file);
        for e in &facts.exports {
            let (Some(tp), Some(tn)) = (&e.target_path, &e.target_name) else {
                continue;
            };
            if rs.get(&e.file_path) != source
                || format!("L{}", e.line) != s(edge, "source_location")
                || rs.get(tp) != rs.get(&path)
                || make_id([file_stem(&path).as_str(), tn.as_str()]) != s(edge, "target")
            {
                continue;
            }
            let (cs, _) = candidates(
                &(rs.get(tp), tn.clone()),
                &exports,
                &stars,
                &FxHashSet::default(),
            );
            if cs.len() == 1 {
                let key = cs.iter().next().unwrap();
                if let Some(id) = symbols.get(key).filter(|id| owned.contains(id.as_str())) {
                    edge["target"] = json!(id);
                    edge["target_file"] = json!(path_map.get(&key.0).unwrap_or(&key.0));
                    retargeted = true;
                }
            }
            break;
        }
    }
    // Retargeting above changed edge identities in place.
    if retargeted {
        keys = edge_keys(edges, &mut rs, root);
    }
    for i in &facts.imports {
        if let Some(source) = file_ids.get(&rs.get(&i.file_path)) {
            let o = origin(
                &(rs.get(&i.target_path), i.imported_name.clone()),
                &named,
                &stars,
                &symbols,
                &members,
                &mut FxHashSet::default(),
            );
            if let Some(target) = symbols.get(&o) {
                add_edge(
                    edges,
                    &mut keys,
                    &mut rs,
                    root,
                    source,
                    target,
                    "imports",
                    ImportEvidence {
                        context: "import",
                        line: i.line,
                        path: &i.file_path,
                        target_file: None,
                        alias: None,
                        type_only: false,
                    },
                );
            }
        }
    }
    // #3777: a provisional AST `imports_from` edge whose package id collided
    // with a same-named sibling module file (`from nettacker import logger`
    // linking `nettacker.py`) is retracted when the submodule edge replaces it.
    let mut provisional: FxHashMap<(PathBuf, String), Vec<usize>> = FxHashMap::default();
    for (i, e) in edges.iter().enumerate() {
        if s(e, "relation") == "imports_from"
            && s(e, "context") == "import"
            && let Some(src) = rs.source(s(e, "source_file"), root)
        {
            let loc = s(e, "source_location");
            if !loc.is_empty() {
                provisional.entry((src, loc.into())).or_default().push(i);
            }
        }
    }
    let mut retracted = FxHashSet::default();
    for (from, to, line, local) in &facts.module_imports {
        if let (Ok(from_rel), Ok(to_rel)) = (from.strip_prefix(root), to.strip_prefix(root)) {
            let src = make_id([file_stem(from_rel).as_str()]);
            let tgt = make_id([file_stem(to_rel).as_str()]);
            let candidates = module_collision_ids(from, to, root);
            let matched = provisional
                .get(&(rs.get(from), format!("L{line}")))
                .into_iter()
                .flatten()
                .copied()
                .find(|i| !retracted.contains(i) && candidates.contains(s(&edges[*i], "target")));
            // The retracted edge's identity stays known: upstream discards a
            // four-part key that matches none of its five-part keys.
            if let Some(i) = matched {
                retracted.insert(i);
            }
            let alias = (Some(local.as_str()) != to.file_stem().and_then(|s| s.to_str()))
                .then_some(local.as_str());
            add_edge(
                edges,
                &mut keys,
                &mut rs,
                root,
                &src,
                &tgt,
                "imports_from",
                ImportEvidence {
                    context: "submodule_import",
                    line: *line,
                    path: from,
                    target_file: None,
                    alias,
                    type_only: false,
                },
            );
        }
    }
    if !retracted.is_empty() {
        let mut i = 0;
        edges.retain(|_| {
            i += 1;
            !retracted.contains(&(i - 1))
        });
    }
    for u in &facts.uses {
        let p = rs.get(&u.file_path);
        let key = (p.clone(), u.local_name.clone());
        let target = aliases
            .get(&key)
            .and_then(|o| {
                symbols.get(&origin(
                    o,
                    &named,
                    &stars,
                    &symbols,
                    &members,
                    &mut FxHashSet::default(),
                ))
            })
            .or_else(|| {
                if matches!(u.relation.as_str(), "inherits" | "implements") {
                    symbols.get(&key)
                } else {
                    None
                }
            });
        let Some(target) = target else { continue };
        let mut source = u.source_id.as_str();
        if matches!(
            u.relation.as_str(),
            "inherits" | "implements" | "references"
        ) && !owned.contains(source)
        {
            continue;
        }
        if u.relation == "calls" && !owned.contains(source) {
            let Some(id) = file_ids.get(&p) else { continue };
            source = id;
        }
        add_edge(
            edges,
            &mut keys,
            &mut rs,
            root,
            source,
            target,
            &u.relation,
            ImportEvidence {
                context: &u.context,
                line: u.line,
                path: &u.file_path,
                target_file: None,
                alias: None,
                type_only: false,
            },
        );
    }
}
/// Node ids a provisional `imports_from` edge for `from pkg import sub` may
/// carry when the package name collided with a module file (#3777). The
/// package `__init__.py` id is deliberately absent: since #3729 that edge is a
/// genuine package dependency and survives next to the submodule edge.
fn module_collision_ids(from: &Path, to: &Path, root: &Path) -> FxHashSet<String> {
    let id = |p: &Path| make_id([p.to_string_lossy().as_ref()]);
    let mut out = FxHashSet::default();
    let package = to.parent().unwrap_or(Path::new(""));
    if let Ok(rel) = package.strip_prefix(root) {
        let dotted = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join(".");
        out.insert(make_id([dotted.as_str()]));
        out.insert(make_id([file_stem(rel).as_str()]));
        out.insert(id(rel));
    }
    out.insert(id(package));
    // `Path.with_suffix` has no spelling for a nameless path.
    if package.file_name().is_some() {
        out.insert(id(&package.with_extension("py")));
    }
    if let Some(parent) = from.parent()
        && let Ok(rel) = parent.strip_prefix(root)
        && rel.file_name().is_some()
    {
        out.insert(id(&rel.with_extension("py")));
    }
    out
}
pub fn augment_symbol_resolution_edges(
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    root: &Path,
    ambiguous: &super::resolution_facts::AmbiguousModules,
    context_nodes: &[Value],
) {
    let mut facts = SymbolResolutionFacts::default();
    super::resolution_facts::collect_js(paths, &mut facts);
    super::resolution_facts::collect_python(paths, root, &mut facts, ambiguous);
    apply_symbol_resolution_facts(paths, nodes, edges, root, &facts, context_nodes);
}
fn ambiguous_modules(r: &Value) -> super::resolution_facts::AmbiguousModules {
    r["ambiguous_python_modules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let action = s(r, "action");
    let root = Path::new(r["root"].as_str().unwrap_or("."));
    let mut nodes = r["nodes"].as_array().cloned().unwrap_or_default();
    let mut edges = r["edges"].as_array().cloned().unwrap_or_default();
    let paths: Vec<PathBuf> = serde_json::from_value(r.get("paths").cloned().unwrap_or(json!([])))
        .map_err(crate::Error::from)?;
    let ambiguous = ambiguous_modules(r);
    let context_nodes = r["context_nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(match action {
        "source_key" => json!(source_key(s(r, "source"), root)),
        "type_like" => json!(is_type_like_definition(&r["node"])),
        "merge_decl_def" => {
            super::resolution_types::merge_decl_def_classes(&mut nodes, &mut edges);
            json!({"nodes":nodes,"edges":edges})
        }
        "python_imports" | "java_imports" | "java_types" | "php_types" | "go_types" => {
            let per_file = r["per_file"].as_array().cloned().unwrap_or_default();
            let mut added = vec![];
            match action {
                "python_imports" => {
                    added = super::resolution_types::cross_file_python_imports(
                        &per_file, &paths, &mut nodes, &mut edges, &ambiguous,
                    )
                }
                "java_imports" => {
                    added = super::resolution_types::cross_file_java_imports(&per_file, &paths)
                }
                "java_types" => super::resolution_types::resolve_java_type_references(
                    &per_file, &paths, &mut nodes, &mut edges,
                ),
                "php_types" => super::resolution_types::resolve_php_type_references(
                    &per_file, &paths, &mut nodes, &mut edges,
                ),
                "go_types" => super::resolution_types::resolve_go_type_references(
                    &per_file,
                    &paths,
                    &mut nodes,
                    &mut edges,
                    root,
                    r["context_nodes"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    r["context_edges"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                ),
                _ => unreachable!(),
            }
            json!({"nodes":nodes,"edges":edges,"added":added})
        }
        "go_import_path" => json!(super::resolution_types::go_import_path_for_file(
            Path::new(s(r, "source")),
            root
        )),
        "php_fqn" => json!(super::resolution_types::php_fqn_from_raw(
            s(r, "raw"),
            s(r, "namespace"),
            &serde_json::from_value(r.get("uses").cloned().unwrap_or(json!({})))
                .map_err(crate::Error::from)?
        )),
        "python_module" => json!(super::resolution_facts::resolve_python_module_path(
            s(r, "module"),
            Path::new(s(r, "path")),
            root,
            r["level"].as_u64().unwrap_or(0) as usize
        )),
        "python_namespace" => json!(super::resolution_facts::resolve_python_namespace_dir(
            s(r, "module"),
            Path::new(s(r, "path")),
            root,
            r["level"].as_u64().unwrap_or(0) as usize
        )),
        "python_probe" => json!(super::engine_config::probe_python_module_candidate(
            Path::new(s(r, "path"))
        )),
        "pascal_root" => json!(super::pascal_paths::project_root(Path::new(s(r, "path")))),
        "pascal_unit" => json!(super::pascal_paths::resolve_unit(
            Path::new(s(r, "path")),
            s(r, "name")
        )),
        "pascal_class" => json!(super::pascal_paths::resolve_class(
            Path::new(s(r, "path")),
            s(r, "name")
        )),
        "c_include" => json!(
            super::resolution_paths::resolve_c_include_path(s(r, "raw"), s(r, "path"))
                .map_err(|e| e.to_string())?
        ),
        "disambiguate" => {
            let mut raw = r["raw_calls"].as_array().cloned().unwrap_or_default();
            disambiguate_colliding_node_ids(&mut nodes, &mut edges, &mut raw, root);
            json!({"nodes":nodes,"edges":edges,"raw_calls":raw})
        }
        "apply_facts" => {
            let facts = serde_json::from_value(r["facts"].clone()).map_err(crate::Error::from)?;
            apply_symbol_resolution_facts(
                &paths,
                &mut nodes,
                &mut edges,
                root,
                &facts,
                context_nodes,
            );
            json!({"nodes":nodes,"edges":edges})
        }
        "facts" => {
            let mut facts = SymbolResolutionFacts::default();
            super::resolution_facts::collect_js(&paths, &mut facts);
            super::resolution_facts::collect_python(&paths, root, &mut facts, &ambiguous);
            json!(facts)
        }
        "js_facts" | "python_facts" => {
            let mut facts: SymbolResolutionFacts =
                serde_json::from_value(r.get("facts").cloned().unwrap_or(json!({})))
                    .map_err(crate::Error::from)?;
            if action == "js_facts" {
                super::resolution_facts::collect_js(&paths, &mut facts);
            } else {
                super::resolution_facts::collect_python(&paths, root, &mut facts, &ambiguous);
            }
            json!(facts)
        }
        "augment" => {
            augment_symbol_resolution_edges(
                &paths,
                &mut nodes,
                &mut edges,
                root,
                &ambiguous,
                context_nodes,
            );
            json!({"nodes":nodes,"edges":edges})
        }
        _ => return Err(format!("Unknown resolution action: {action}").into()),
    })
}
