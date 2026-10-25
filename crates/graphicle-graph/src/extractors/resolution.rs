// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
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
    resolution_facts::{PassFacts, UseRef},
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
        .map(crate::fsutil::rel_string)
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
/// A dotted label names a namespace or module in most languages, but a SQL
/// table is `schema.name`: a type-like definition a reference binds to
/// (Graphify 0.9.94, #4653).
pub fn is_type_like_definition(n: &Value) -> bool {
    let label = s(n, "label").trim();
    s(n, "type") != "namespace"
        && !label.is_empty()
        && !label.ends_with(')')
        && !label.starts_with('.')
        && (!label.contains('.') || s(n, "source_file").to_lowercase().ends_with(".sql"))
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
pub fn disambiguate_colliding_node_ids<R: crate::resolver_registry::Rec>(
    nodes: &mut [Value],
    edges: &mut [Value],
    raw: &mut [R],
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
            crate::resolver_registry::rs(r, "caller_nid").to_string(),
            source_keys.key(crate::resolver_registry::rs(r, "source_file")),
        );
        if let Some(id) = remap.get(&key) {
            r.rec_set_str("caller_nid", id);
        }
    }
}
/// A resolved path of one resolution pass, as two numbers the pass's
/// [`Resolver`] hands out: `entry` names its exact spelling and `class` the
/// paths equal to it component-wise. Keys compare, hash and order by `class`
/// alone, exactly as the `PathBuf`s they replace compare, and each key keeps
/// the spelling it was made from, as a `PathBuf` key does.
#[derive(Clone, Copy, Debug)]
struct P {
    class: u32,
    entry: u32,
}
impl PartialEq for P {
    fn eq(&self, other: &Self) -> bool {
        self.class == other.class
    }
}
impl Eq for P {}
impl std::hash::Hash for P {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.class, state);
    }
}
impl PartialOrd for P {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for P {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.class.cmp(&other.class)
    }
}
/// A symbol: a resolved path and an interned name. The tables are lookup-only
/// (no iteration order reaches the output), so the numbers can stand in for
/// the path and the string.
type Key = (P, u32);
type Origins = FxHashMap<Key, Key>;
type Symbols = FxHashMap<Key, Sym>;
type Stars = FxHashMap<P, Vec<P>>;
/// An import's authored site (importer, `L<line>`, target file) and the
/// `(barrel, imported name)` pairs it named there (#4486).
type ImportSites<'a> = FxHashMap<(P, String, P), Vec<(P, &'a str)>>;
/// The node a symbol names, by its number among the pass's fresh nodes
/// ([`Nodes`]) or its index in the context nodes, so the tables hold no copy
/// of the id.
#[derive(Clone, Copy)]
enum Sym {
    Node(u32),
    Context(u32),
}
/// The pass's fresh nodes: the ones it was given and the ones it appends,
/// numbered in that order. The appended ones are kept apart until the pass
/// ends and then moved over in one exactly sized step, as its edges are.
#[derive(Clone, Copy)]
struct Nodes<'n> {
    base: &'n [Value],
    added: &'n [Value],
}
impl<'n> Nodes<'n> {
    fn id(&self, i: u32) -> &'n str {
        let i = i as usize;
        match i.checked_sub(self.base.len()) {
            Some(j) => s(&self.added[j], "id"),
            None => s(&self.base[i], "id"),
        }
    }
    fn iter(self) -> impl Iterator<Item = &'n Value> {
        self.base.iter().chain(self.added)
    }
}
fn sym_id<'v>(sym: Sym, nodes: Nodes<'v>, context: &'v [Value]) -> &'v str {
    match sym {
        Sym::Node(i) => nodes.id(i),
        Sym::Context(i) => s(&context[i as usize], "id"),
    }
}
/// The names of one pass's symbol keys, each stored once.
#[derive(Default)]
struct Names(FxHashMap<Box<str>, u32>);
impl Names {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.0.get(name) {
            return id;
        }
        let id = self.0.len() as u32;
        self.0.insert(name.into(), id);
        id
    }
    /// The name's number, if any key was made with it.
    fn get(&self, name: &str) -> Option<u32> {
        self.0.get(name).copied()
    }
}
/// Memoised [`resolved`] for the paths one resolution pass keeps revisiting;
/// avoids the global realpath cache lock on every fact. It numbers the
/// resolved paths so keys copy no path, and lives only as long as its pass.
#[derive(Default)]
struct Resolver {
    /// Input path (compared component-wise) to its resolved path.
    inputs: FxHashMap<PathBuf, P>,
    /// Resolved spelling (compared byte-wise) to its entry.
    entries: FxHashMap<std::ffi::OsString, u32>,
    /// Resolved spelling of each entry.
    paths: Vec<PathBuf>,
    /// Class of each entry, and the classes by component-wise path.
    class_of: Vec<u32>,
    classes: FxHashMap<PathBuf, u32>,
}
impl Resolver {
    fn get(&mut self, p: &Path) -> P {
        if let Some(r) = self.inputs.get(p) {
            return *r;
        }
        let r = self.intern(resolved(p));
        self.inputs.insert(p.to_path_buf(), r);
        r
    }
    fn intern(&mut self, r: PathBuf) -> P {
        if let Some(&entry) = self.entries.get(r.as_os_str()) {
            return P {
                class: self.class_of[entry as usize],
                entry,
            };
        }
        let entry = self.paths.len() as u32;
        let next = self.classes.len() as u32;
        let class = match self.classes.get(&r) {
            Some(&class) => class,
            None => {
                self.classes.insert(r.clone(), next);
                next
            }
        };
        self.class_of.push(class);
        self.entries.insert(r.clone().into_os_string(), entry);
        self.paths.push(r);
        P { class, entry }
    }
    /// The spelling `p` was resolved to.
    fn path(&self, p: P) -> &Path {
        &self.paths[p.entry as usize]
    }
    fn source(&mut self, source: &str, root: &Path) -> Option<P> {
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
#[allow(clippy::too_many_arguments)]
fn symbol_node(
    path: &Path,
    name: &str,
    line: i64,
    symbols: &mut Symbols,
    names: &mut Names,
    nodes: &[Value],
    added: &mut Vec<Value>,
    resolver: &mut Resolver,
) -> Sym {
    let key = (resolver.get(path), names.id(name));
    if let Some(sym) = symbols.get(&key) {
        return *sym;
    }
    let id = make_id([file_stem(path).as_str(), name]);
    let sym = Sym::Node((nodes.len() + added.len()) as u32);
    symbols.insert(key, sym);
    added.push(json!({"id":id,"label":name,"file_type":"code","source_file":path,"source_location":format!("L{line}")}));
    sym
}
/// `(source, target, relation, context, emitting file)` of an edge, the
/// identity the pinned application checks before appending a resolved edge.
/// The emitting file (its resolved spelling, `None` without one) keeps two
/// files whose ids collide until disambiguation from deduplicating each
/// other's edge (#4143).
type Ident<'a> = (&'a str, &'a str, &'a str, &'a str, Option<u32>);
/// 32 bits suffice: identities that share a hash are told apart by
/// comparing the edges themselves.
fn ident_hash(k: &Ident<'_>) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    for part in [k.0, k.1, k.2, k.3] {
        h.write_usize(part.len());
        h.write(part.as_bytes());
    }
    h.write_u64(k.4.map_or(0, |f| u64::from(f) + 1));
    let h = h.finish();
    (h ^ (h >> 32)) as u32
}
fn ident<'e>(e: &'e Value, rs: &mut Resolver, root: &Path) -> Ident<'e> {
    let file = rs.source(s(e, "source_file"), root).map(|p| p.entry);
    (
        s(e, "source"),
        s(e, "target"),
        s(e, "relation"),
        s(e, "context"),
        file,
    )
}
/// The pass's edges: the ones it was given and the ones it appends, numbered
/// in that order.
#[derive(Clone, Copy)]
struct Edges<'e> {
    base: &'e [Value],
    added: &'e [Value],
}
impl<'e> Edges<'e> {
    fn len(&self) -> usize {
        self.base.len() + self.added.len()
    }
    fn get(&self, i: u32) -> &'e Value {
        let i = i as usize;
        match i.checked_sub(self.base.len()) {
            Some(j) => &self.added[j],
            None => &self.base[i],
        }
    }
}
/// The set of edge identities, held as the number of the first edge with each
/// identity: the edges themselves spell the strings, so the set copies none.
/// An identity is never removed, and an edge is only changed in place before
/// [`EdgeSet::index`] numbers them all again.
#[derive(Default)]
struct EdgeSet {
    /// Identity hash to the first edge with it.
    first: FxHashMap<u32, u32>,
    /// Further edges whose identities share a hash with another's.
    more: FxHashMap<u32, Vec<u32>>,
}
impl EdgeSet {
    fn index(&mut self, edges: Edges<'_>, rs: &mut Resolver, root: &Path) {
        self.first.clear();
        self.more.clear();
        self.first.reserve(edges.len());
        for i in 0..edges.len() as u32 {
            let k = ident(edges.get(i), rs, root);
            self.insert(edges, rs, root, &k, i);
        }
    }
    /// Records edge `i` under identity `k`, unless an edge already has it.
    fn insert(
        &mut self,
        edges: Edges<'_>,
        rs: &mut Resolver,
        root: &Path,
        k: &Ident<'_>,
        i: u32,
    ) -> bool {
        let hash = ident_hash(k);
        match self.first.entry(hash) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(i);
                return true;
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                if ident(edges.get(*o.get()), rs, root) == *k {
                    return false;
                }
            }
        }
        let more = self.more.entry(hash).or_default();
        if more.iter().any(|&j| ident(edges.get(j), rs, root) == *k) {
            return false;
        }
        more.push(i);
        true
    }
}
/// The edges a pass appends, kept apart from the ones it was given until the
/// pass ends, then moved over in one exactly sized step.
#[derive(Default)]
struct NewEdges {
    keys: EdgeSet,
    added: Vec<Value>,
}
impl NewEdges {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        base: &[Value],
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

        let file = rs.source(&path.to_string_lossy(), root).map(|p| p.entry);
        let edges = Edges {
            base,
            added: &self.added,
        };
        let i = edges.len() as u32;
        if !self
            .keys
            .insert(edges, rs, root, &(src, target, relation, context, file), i)
        {
            return;
        }
        // Sized exactly (plus the `_origin` slot `finish` adds) so the map never
        // doubles past its contents; same key order as the `json!` literal.
        let extra = usize::from(target_file.is_some())
            + usize::from(alias.is_some())
            + usize::from(type_only);
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
        self.added.push(Value::Object(e));
    }
}
fn origin(
    key: &Key,
    named: &Origins,
    stars: &Stars,
    symbols: &Symbols,
    members: &FxHashSet<Key>,
    seen: &mut FxHashSet<Key>,
) -> Key {
    if !seen.insert(*key) {
        return *key;
    }
    if let Some(next) = named.get(key) {
        return origin(next, named, stars, symbols, members, seen);
    }
    for p in stars.get(&key.0).into_iter().flatten() {
        let k = (*p, key.1);
        if symbols.contains_key(&k) && !members.contains(&k) {
            return k;
        }
        let k = origin(&k, named, stars, symbols, members, seen);
        if symbols.contains_key(&k) && !members.contains(&k) {
            return k;
        }
    }
    *key
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
    seen.insert(*key);
    let mut out = BTreeSet::new();
    let mut cyclic = false;
    if let Some(origins) = exports.get(key) {
        for o in origins {
            if o == key {
                out.insert(*o);
            } else {
                let (branch, c) = candidates(o, exports, stars, &seen);
                cyclic |= c;
                if branch.is_empty() && !c {
                    out.insert(*o);
                }
                out.extend(branch);
            }
        }
    } else {
        for p in stars.get(&key.0).into_iter().flatten() {
            let (branch, c) = candidates(&(*p, key.1), exports, stars, &seen);
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
    let facts = PassFacts::new(Vec::new(), facts.clone());
    apply_pass_facts(paths, nodes, edges, root, facts, context_nodes);
}
/// [`apply_symbol_resolution_facts`] consuming the facts, so each kind is
/// freed as soon as its last pass ends.
fn apply_pass_facts(
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    root: &Path,
    mut facts: PassFacts,
    context_nodes: &[Value],
) {
    let span = crate::profile::span("assemble.symbol_resolution.index");
    let mut rs = Resolver::default();
    let mut new = NewEdges::default();
    new.keys.index(
        Edges {
            base: edges,
            added: &[],
        },
        &mut rs,
        root,
    );
    // Last spelling in `paths` per resolved path, as collecting into a map does.
    let mut path_map: FxHashMap<P, usize> = FxHashMap::default();
    let mut file_ids: FxHashMap<P, String> = FxHashMap::default();
    for (i, p) in paths.iter().enumerate() {
        let r = rs.get(p);
        path_map.insert(r, i);
        file_ids.insert(r, make_id([p.to_string_lossy().as_ref()]));
    }
    // `reexport_file_target`: a re-export whose target file is missing gets a
    // portable `unresolved_reexport` id salted with its root-relative path
    // instead of an id minted from the checkout's absolute path (#4257).
    let reexport_target = |target: P, rs: &mut Resolver| -> (String, PathBuf) {
        let target = rs.path(target).to_path_buf();
        let r = rs.get(&target);
        let resolved = rs.path(r);
        let stored = path_map
            .get(&r)
            .map(|&i| paths[i].clone())
            .unwrap_or_else(|| resolved.to_path_buf());
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
    let mut names = Names::default();
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
        for (i, n) in context_nodes.iter().enumerate() {
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
            let key = (path, names.id(label));
            if raw.starts_with('.') {
                if symbols.contains_key(&key) {
                    continue;
                }
                members.insert(key);
            } else {
                members.remove(&key);
            }
            symbols.insert(key, Sym::Context(i as u32));
        }
    }
    for (i, n) in nodes.iter().enumerate() {
        let Some(path) = rs.source(s(n, "source_file"), root) else {
            continue;
        };
        let raw = s(n, "label").trim();
        let label = raw.trim_matches(['(', ')']).trim_start_matches('.');
        if label.is_empty() || s(n, "id").is_empty() {
            continue;
        }
        let key = (path, names.id(label));
        if raw.starts_with('.') {
            if symbols.contains_key(&key) {
                continue;
            }
            members.insert(key);
        } else {
            members.remove(&key);
        }
        symbols.insert(key, Sym::Node(i as u32));
    }
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.declarations");
    let mut added_nodes = Vec::new();
    for d in facts.declarations() {
        symbol_node(
            d.file_path,
            d.name,
            d.line,
            &mut symbols,
            &mut names,
            nodes,
            &mut added_nodes,
            &mut rs,
        );
    }
    facts.free_declarations();
    let mut aliases: Origins = FxHashMap::default();
    for i in facts.imports() {
        aliases.insert(
            (rs.get(i.file_path), names.id(i.local_name)),
            (rs.get(i.target_path), names.id(i.imported_name)),
        );
    }
    loop {
        let mut changed = false;
        for a in facts.aliases() {
            let path = rs.get(a.file_path);
            if names
                .get(a.alias)
                .is_some_and(|alias| aliases.contains_key(&(path, alias)))
            {
                continue;
            }
            let o = names
                .get(a.target_name)
                .and_then(|target| aliases.get(&(path, target)))
                .copied();
            if let Some(o) = o {
                aliases.insert((path, names.id(a.alias)), o);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    facts.free_aliases();
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.exports");
    let mut named = Origins::default();
    let mut exports: FxHashMap<Key, BTreeSet<Key>> = FxHashMap::default();
    let mut stars = Stars::default();
    for star in facts.star_exports() {
        let p = rs.get(star.file_path);
        let t = rs.get(star.target_path);
        stars.entry(p).or_default().push(t);
        if let Some(source) = file_ids.get(&p) {
            let (target_id, target) = reexport_target(t, &mut rs);
            new.add(
                edges,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: star.line,
                    path: star.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: star.type_only,
                },
            );
        }
    }
    facts.free_star_exports();
    for n in facts.namespace_exports() {
        let p = rs.get(n.file_path);
        let t = rs.get(n.target_path);
        let sym = symbol_node(
            n.file_path,
            n.exported_name,
            n.line,
            &mut symbols,
            &mut names,
            nodes,
            &mut added_nodes,
            &mut rs,
        );
        let key = (p, names.id(n.exported_name));
        named.insert(key, key);
        if let Some(source) = file_ids.get(&p) {
            new.add(
                edges,
                &mut rs,
                root,
                source,
                sym_id(
                    sym,
                    Nodes {
                        base: nodes,
                        added: &added_nodes,
                    },
                    context_nodes,
                ),
                "contains",
                ImportEvidence {
                    context: "namespace_export",
                    line: n.line,
                    path: n.file_path,
                    target_file: None,
                    alias: None,
                    type_only: false,
                },
            );
            let (target_id, target) = reexport_target(t, &mut rs);
            new.add(
                edges,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: n.line,
                    path: n.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: n.type_only,
                },
            );
        }
    }
    facts.free_namespace_exports();
    for e in facts.exports() {
        let p = rs.get(e.file_path);
        let key = (p, names.id(e.exported_name));
        let o = if let (Some(tp), Some(tn)) = (e.target_path, e.target_name) {
            Some((rs.get(tp), names.id(tn)))
        } else if let Some(local) = e.local_name {
            names.get(local).and_then(|local| {
                let lk = (p, local);
                aliases
                    .get(&lk)
                    .copied()
                    .or_else(|| symbols.contains_key(&lk).then_some(lk))
            })
        } else {
            None
        };
        let Some(o) = o else {
            if let Some(local) = e.local_name {
                exports.entry(key).or_default().insert((p, names.id(local)));
            }
            continue;
        };
        exports.entry(key).or_default().insert(o);
        named.insert(key, o);
        if o.0 != p
            && let Some(source) = file_ids.get(&p)
        {
            let (target_id, target) = reexport_target(o.0, &mut rs);
            new.add(
                edges,
                &mut rs,
                root,
                source,
                &target_id,
                "re_exports",
                ImportEvidence {
                    context: "export",
                    line: e.line,
                    path: e.file_path,
                    target_file: Some(&target),
                    alias: None,
                    type_only: e.type_only,
                },
            );
        }
    }
    let all = Nodes {
        base: nodes,
        added: &added_nodes,
    };
    let owned = all.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.retarget");
    let mut retargeted = false;
    for edge in edges.iter_mut().chain(new.added.iter_mut()) {
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
        for e in facts.exports() {
            let (Some(tp), Some(tn)) = (e.target_path, e.target_name) else {
                continue;
            };
            if rs.get(e.file_path) != source
                || format!("L{}", e.line) != s(edge, "source_location")
                || rs.get(tp) != rs.get(&path)
                || make_id([file_stem(&path).as_str(), tn]) != s(edge, "target")
            {
                continue;
            }
            let (cs, _) = candidates(
                &(rs.get(tp), names.id(tn)),
                &exports,
                &stars,
                &FxHashSet::default(),
            );
            if cs.len() == 1 {
                let key = cs.iter().next().unwrap();
                if let Some(id) = symbols
                    .get(key)
                    .map(|sym| sym_id(*sym, all, context_nodes))
                    .filter(|id| owned.contains(id))
                {
                    edge["target"] = json!(id);
                    edge["target_file"] = match path_map.get(&key.0) {
                        Some(&i) => json!(paths[i]),
                        None => json!(rs.path(key.0)),
                    };
                    retargeted = true;
                }
            }
            break;
        }
    }
    facts.free_exports();
    drop(path_map);
    // Retargeting above changed edge identities in place.
    if retargeted {
        let NewEdges { keys, added } = &mut new;
        keys.index(Edges { base: edges, added }, &mut rs, root);
    }
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.imports");
    for i in facts.imports() {
        if let Some(source) = file_ids.get(&rs.get(i.file_path)) {
            let o = origin(
                &(rs.get(i.target_path), names.id(i.imported_name)),
                &named,
                &stars,
                &symbols,
                &members,
                &mut FxHashSet::default(),
            );
            if let Some(sym) = symbols.get(&o) {
                new.add(
                    edges,
                    &mut rs,
                    root,
                    source,
                    sym_id(*sym, all, context_nodes),
                    "imports",
                    ImportEvidence {
                        context: "import",
                        line: i.line,
                        path: i.file_path,
                        target_file: None,
                        alias: None,
                        type_only: false,
                    },
                );
            }
        }
    }
    // The structural extractor names the imported symbol on the immediate
    // barrel, which defines nothing, so its edge dangles and build mints an
    // external stub beside the edge the loop above added to the defining
    // symbol (0.9.92, #4486). Drop it once the import resolved through the
    // barrel to a node the corpus owns; a chain ending outside the corpus
    // keeps its edge, and a barrel exporting one name from two modules keeps
    // the edge on the barrel (#2034).
    let mut superseded: FxHashSet<usize> = FxHashSet::default();
    {
        let mut sites: ImportSites<'_> = FxHashMap::default();
        for i in facts.imports() {
            let target = rs.get(i.target_path);
            sites
                .entry((rs.get(i.file_path), format!("L{}", i.line), target))
                .or_default()
                .push((target, i.imported_name));
        }
        for (index, edge) in edges.iter().enumerate() {
            if s(edge, "relation") != "imports" || owned.contains(s(edge, "target")) {
                continue;
            }
            let target_file = s(edge, "target_file");
            if target_file.is_empty() {
                continue;
            }
            let Some(source) = rs.source(s(edge, "source_file"), root) else {
                continue;
            };
            let target_path = PathBuf::from(target_file);
            let site = (
                source,
                s(edge, "source_location").to_string(),
                rs.get(&target_path),
            );
            let stem = file_stem(&target_path);
            for (barrel, name) in sites.get(&site).into_iter().flatten() {
                if s(edge, "target") != make_id([stem.as_str(), name]) {
                    continue;
                }
                let barrel_key = (*barrel, names.id(name));
                let (cs, _) = candidates(&barrel_key, &exports, &stars, &FxHashSet::default());
                if cs.len() == 1 {
                    let origin = *cs.iter().next().unwrap();
                    if origin != barrel_key
                        && symbols
                            .get(&origin)
                            .is_some_and(|sym| owned.contains(sym_id(*sym, all, context_nodes)))
                    {
                        superseded.insert(index);
                    }
                }
                break;
            }
        }
    }
    drop(exports);
    facts.free_imports();
    // #3777: a provisional AST `imports_from` edge whose package id collided
    // with a same-named sibling module file (`from nettacker import logger`
    // linking `nettacker.py`) is retracted when the submodule edge replaces it.
    // Only edges the pass was given qualify: it appends none of that kind
    // before this point.
    let mut provisional: FxHashMap<(P, u32), Vec<usize>> = FxHashMap::default();
    for (i, e) in edges.iter().enumerate() {
        if s(e, "relation") == "imports_from"
            && s(e, "context") == "import"
            && let Some(src) = rs.source(s(e, "source_file"), root)
        {
            let loc = s(e, "source_location");
            if !loc.is_empty() {
                provisional.entry((src, names.id(loc))).or_default().push(i);
            }
        }
    }
    let mut retracted = superseded;
    for (from, to, line, local) in facts.module_imports() {
        if let (Ok(from_rel), Ok(to_rel)) = (from.strip_prefix(root), to.strip_prefix(root)) {
            let src = make_id([file_stem(from_rel).as_str()]);
            let tgt = make_id([file_stem(to_rel).as_str()]);
            let candidates = module_collision_ids(from, to, root);
            let from_key = rs.get(from);
            let matched = names
                .get(&format!("L{line}"))
                .and_then(|loc| provisional.get(&(from_key, loc)))
                .into_iter()
                .flatten()
                .copied()
                .find(|i| !retracted.contains(i) && candidates.contains(s(&edges[*i], "target")));
            // The retracted edge's identity stays known: upstream discards a
            // four-part key that matches none of its five-part keys. The edge
            // itself is removed once the pass ends, so its number stays valid.
            if let Some(i) = matched {
                retracted.insert(i);
            }
            let alias = (Some(local.as_str()) != to.file_stem().and_then(|s| s.to_str()))
                .then_some(local.as_str());
            new.add(
                edges,
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
    facts.free_module_imports();
    drop(provisional);
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.uses");
    let mut apply_use = |u: UseRef<'_>| {
        let p = rs.get(u.file_path);
        let key = names.get(u.local_name).map(|name| (p, name));
        let target = key
            .and_then(|key| aliases.get(&key))
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
                if matches!(u.relation, "inherits" | "implements") {
                    key.and_then(|key| symbols.get(&key))
                } else {
                    None
                }
            });
        let Some(target) = target else { return };
        let target = sym_id(*target, all, context_nodes);
        let mut source = u.source_id;
        if matches!(u.relation, "inherits" | "implements" | "references") && !owned.contains(source)
        {
            return;
        }
        if u.relation == "calls" && !owned.contains(source) {
            let Some(id) = file_ids.get(&p) else { return };
            source = id;
        }
        new.add(
            edges,
            &mut rs,
            root,
            source,
            target,
            u.relation,
            ImportEvidence {
                context: u.context,
                line: u.line,
                path: u.file_path,
                target_file: None,
                alias: None,
                type_only: false,
            },
        );
    };
    for u in facts.js_uses() {
        apply_use(u);
    }
    // Consumed one by one, so each use is freed once it is applied.
    for u in facts.take_rest_uses() {
        apply_use(UseRef::from(&u));
    }
    drop(facts);
    drop(owned);
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.append");
    let NewEdges { keys, mut added } = new;
    drop(keys);
    drop((rs, names, symbols, members, aliases, named, stars, file_ids));
    if !retracted.is_empty() {
        let mut i = 0;
        edges.retain(|_| {
            i += 1;
            !retracted.contains(&(i - 1))
        });
    }
    edges.reserve_exact(added.len());
    edges.append(&mut added);
    nodes.reserve_exact(added_nodes.len());
    nodes.append(&mut added_nodes);
    drop(span);
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
    // The JS/TS facts stay in the compact per-file form the files' passes
    // derived them in; only Python's are collected whole.
    let span = crate::profile::span("assemble.symbol_resolution.collect_js");
    let js = super::resolution_facts::collect_js_files(paths);
    drop(span);
    let span = crate::profile::span("assemble.symbol_resolution.collect_python");
    let mut python = SymbolResolutionFacts::default();
    super::resolution_facts::collect_python(paths, root, &mut python, ambiguous);
    drop(span);
    let facts = PassFacts::new(js, python);
    apply_pass_facts(paths, nodes, edges, root, facts, context_nodes);
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
            let facts = PassFacts::new(Vec::new(), facts);
            apply_pass_facts(&paths, &mut nodes, &mut edges, root, facts, context_nodes);
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
#[cfg(test)]
mod pass_facts_tests {
    use super::*;

    /// The pass reads the JS/TS files' compact facts exactly as it reads the
    /// same facts collected whole, and the edge-identity set keeps the first
    /// edge of each identity.
    #[test]
    fn compact_js_facts_resolve_as_whole_facts() {
        let temp = tempfile::tempdir().unwrap();
        let root = resolved(temp.path());
        let write = |name: &str, text: &str| {
            let p = root.join(name);
            std::fs::write(&p, text).unwrap();
            p
        };
        let paths = vec![
            write(
                "a.ts",
                "export class Base {}\n\
                 export interface Shape { area(): number }\n\
                 export function helper(x: number): number { return x }\n\
                 export const alias = helper;\n\
                 export default Base;\n",
            ),
            write(
                "b.ts",
                "import { Base, helper, Shape } from './a';\n\
                 import Def from './a';\n\
                 export * from './a';\n\
                 export * as ns from './a';\n\
                 export { helper as h } from './a';\n\
                 const local = helper;\n\
                 export { local };\n\
                 export class Child extends Base implements Shape {\n\
                   field: Base;\n\
                   run(p: Base, q: Array<Shape>): Base { return p }\n\
                 }\n\
                 export function main() { helper(1); local(2); Def(); }\n",
            ),
            write(
                "c.ts",
                "import { h, ns, Child } from './b';\n\
                 export function go() { h(3); Child(); }\n",
            ),
        ];
        let node = |id: &str, label: &str, path: &Path| {
            json!({"id": id, "label": label, "file_type": "code",
                   "source_file": path, "source_location": "L1"})
        };
        // The ids the extractor gives the class and its method.
        let child = make_id([file_stem(&paths[1]).as_str(), "Child"]);
        let run = make_id([child.as_str(), "run"]);
        let base = make_id([file_stem(&paths[0]).as_str(), "Base"]);
        let file = make_id([paths[1].to_string_lossy().as_ref()]);
        let nodes = vec![
            node(&file, "b.ts", &paths[1]),
            node(&child, "Child", &paths[1]),
            node(&run, ".run()", &paths[1]),
        ];
        // An edge the pass would add again is kept once.
        let edges = vec![json!({
            "source": child, "target": base, "relation": "inherits",
            "context": "type", "source_file": paths[1],
        })];

        let (mut compact_nodes, mut compact_edges) = (nodes.clone(), edges.clone());
        let facts = PassFacts::new(
            super::super::resolution_facts::collect_js_files(&paths),
            SymbolResolutionFacts::default(),
        );
        apply_pass_facts(
            &paths,
            &mut compact_nodes,
            &mut compact_edges,
            &root,
            facts,
            &[],
        );

        let mut whole = SymbolResolutionFacts::default();
        super::super::resolution_facts::collect_js(&paths, &mut whole);
        assert!(!whole.uses.is_empty() && !whole.exports.is_empty());
        let (mut whole_nodes, mut whole_edges) = (nodes, edges);
        apply_symbol_resolution_facts(
            &paths,
            &mut whole_nodes,
            &mut whole_edges,
            &root,
            &whole,
            &[],
        );

        assert_eq!(compact_nodes, whole_nodes);
        assert_eq!(compact_edges, whole_edges);
        let relations = compact_edges
            .iter()
            .map(|e| s(e, "relation"))
            .collect::<BTreeSet<_>>();
        for relation in [
            "calls",
            "contains",
            "imports",
            "implements",
            "inherits",
            "re_exports",
            "references",
        ] {
            assert!(relations.contains(relation), "no {relation} edge");
        }
        let inherits = compact_edges
            .iter()
            .filter(|e| s(e, "relation") == "inherits")
            .count();
        assert_eq!(inherits, 1, "{compact_edges:#?}");
        let references = compact_edges
            .iter()
            .filter(|e| s(e, "relation") == "references")
            .map(|e| (s(e, "source"), s(e, "context")))
            .collect::<BTreeSet<_>>();
        for (source, context) in [
            (child.as_str(), "field"),
            (run.as_str(), "parameter_type"),
            (run.as_str(), "return_type"),
            (run.as_str(), "generic_arg"),
        ] {
            assert!(
                references.contains(&(source, context)),
                "no {context} reference from {source}: {references:?}"
            );
        }
    }
}
