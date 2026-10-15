// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Cross-file receiver binding for the application extraction pipeline.
use crate::{
    build::truthy,
    extract::per_file::PerFile,
    extract_merge::{family, label_key, rewire_unique_stub_edges},
    extractors::{
        base::{is_language_builtin, make_id},
        csharp::{CsharpNameResolver, is_cs_file},
        resolution::{is_type_like_definition, s},
    },
    python_compat::string,
    resolver_registry::{Rec, rs},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::rc::Rc;
type Index = BTreeMap<String, Vec<String>>;
type Fields = BTreeMap<String, BTreeMap<String, String>>;
fn key(s: &str, lang: &str) -> String {
    if lang == "java" {
        java_key(s)
    } else {
        label_key(s, true)
    }
}
/// Python's `str.strip()`.
fn py_strip(s: &str) -> &str {
    s.trim_matches(crate::dedup_strings::whitespace)
}
/// The Java resolver's key: `label.strip().removeprefix(".").removesuffix("()")`.
fn java_key(s: &str) -> String {
    let s = py_strip(s);
    let s = s.strip_prefix('.').unwrap_or(s);
    s.strip_suffix("()").unwrap_or(s).to_string()
}
fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}
pub fn park<R: Rec>(node: Option<&mut Value>, callee: &str, receiver: &str, lang: &str, raw: &R) {
    let Some(n) = node else { return };
    if callee.is_empty() || receiver.is_empty() {
        return;
    }
    if n.get("metadata").is_none() {
        n["metadata"] = json!({});
    }
    let Some(meta) = n["metadata"].as_object_mut() else {
        return;
    };
    let calls = meta.entry("unresolved_calls").or_insert(json!([]));
    let Some(calls) = calls.as_array_mut() else {
        return;
    };
    if calls.len() >= 64
        || calls
            .iter()
            .any(|v| s(v, "callee") == callee && s(v, "receiver_type") == receiver)
    {
        return;
    }
    let mut v = json!({"callee":callee,"receiver_type":receiver,"lang":lang});
    if !rs(raw, "source_location").is_empty() {
        v["line"] = raw
            .rec_get("source_location")
            .cloned()
            .unwrap_or(Value::Null);
    }
    calls.push(v);
}
/// Calls to park on their callers' metadata: (caller, callee, receiver type,
/// raw call), in a resolver's loop order.
type Parked<'a, R> = Vec<(String, String, String, &'a R)>;
/// Apply the parks a resolver collected, in its loop order: nothing a
/// resolver reads changes when a call is parked on its caller's metadata.
/// The caller is the last node with its id, as the reference's
/// `node_by_id` map has it; the index is built once, not searched per park.
fn apply_parks<R: Rec>(nodes: &mut [Value], parked: Parked<'_, R>, lang: &str) {
    if parked.is_empty() {
        return;
    }
    let wanted = parked
        .iter()
        .map(|(caller, ..)| caller.as_str())
        .collect::<FxHashSet<_>>();
    let mut index: FxHashMap<String, usize> = FxHashMap::default();
    for (i, n) in nodes.iter().enumerate() {
        if wanted.contains(s(n, "id")) {
            index.insert(s(n, "id").to_string(), i);
        }
    }
    drop(wanted);
    for (caller, callee, ty, raw) in parked {
        let node = index.get(&caller).map(|&i| &mut nodes[i]);
        park(node, &callee, &ty, lang, raw);
    }
}
/// Emitted (caller, target) pairs; membership only, so hashed. Each distinct
/// id text is numbered once and a pair is two integers, not two string
/// copies: the table starts with every edge (Kubernetes: 1.6 million pairs,
/// about 300 MB as strings). Equal texts get equal numbers, so every answer
/// is the one the strings would give. The id texts are owned (`Box<str>`) by
/// default; a resolver whose ids all borrow from the records, nodes and
/// edges it reads keys them by `&str` instead and copies no text at all.
struct Seen<K = Box<str>> {
    ids: FxHashMap<K, u32>,
    pairs: FxHashSet<(u32, u32)>,
}
/// An id text as a [`Seen`] key: an owned copy, or the borrowed text itself.
trait SeenKey<'a>: std::borrow::Borrow<str> + std::hash::Hash + Eq {
    fn make(text: &'a str) -> Self;
}
impl<'a> SeenKey<'a> for Box<str> {
    fn make(text: &'a str) -> Self {
        text.into()
    }
}
impl<'a> SeenKey<'a> for &'a str {
    fn make(text: &'a str) -> Self {
        text
    }
}
impl<K> Default for Seen<K> {
    fn default() -> Self {
        Self {
            ids: FxHashMap::default(),
            pairs: FxHashSet::default(),
        }
    }
}
impl<K: std::borrow::Borrow<str> + std::hash::Hash + Eq> Seen<K> {
    fn id<'a>(&mut self, text: &'a str) -> u32
    where
        K: SeenKey<'a>,
    {
        if let Some(&id) = self.ids.get(text) {
            return id;
        }
        let id = u32::try_from(self.ids.len()).expect("fewer than 2^32 distinct ids");
        self.ids.insert(K::make(text), id);
        id
    }
    /// Adds the pair; `true` when it was not there before.
    fn insert<'a>(&mut self, source: &'a str, target: &'a str) -> bool
    where
        K: SeenKey<'a>,
    {
        let pair = (self.id(source), self.id(target));
        self.pairs.insert(pair)
    }
    fn contains(&self, source: &str, target: &str) -> bool {
        match (self.ids.get(source), self.ids.get(target)) {
            (Some(&source), Some(&target)) => self.pairs.contains(&(source, target)),
            _ => false,
        }
    }
}
/// Collects a call edge into `added`. The resolvers append `added` to the
/// edge list once their tables are gone, growing it by exactly that many
/// edges: one push at a time would double the whole list for a few calls.
fn emit<R: Rec>(
    added: &mut Vec<Value>,
    seen: &mut Seen,
    raw: &R,
    target: &str,
    relation: &str,
    exact: bool,
) {
    emit_with(added, seen, raw, target, relation, exact);
}
/// [`emit`] over a [`Seen`] keyed by any id text type (borrowed ids copy no
/// text).
fn emit_with<'a, R: Rec, K: SeenKey<'a>>(
    added: &mut Vec<Value>,
    seen: &mut Seen<K>,
    raw: &'a R,
    target: &'a str,
    relation: &str,
    exact: bool,
) {
    let caller = rs(raw, "caller_nid");
    if target.is_empty() || target == caller || !seen.insert(caller, target) {
        return;
    }
    added.push(json!({"source":caller,"target":target,"relation":relation,"context":"call","confidence":if exact{"EXTRACTED"}else{"INFERRED"},"confidence_score":if exact{1.0}else{0.85},"source_file":rs(raw,"source_file"),"source_location":raw.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
}
/// Append the edges a resolver collected (JSON objects from its `json!`
/// literals) to the edge list in its own form, growing it by exactly that
/// many: a record keeps the literal's key order and number texts.
fn append<E: Rec>(edges: &mut Vec<E>, added: Vec<Value>) {
    edges.reserve_exact(added.len());
    edges.extend(added.into_iter().map(|edge| match edge {
        Value::Object(map) => E::from_object(map),
        _ => unreachable!("an edge literal is an object"),
    }));
}
/// `record[key]` (`Null` when absent), on a raw call or edge of either form.
fn at<'a, R: Rec>(record: &'a R, key: &str) -> &'a Value {
    record.rec_get(key).unwrap_or(&NULL)
}
static NULL: Value = Value::Null;
/// Every raw call record of the per-file results (in either form), in file
/// order.
fn raw_calls_of<P: PerFile>(per: &[P]) -> impl Iterator<Item = &P::R> {
    per.iter().flat_map(|r| r.raw_calls().into_iter().flatten())
}
fn pairs<'a, K: SeenKey<'a>, E: Rec>(edges: &'a [E], calls_only: bool) -> Seen<K> {
    let mut seen = Seen::default();
    for e in edges
        .iter()
        .filter(|e| !calls_only || rs(*e, "relation") == "calls")
    {
        seen.insert(rs(e, "source"), rs(e, "target"));
    }
    seen
}
fn inherited_field(fields: &Fields, bases: &Index, owner: &str, field: &str) -> Option<String> {
    let mut q = VecDeque::from([owner.to_string()]);
    let mut seen = BTreeSet::new();
    while let Some(c) = q.pop_front() {
        if c.is_empty() || !seen.insert(c.clone()) {
            continue;
        }
        if let Some(t) = fields.get(&c).and_then(|t| t.get(field)) {
            return Some(t.clone());
        }
        q.extend(bases.get(&c).into_iter().flatten().cloned());
    }
    None
}
/// `_bind_member_field_tables`: each per-file table (keyed by class label
/// and source file, so it survives every id remap) bound to the one class
/// node carrying that label, disambiguated by source file, then basename. An
/// entry that stays ambiguous binds nothing; a name two tables disagree on
/// is dropped.
pub fn bind_field_table_values<P: PerFile>(
    per: &[P],
    nodes: &[Value],
    lang: &str,
) -> BTreeMap<String, BTreeMap<String, Value>> {
    let mut labels: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for n in nodes {
        if !s(n, "source_file").is_empty() && !s(n, "label").is_empty() {
            labels.entry(s(n, "label")).or_default().push(n);
        }
    }
    let mut out: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for entry in per
        .iter()
        .flat_map(|r| r.at("member_field_tables").as_array().into_iter().flatten())
    {
        if s(entry, "lang") != lang || s(entry, "class_label").is_empty() {
            continue;
        }
        let Some(fields) = entry["fields"].as_object() else {
            continue;
        };
        let mut candidates = labels
            .get(s(entry, "class_label"))
            .cloned()
            .unwrap_or_default();
        if candidates.len() > 1 {
            let exact = candidates
                .iter()
                .filter(|n| s(n, "source_file") == s(entry, "source_file"))
                .copied()
                .collect::<Vec<_>>();
            if exact.len() == 1 {
                candidates = exact
            } else {
                let base = Path::new(s(entry, "source_file")).file_name();
                candidates.retain(|n| Path::new(s(n, "source_file")).file_name() == base);
            }
        }
        if candidates.len() != 1 {
            continue;
        }
        let table = out.entry(s(candidates[0], "id").into()).or_default();
        for (name, ty) in fields {
            if table.get(name).is_some_and(|p| p != ty) {
                table.remove(name);
            } else {
                table.insert(name.clone(), ty.clone());
            }
        }
    }
    out
}
/// [`bind_field_table_values`] for tables of type names.
pub fn bind_field_tables<P: PerFile>(per: &[P], nodes: &[Value], lang: &str) -> Fields {
    let mut labels: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for n in nodes {
        if !s(n, "source_file").is_empty() && !s(n, "label").is_empty() {
            labels.entry(s(n, "label")).or_default().push(n);
        }
    }
    let mut out = Fields::new();
    for entry in per
        .iter()
        .flat_map(|r| r.at("member_field_tables").as_array().into_iter().flatten())
    {
        if s(entry, "lang") != lang || s(entry, "class_label").is_empty() {
            continue;
        }
        let Some(fields) = entry["fields"].as_object() else {
            continue;
        };
        let mut candidates = labels
            .get(s(entry, "class_label"))
            .cloned()
            .unwrap_or_default();
        if candidates.len() > 1 {
            let exact = candidates
                .iter()
                .filter(|n| s(n, "source_file") == s(entry, "source_file"))
                .copied()
                .collect::<Vec<_>>();
            if exact.len() == 1 {
                candidates = exact
            } else {
                let base = Path::new(s(entry, "source_file")).file_name();
                candidates.retain(|n| Path::new(s(n, "source_file")).file_name() == base);
            }
        }
        if candidates.len() != 1 {
            continue;
        }
        let table = out.entry(s(candidates[0], "id").into()).or_default();
        for (name, ty) in fields {
            let Some(ty) = ty.as_str() else { continue };
            if table.get(name).is_some_and(|p| p != ty) {
                table.remove(name);
            } else {
                table.insert(name.clone(), ty.into());
            }
        }
    }
    out
}
pub fn resolve_members<P: PerFile, E: Rec>(
    lang: &str,
    per: &[P],
    nodes: &mut [Value],
    edges: &mut Vec<E>,
) {
    match lang {
        "python" => return python_members(per, nodes, edges),
        "csharp" => return csharp_members(per, nodes, edges),
        "java" => return java_members(per, nodes, edges),
        "php" => return php_members(per, nodes, edges),
        _ => {}
    }
    let Some((added, parked)) = typed_member_edges(lang, per, nodes, edges) else {
        return;
    };
    // Every table borrowed the nodes and edges and is gone: grow by exactly
    // the new edges, then park (nothing above reads a node's metadata list
    // of unresolved calls, so the parks apply now, in the same order).
    append(edges, added);
    apply_parks(nodes, parked, lang);
}
/// The call edges and parks of [`resolve_members`] for Swift, TypeScript,
/// C++ and Objective-C, in raw-call order; `None` when a Swift or TypeScript
/// corpus has no receiver type table at all.
///
/// Every id in its tables borrows from the nodes and edges it reads (only the
/// `key()` texts are built), and each file's receiver table is the per-file
/// JSON object itself: the ordered tables order `&str` exactly as they
/// ordered `String`, and the others are only looked up.
fn typed_member_edges<'p, P: PerFile, E: Rec>(
    lang: &str,
    per: &'p [P],
    nodes: &[Value],
    edges: &[E],
) -> Option<(Vec<Value>, Parked<'p, P::R>)> {
    let table_key = match lang {
        "swift" => "swift_type_table",
        "typescript" => "ts_type_table",
        "cpp" => "cpp_type_table",
        "objc" => "objc_type_table",
        _ => "",
    };
    // A file's receiver -> type table: its string entries (a later table of
    // the same path replaces an earlier one).
    let mut tables: FxHashMap<&str, &serde_json::Map<String, Value>> = FxHashMap::default();
    let mut factories: BTreeMap<&str, &Value> = BTreeMap::new();
    for r in per {
        let entry = r.at(table_key);
        if let Some(t) = entry["table"].as_object() {
            let path = s(entry, "path");
            if !path.is_empty() {
                tables.insert(path, t);
                if entry["factory"].is_object() {
                    factories.insert(path, &entry["factory"]);
                }
            }
        }
    }
    // TS class fields live on class nodes (`_ts_fields`, `_ts_static_fields`).
    // One id can carry several nodes (`class UsecaseVisitor` and `const
    // usecaseVisitor` fold together), so the fields of every node with that
    // id are gathered; the first type given for a name wins.
    let mut ts_fields: FxHashMap<(String, bool), BTreeMap<String, String>> = FxHashMap::default();
    if lang == "typescript" {
        for n in nodes.iter() {
            for (is_static, attr) in [(false, "_ts_fields"), (true, "_ts_static_fields")] {
                for (name, ty) in n.get(attr).and_then(Value::as_object).into_iter().flatten() {
                    let Some(ty) = ty.as_str() else { continue };
                    ts_fields
                        .entry((s(n, "id").to_string(), is_static))
                        .or_default()
                        .entry(name.clone())
                        .or_insert_with(|| ty.to_string());
                }
            }
        }
    }
    if lang == "swift" && tables.is_empty()
        || lang == "typescript" && tables.is_empty() && ts_fields.is_empty()
    {
        return None;
    }
    // The nodes as they were on entry, by id (the last of equal ids).
    let snapshot = nodes
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<FxHashMap<_, _>>();
    let contained = edges
        .iter()
        .filter(|e| rs(*e, "relation") == "contains")
        .map(|e| rs(e, "target"))
        .collect::<FxHashSet<_>>();
    let mut defs: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for n in nodes {
        // Only the JS/TS interop family declares a type a TS receiver can
        // hold (SFC script blocks included).
        let declared = if lang == "typescript" {
            family(s(n, "source_file")) == Some("jsts")
        } else {
            !s(n, "source_file").is_empty()
        };
        if declared
            && contained.contains(s(n, "id"))
            && is_type_like_definition(n)
            && !(lang == "objc" && s(n, "label").starts_with('<') && s(n, "label").ends_with('>'))
        {
            defs.entry(key(s(n, "label"), lang))
                .or_default()
                .push(s(n, "id"));
        }
    }
    drop(contained);
    let mut methods: BTreeMap<(&str, String), &str> = BTreeMap::new();
    let mut enclosing: BTreeMap<&str, &str> = BTreeMap::new();
    // TS: the class of each method (the last `method` edge names it).
    let mut class_of_method: FxHashMap<&str, &str> = FxHashMap::default();
    if lang == "typescript" {
        for e in edges.iter().filter(|e| rs(*e, "relation") == "method") {
            if at(e, "source").is_string() && at(e, "target").is_string() {
                class_of_method.insert(rs(e, "target"), rs(e, "source"));
            }
        }
    }
    let mut bases: Index = BTreeMap::new();
    let mut file_of: BTreeMap<&str, &str> = BTreeMap::new();
    let mut imports: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for rel in if lang == "cpp" {
        vec!["defines", "method"]
    } else {
        vec!["method"]
    } {
        for e in edges.iter().filter(|e| rs(*e, "relation") == rel) {
            if let Some(n) = snapshot.get(rs(e, "target")) {
                enclosing.entry(rs(e, "target")).or_insert(rs(e, "source"));
                methods.insert((rs(e, "source"), key(s(n, "label"), lang)), rs(e, "target"));
            }
        }
    }
    for e in edges {
        match rs(e, "relation") {
            "inherits" => {
                bases
                    .entry(rs(e, "source").into())
                    .or_default()
                    .push(rs(e, "target").into());
            }
            "contains" => {
                file_of.insert(rs(e, "target"), rs(e, "source"));
            }
            "imports" | "imports_from" => {
                imports
                    .entry(rs(e, "source"))
                    .or_default()
                    .insert(rs(e, "target"));
            }
            _ => {}
        }
    }
    for (&method, &owner) in &enclosing {
        if let Some(&file) = file_of.get(owner) {
            file_of.entry(method).or_insert(file);
        }
    }
    let mut fields = Fields::new();
    if lang == "objc" {
        let mut conflicts = BTreeSet::new();
        for r in per {
            if let Some(ts) = r.at("objc_field_types")["tables"].as_object() {
                for (cls, fs) in ts {
                    for (f, t) in fs.as_object().into_iter().flatten() {
                        let Some(t) = t.as_str() else { continue };
                        if conflicts.contains(&(cls.clone(), f.clone())) {
                            continue;
                        }
                        let map = fields.entry(cls.clone()).or_default();
                        if map.get(f).is_some_and(|old| old != t) {
                            map.remove(f);
                            conflicts.insert((cls.clone(), f.clone()));
                        } else {
                            map.insert(f.clone(), t.into());
                        }
                    }
                }
            }
        }
    }
    // Swift receivers typed by a factory's plain return type, where the
    // file's own table has no type for them (path, receiver) -> type.
    let mut factory_types: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    if lang == "swift" {
        let mut returns: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for e in edges {
            if rs(e, "relation") == "references"
                && rs(e, "context") == "return_type"
                && at(e, "metadata")["swift_plain_return"] == true
            {
                returns
                    .entry(rs(e, "source"))
                    .or_default()
                    .insert(rs(e, "target"));
            }
        }
        for (path, pending) in factories {
            for (receiver, bind) in pending.as_object().into_iter().flatten() {
                let Some(pair) = bind.as_array().filter(|a| a.len() == 2) else {
                    continue;
                };
                let Some(ty) = pair[0].as_str() else { continue };
                let Some(method) = pair[1].as_str() else {
                    continue;
                };
                if is_language_builtin(ty) {
                    continue;
                }
                let Some(ds) = defs.get(&key(ty, lang)).filter(|d| d.len() == 1) else {
                    continue;
                };
                let Some(ms) = methods.get(&(ds[0], key(method, lang))) else {
                    continue;
                };
                let Some(rs) = returns.get(ms).filter(|d| d.len() == 1) else {
                    continue;
                };
                let Some(n) = snapshot.get(*rs.iter().next().unwrap()) else {
                    continue;
                };
                let label = s(n, "label");
                if label.is_empty()
                    || is_language_builtin(label)
                    || defs.get(&key(label, lang)).is_none_or(|d| d.len() != 1)
                {
                    continue;
                }
                let typed = tables
                    .get(path)
                    .and_then(|t| t.get(receiver))
                    .is_some_and(Value::is_string);
                if !typed {
                    factory_types
                        .entry((path, receiver.as_str()))
                        .or_insert(label);
                }
            }
        }
    }
    // TS `class B extends A`: a field A declares is B's too. Bases are
    // matched by their node, else by the one class of that label carrying
    // fields.
    let mut field_classes: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    if lang == "typescript" {
        for n in nodes {
            let has = |attr: &str| n.get(attr).is_some_and(truthy);
            if has("_ts_fields") || has("_ts_static_fields") {
                let entry = field_classes.entry(key(s(n, "label"), lang)).or_default();
                if !entry.contains(&s(n, "id")) {
                    entry.push(s(n, "id"));
                }
            }
        }
    }
    let field_type = |class: &str, field: &str, is_static: bool| -> Option<(String, String)> {
        #[allow(clippy::too_many_arguments)]
        fn walk(
            class: &str,
            field: &str,
            is_static: bool,
            depth: usize,
            ts_fields: &FxHashMap<(String, bool), BTreeMap<String, String>>,
            bases: &Index,
            field_classes: &BTreeMap<String, Vec<&str>>,
            snapshot: &FxHashMap<&str, &Value>,
        ) -> Option<(String, String)> {
            if let Some(found) = ts_fields
                .get(&(class.to_string(), is_static))
                .and_then(|f| f.get(field))
                .filter(|t| !t.is_empty())
            {
                return Some((found.clone(), class.to_string()));
            }
            if depth >= 8 {
                return None;
            }
            for base in bases.get(class).into_iter().flatten() {
                let base_nid = if ts_fields.contains_key(&(base.clone(), is_static)) {
                    Some(base.as_str())
                } else {
                    let label = snapshot.get(base.as_str()).map_or("", |n| s(n, "label"));
                    field_classes
                        .get(&key(label, "typescript"))
                        .filter(|same| same.len() == 1)
                        .map(|same| same[0])
                };
                if let Some(base_nid) = base_nid
                    && let Some(hit) = walk(
                        base_nid,
                        field,
                        is_static,
                        depth + 1,
                        ts_fields,
                        bases,
                        field_classes,
                        snapshot,
                    )
                {
                    return Some(hit);
                }
            }
            None
        }
        walk(
            class,
            field,
            is_static,
            0,
            &ts_fields,
            &bases,
            &field_classes,
            &snapshot,
        )
    };
    let table_type = |file: &str, receiver: &str| -> String {
        tables
            .get(file)
            .and_then(|t| t.get(receiver))
            .and_then(Value::as_str)
            .or_else(|| factory_types.get(&(file, receiver)).copied())
            .unwrap_or_default()
            .to_string()
    };
    // (caller, callee, type, raw call) to park, in loop order.
    let mut parked = Vec::new();
    let type_lookup = |name: &str| {
        if name.is_empty() {
            return None;
        }
        defs.get(&key(name, lang))
            .filter(|d| d.len() == 1)
            .map(|d| d[0])
    };
    let mut seen: Seen<&str> = pairs(edges, false);
    let mut added = vec![];
    for raw in raw_calls_of(per) {
        if at(raw, "is_member_call") != true {
            continue;
        }
        if matches!(lang, "cpp" | "objc") && rs(raw, "lang") != lang {
            continue;
        }
        let (receiver, callee, caller, file) = (
            rs(raw, "receiver"),
            rs(raw, "callee"),
            rs(raw, "caller_nid"),
            rs(raw, "source_file"),
        );
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let mut exact = upper(receiver);
        let mut ty = String::new();
        let mut direct = None;
        // TS: the class whose field typed the receiver; the origin gate reads
        // that class's file (`class Child extends Base` need not import what
        // `Base` holds).
        let mut declaring: Option<String> = None;
        let this_owner =
            (lang == "typescript" && !upper(receiver) && truthy(at(raw, "ts_this_field")))
                .then(|| class_of_method.get(caller).copied())
                .flatten();
        if let Some(owner) = this_owner {
            // `this.x` in a method of a known class reads that class's fields,
            // then its bases': never the file's table.
            if let Some((found, class)) =
                field_type(owner, receiver, truthy(at(raw, "ts_static_this")))
            {
                ty = found;
                declaring = Some(class);
            }
        } else if lang == "cpp" && receiver == "this" {
            direct = enclosing.get(caller).copied();
            exact = true;
            if direct.is_none() {
                continue;
            }
        } else if lang == "objc" && rs(raw, "receiver_kind") == "self_field" {
            ty = inherited_field(
                &fields,
                &bases,
                enclosing.get(caller).copied().unwrap_or(""),
                receiver,
            )
            .unwrap_or_default();
            exact = false;
        } else if lang == "objc" && matches!(receiver, "self" | "super") {
            direct = enclosing.get(caller).copied();
            exact = true;
            if direct.is_none() {
                continue;
            }
        } else if upper(receiver) {
            ty = receiver.into();
        } else {
            ty = table_type(file, receiver);
            if ty.is_empty() && lang == "objc" {
                ty = inherited_field(
                    &fields,
                    &bases,
                    enclosing.get(caller).copied().unwrap_or(""),
                    receiver,
                )
                .unwrap_or_default();
            }
        }
        if matches!(lang, "swift" | "typescript") && is_language_builtin(&ty) {
            continue;
        }
        if direct.is_none() {
            if ty.is_empty() {
                continue;
            }
            if lang == "typescript" {
                if defs.get(&key(&ty, lang)).is_none_or(Vec::is_empty) {
                    // Table-typed only (an uppercase receiver also matches
                    // namespace aliases and npm default imports): park it for
                    // a merged graph (#3152).
                    if !upper(receiver) && family(file) == Some("jsts") {
                        parked.push((caller.to_string(), callee.to_string(), ty, raw));
                    }
                    continue;
                }
                // Origin gate (#2553), which also picks among same-named
                // definitions (#4233): the one the caller's file defines,
                // imports by name, or imports the module of. Exactly one
                // visible definition binds; none or several emit nothing.
                let cf = file_of.get(declaring.as_deref().unwrap_or(caller));
                let imported = cf.and_then(|f| imports.get(f));
                let visible = defs
                    .get(&key(&ty, lang))
                    .into_iter()
                    .flatten()
                    .filter(|nid| {
                        let tf = file_of.get(**nid);
                        (cf.is_some() && cf == tf)
                            || imported.is_some_and(|i| {
                                i.contains(**nid) || tf.is_some_and(|f| i.contains(f))
                            })
                    })
                    .collect::<Vec<_>>();
                if visible.len() != 1 {
                    continue;
                }
                direct = Some(*visible[0]);
            } else {
                direct = type_lookup(&ty);
            }
        }
        let Some(owner) = direct else {
            let absent = defs.get(&key(&ty, lang)).is_none_or(Vec::is_empty);
            // A builtin ObjC type (NSString, ...) is not what another repo
            // declares (#3152).
            if absent
                && (lang == "cpp"
                    || (lang == "swift" && file.to_lowercase().ends_with(".swift"))
                    || (lang == "objc" && !is_language_builtin(&ty)))
            {
                parked.push((caller.to_string(), callee.to_string(), ty, raw));
            }
            continue;
        };
        let mut target = methods.get(&(owner, key(callee, lang))).copied();
        if lang == "typescript"
            && let Some(method) = target.and_then(|t| snapshot.get(t))
            && s(method, "label").trim_start_matches('.').starts_with('#')
            && !callee.starts_with('#')
            && !method.get("_ts_public_twin").is_some_and(truthy)
        {
            // `foo` and `#foo` share a node labelled after the first one
            // declared: a public `c.foo()` reaches a `#foo` node only when the
            // class also declares a public `foo` (#4233, #4298).
            continue;
        }
        if lang == "cpp" && target.is_none() && upper(receiver) {
            let label = format!("{receiver}::{callee}()");
            let candidates = nodes
                .iter()
                .filter(|n| !s(n, "source_file").is_empty() && s(n, "label") == label)
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                target = Some(s(candidates[0], "id"));
            }
        }
        if let Some(target) = target {
            emit_with(&mut added, &mut seen, raw, target, "calls", exact);
        } else if matches!(lang, "swift" | "cpp" | "objc") {
            emit_with(&mut added, &mut seen, raw, owner, "references", exact);
        }
    }
    Some((added, parked))
}
/// `_resolve_csharp_member_calls` (#1609, #1620): a member call bound to its
/// receiver's declared type with the namespace/using/alias scoping of the C#
/// type-reference pass, the method looked up through the `inherits` chain
/// (an out-of-corpus base poisons the lookup). Receivers: `this`/`base`
/// (constructor bodies count as the type, #4246), an explicitly named type
/// whose same-named field or property shadows it (#3797), a field chain
/// `x.F.M()` (#4246), a `var` typed from the called method's declared return
/// type (#4266), or the extractor's method-scoped type table.
pub fn csharp_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    // Case and `_` are part of a C# name: `Put` and `_Put` are two methods,
    // so `this.Put()` must not bind to `_Put` (#4415). Only the label
    // decoration (`.Put()`) goes: `re.sub(r"\W+", "", label)`.
    let ckey = |label: &str| {
        label
            .chars()
            .filter(|c| crate::export::python_word(*c))
            .collect::<String>()
    };
    let nodes_ro: &[Value] = nodes;
    let edges_ro: &[E] = edges;
    let contained = edges_ro
        .iter()
        .filter(|e| rs(*e, "relation") == "contains")
        .map(|e| rs(e, "target"))
        .collect::<FxHashSet<_>>();
    let mut type_defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut node_by_id: FxHashMap<&str, &Value> = FxHashMap::default();
    for n in nodes_ro {
        node_by_id.insert(s(n, "id"), n);
        if !s(n, "source_file").is_empty()
            && contained.contains(s(n, "id"))
            && is_type_like_definition(n)
        {
            type_defs
                .entry(ckey(s(n, "label")))
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let resolver = CsharpNameResolver::new_of(nodes_ro, edges_ro);
    let mut method_index: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut enclosing: BTreeMap<String, String> = BTreeMap::new();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        let Some(method) = node_by_id.get(rs(e, "target")) else {
            continue;
        };
        enclosing
            .entry(rs(e, "target").into())
            .or_insert_with(|| rs(e, "source").into());
        method_index.insert(
            (rs(e, "source").into(), ckey(s(method, "label"))),
            rs(e, "target").into(),
        );
    }
    let mut bases_of: Index = BTreeMap::new();
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "inherits") {
        if !is_cs_file(at(e, "source_file"))
            || !at(e, "source").is_string()
            || !at(e, "target").is_string()
        {
            continue;
        }
        let (src, tgt) = (rs(e, "source"), rs(e, "target"));
        if node_by_id
            .get(tgt)
            .is_none_or(|n| !n.get("source_file").is_some_and(truthy))
        {
            unresolved.insert(src.into());
        } else {
            let bucket = bases_of.entry(src.into()).or_default();
            if !bucket.iter().any(|b| b == tgt) {
                bucket.push(tgt.into());
            }
        }
    }
    let method_on_type_or_bases = |type_nid: &str, callee_key: &str| -> Option<String> {
        let mut hits = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut frontier = vec![type_nid.to_string()];
        while let Some(nid) = frontier.pop() {
            if !seen.insert(nid.clone()) {
                continue;
            }
            if let Some(method) = method_index
                .get(&(nid.clone(), callee_key.to_string()))
                .filter(|m| !m.is_empty())
            {
                hits.insert(method.clone());
                continue;
            }
            if unresolved.contains(&nid) {
                return None;
            }
            frontier.extend(bases_of.get(&nid).into_iter().flatten().cloned());
        }
        (hits.len() == 1).then(|| hits.into_iter().next().unwrap())
    };
    let resolve_type = |name: &str, caller: Option<&Value>, file: &str| -> Option<String> {
        if name.is_empty() {
            return None;
        }
        if let Some(caller) = caller {
            let (hit, decisive) = resolver.resolve_type_name(name, caller, file);
            if hit.is_some() {
                return hit;
            }
            if decisive {
                return None;
            }
        }
        type_defs
            .get(&ckey(name))
            .filter(|d| d.len() == 1)
            .map(|d| d[0].clone())
    };
    let absent =
        |name: &str| !name.is_empty() && type_defs.get(&ckey(name)).is_none_or(Vec::is_empty);
    let class_fields = bind_field_tables(per, nodes_ro, "csharp");
    let type_nids = type_defs
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>();
    let own_type = |caller: &str| {
        enclosing
            .get(caller)
            .cloned()
            .or_else(|| type_nids.contains(caller).then(|| caller.to_string()))
    };
    let field_type_nid = |type_nid: &str, field: &str| -> Option<String> {
        let mut hits = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut frontier = vec![type_nid.to_string()];
        while let Some(nid) = frontier.pop() {
            if !seen.insert(nid.clone()) {
                continue;
            }
            let fields = class_fields.get(&nid);
            if let Some(ty) = fields.and_then(|f| f.get(field)) {
                if fields.is_some_and(|f| f.contains_key(&format!("{field}[]"))) {
                    return None;
                }
                hits.insert((nid.clone(), ty.clone()));
                continue;
            }
            if unresolved.contains(&nid) {
                return None;
            }
            frontier.extend(bases_of.get(&nid).into_iter().flatten().cloned());
        }
        if hits.len() != 1 {
            return None;
        }
        let (owner, ty) = hits.into_iter().next().unwrap();
        let owner = node_by_id.get(owner.as_str()).copied();
        resolve_type(&ty, owner, owner.map_or("", |o| s(o, "source_file")))
    };
    let method_returns = bind_field_table_values(per, nodes_ro, "csharp_returns");
    // The type a `var` gets from `R.M<A>()`: M's declared return type, or the
    // call site's type argument when M returns its own type parameter.
    let call_return_type =
        |call: &Value, caller: &str, caller_node: Option<&Value>, file: &str| -> Option<String> {
            let [recv, method, targs] = call.as_array()?.as_slice() else {
                return None;
            };
            let recv = recv.as_str().unwrap_or("");
            let method = method.as_str()?;
            let targs = targs.as_array().map(Vec::as_slice).unwrap_or_default();
            let owner = if recv == "this" || recv.is_empty() {
                own_type(caller)
            } else {
                resolve_type(recv, caller_node, file)
            }?;
            let method_nid = method_on_type_or_bases(&owner, &ckey(method))?;
            let decl_type = enclosing.get(&method_nid)?;
            let mut decls = method_returns
                .get(decl_type)
                .and_then(|returns| returns.get(method))?
                .as_array()?
                .iter()
                .collect::<Vec<_>>();
            if !targs.is_empty() {
                decls.retain(|d| d[1].as_u64() == Some(targs.len() as u64));
            }
            let [decl] = decls.as_slice() else {
                return None;
            };
            let ret = &decl[0];
            if let Some(index) = ret.as_str().and_then(|r| r.strip_prefix('#')) {
                let index = index.parse::<usize>().ok()?;
                let ret = targs.get(index)?.as_str()?;
                return resolve_type(ret, caller_node, file);
            }
            let decl_node = node_by_id.get(decl_type.as_str()).copied();
            resolve_type(
                ret.as_str()?,
                decl_node,
                decl_node.map_or("", |n| s(n, "source_file")),
            )
        };
    let mut parked = Vec::new();
    let mut added = vec![];
    let mut seen: Seen = pairs(edges_ro, false);
    for rc in raw_calls_of(per) {
        if rs(rc, "lang") != "csharp" || !rc.rec_get("is_member_call").is_some_and(truthy) {
            continue;
        }
        let (receiver, callee, caller) =
            (rs(rc, "receiver"), rs(rc, "callee"), rs(rc, "caller_nid"));
        let chain = at(rc, "receiver_chain")
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if (receiver.is_empty() && chain.is_empty()) || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let file = rs(rc, "source_file");
        let caller_node = node_by_id.get(caller).copied();
        let receiver_type = rs(rc, "receiver_type");
        let (type_nid, qualified) = if receiver.is_empty() {
            // `x.F.M()` -> [type of x, "F"]; `xs[i].M()` -> [element type].
            let mut type_nid = resolve_type(chain[0].as_str().unwrap_or(""), caller_node, file);
            for field in &chain[1..] {
                let Some(current) = type_nid else { break };
                type_nid = field_type_nid(&current, field.as_str().unwrap_or(""));
            }
            let Some(type_nid) = type_nid else { continue };
            (type_nid, false)
        } else if receiver == "this" {
            let Some(type_nid) = own_type(caller) else {
                continue;
            };
            (type_nid, true)
        } else if receiver == "base" {
            let Some(own) = own_type(caller).filter(|t| !unresolved.contains(t)) else {
                continue;
            };
            let Some(base) = bases_of.get(&own).filter(|b| b.len() == 1) else {
                continue;
            };
            (base[0].clone(), true)
        } else if upper(receiver) {
            // `Type.M()` names the type, but a same-named field, property or
            // local in scope shadows it: its declared type wins (#3797).
            let type_nid = resolve_type(receiver_type, caller_node, file)
                .or_else(|| resolve_type(receiver, caller_node, file));
            let Some(type_nid) = type_nid else {
                let name = if receiver_type.is_empty() {
                    receiver
                } else {
                    receiver_type
                };
                if absent(name) {
                    parked.push((caller.to_string(), callee.to_string(), name.to_string(), rc));
                }
                continue;
            };
            (type_nid, true)
        } else if rc.rec_get("receiver_call").is_some_and(truthy) && receiver_type.is_empty() {
            let Some(type_nid) =
                call_return_type(at(rc, "receiver_call"), caller, caller_node, file)
            else {
                continue;
            };
            (type_nid, false)
        } else {
            if receiver_type.is_empty() {
                continue;
            }
            let Some(type_nid) = resolve_type(receiver_type, caller_node, file) else {
                if absent(receiver_type) {
                    parked.push((
                        caller.to_string(),
                        callee.to_string(),
                        receiver_type.to_string(),
                        rc,
                    ));
                }
                continue;
            };
            (type_nid, false)
        };
        let Some(method) = method_on_type_or_bases(&type_nid, &ckey(callee)) else {
            continue;
        };
        if method == caller || !seen.insert(caller, &method) {
            continue;
        }
        added.push(json!({"source":caller,"target":method,"relation":"calls","context":"call","confidence":if qualified{"EXTRACTED"}else{"INFERRED"},"confidence_score":if qualified{1.0}else{0.85},"source_file":file,"source_location":rc.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    drop(node_by_id);
    drop(resolver);
    drop(seen);
    drop(contained);
    append(edges, added);
    apply_parks(nodes, parked, "csharp");
}
/// What `_chain_type` reads while typing a Java receiver chain.
struct JavaTables<'a> {
    enclosing: &'a BTreeMap<String, String>,
    jvm_bases: &'a dyn Fn(&str) -> Option<Vec<String>>,
    method_on: &'a dyn Fn(&str, &str) -> Option<String>,
    returned: &'a dyn Fn(&str) -> Option<String>,
    type_named: &'a dyn Fn(&str, &str) -> Option<String>,
}
impl JavaTables<'_> {
    /// `_chain_type`: `["type", T]`, `["this"]`, `["super"]` or
    /// `["call", m, inner]` (inner `None` is the caller's own class), each
    /// call typed from the declared return type of the method it binds to.
    fn chain_type(&self, chain: &Value, caller: &str, depth: usize) -> Option<String> {
        let items = chain.as_array().filter(|_| depth <= 16)?;
        match items.first()?.as_str()? {
            "type" => (self.type_named)(items.get(1)?.as_str()?, caller),
            "this" => self.enclosing.get(caller).cloned(),
            "super" => {
                (self.jvm_bases)(self.enclosing.get(caller).map(String::as_str).unwrap_or(""))
                    .filter(|b| b.len() == 1)
                    .map(|mut b| b.remove(0))
            }
            "call" => {
                let inner = items.get(2)?;
                let owner = if inner.is_null() {
                    self.enclosing.get(caller).cloned()
                } else {
                    self.chain_type(inner, caller, depth + 1)
                }?;
                let name = items.get(1).map(string).unwrap_or_default();
                let method = (self.method_on)(&owner, &java_key(&name))?;
                let ret = (self.returned)(&method)?;
                (self.type_named)(&ret, &method)
            }
            _ => None,
        }
    }
}
/// `_resolve_java_member_calls`: a member call bound to its receiver's
/// declared type (`this`/`super`/named types exact, typed receivers
/// inferred), the method found on the type or its JVM superclass chain; a
/// bare `m()` or `field.m()` the caller's file could not bind to a member
/// inherited from a supertype in another file (class chain before interface
/// defaults, ties bind nothing, constructors are not inherited); and a
/// receiver that is itself a call, `new T()` or a cast, typed step by step
/// from the declared return types on method nodes (`_java_ret`, #4304-#4307).
pub fn java_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    let nodes_ro: &[Value] = nodes;
    let edges_ro: &[E] = edges;
    let contained = edges_ro
        .iter()
        .filter(|e| rs(*e, "relation") == "contains")
        .map(|e| rs(e, "target"))
        .collect::<FxHashSet<_>>();
    let mut node_by_id: FxHashMap<&str, &Value> = FxHashMap::default();
    let mut type_defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes_ro {
        node_by_id.insert(s(n, "id"), n);
    }
    for n in nodes_ro {
        if !s(n, "source_file").is_empty()
            && contained.contains(s(n, "id"))
            && is_type_like_definition(n)
        {
            type_defs
                .entry(java_key(s(n, "label")))
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let mut method_index: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut enclosing: BTreeMap<String, String> = BTreeMap::new();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        let Some(method) = node_by_id.get(rs(e, "target")) else {
            continue;
        };
        enclosing
            .entry(rs(e, "target").into())
            .or_insert_with(|| rs(e, "source").into());
        method_index
            .entry((rs(e, "source").into(), java_key(s(method, "label"))))
            .or_default()
            .insert(rs(e, "target").into());
    }
    let mut seen: Seen = pairs(edges_ro, false);
    let mut inherits_bases: Index = BTreeMap::new();
    let mut supertypes: Index = BTreeMap::new();
    for e in edges_ro {
        let relation = rs(e, "relation");
        if relation == "inherits" {
            inherits_bases
                .entry(rs(e, "source").into())
                .or_default()
                .push(rs(e, "target").into());
        }
        if matches!(relation, "inherits" | "implements") {
            supertypes
                .entry(rs(e, "source").into())
                .or_default()
                .push(rs(e, "target").into());
        }
    }
    let class_fields = bind_field_tables(per, nodes_ro, "java");
    let source_family = |nid: &str| {
        node_by_id
            .get(nid)
            .map(|n| s(n, "source_file"))
            .filter(|sf| !sf.is_empty())
            .map(family)
    };
    let inherited_method = |class_nid: &str, callee_key: &str| -> Option<String> {
        // Java's "class wins": the superclass chain first, whatever the
        // depth, then the interfaces level by level. The class itself was
        // searched in-file.
        let mut seen = BTreeSet::from([class_nid.to_string()]);
        let mut chain = vec![];
        let mut cls = class_nid.to_string();
        loop {
            let parents = inherits_bases
                .get(&cls)
                .into_iter()
                .flatten()
                .filter(|b| !seen.contains(*b))
                .collect::<Vec<_>>();
            let [parent] = parents.as_slice() else {
                break;
            };
            cls = (*parent).clone();
            seen.insert(cls.clone());
            chain.push(cls.clone());
        }
        for nid in &chain {
            if source_family(nid).is_some_and(|f| f != Some("jvm")) {
                return None;
            }
            if let Some(declared) = method_index
                .get(&(nid.clone(), callee_key.to_string()))
                .filter(|d| !d.is_empty())
            {
                return (declared.len() == 1).then(|| declared.iter().next().unwrap().clone());
            }
        }
        let next_level = |level: &[String], seen: &BTreeSet<String>| {
            let mut out: Vec<String> = vec![];
            for nid in level {
                for base in supertypes.get(nid).into_iter().flatten() {
                    if !seen.contains(base) && !out.contains(base) {
                        out.push(base.clone());
                    }
                }
            }
            out
        };
        let mut start = vec![class_nid.to_string()];
        start.extend(chain.iter().cloned());
        let mut level = next_level(&start, &seen);
        while !level.is_empty() {
            let mut hits = BTreeSet::new();
            for nid in &level {
                seen.insert(nid.clone());
                if source_family(nid).is_some_and(|f| f != Some("jvm")) {
                    return None;
                }
                hits.extend(
                    method_index
                        .get(&(nid.clone(), callee_key.to_string()))
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
            if !hits.is_empty() {
                return (hits.len() == 1).then(|| hits.into_iter().next().unwrap());
            }
            level = next_level(&level, &seen);
        }
        None
    };
    // `_jvm_bases`: a type's `inherits` targets, or None when any base is the
    // type itself or is declared outside the JVM family (or nowhere), so a
    // base whose methods this corpus cannot see never yields a guess.
    let jvm_bases = |type_nid: &str| -> Option<Vec<String>> {
        let list = inherits_bases.get(type_nid).cloned().unwrap_or_default();
        for base in &list {
            let family = node_by_id
                .get(base.as_str())
                .and_then(|n| family(s(n, "source_file")));
            if base == type_nid || family != Some("jvm") {
                return None;
            }
        }
        Some(list)
    };
    // `_method_on_type_or_bases`: a nearer declaration hides the bases, and
    // anything ambiguous or unresolvable along the way resolves nothing.
    let method_on_type_or_bases = |type_nid: &str, callee_key: &str| -> Option<String> {
        let mut hits = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut frontier = vec![type_nid.to_string()];
        while let Some(n) = frontier.pop() {
            if !visited.insert(n.clone()) {
                continue;
            }
            if let Some(ms) = method_index
                .get(&(n.clone(), callee_key.to_string()))
                .filter(|m| !m.is_empty())
            {
                hits.extend(ms.iter().cloned());
                continue;
            }
            frontier.extend(jvm_bases(&n)?);
        }
        (hits.len() == 1).then(|| hits.into_iter().next().unwrap())
    };
    // A name declared more than once (a nested `Builder` in many classes) is
    // the one in the file that names it, or none.
    let type_named = |name: &str, near: &str| -> Option<String> {
        let mut defs = type_defs.get(&java_key(name)).cloned().unwrap_or_default();
        if defs.len() > 1 {
            let here = node_by_id.get(near).map(|n| s(n, "source_file"));
            defs.retain(|nid| node_by_id.get(nid.as_str()).map(|n| s(n, "source_file")) == here);
        }
        (defs.len() == 1).then(|| defs.remove(0))
    };
    let returned = |method: &str| {
        node_by_id
            .get(method)
            .and_then(|n| n.get("_java_ret"))
            .filter(|r| truthy(r))
            .map(string)
    };
    let tables = JavaTables {
        enclosing: &enclosing,
        jvm_bases: &jvm_bases,
        method_on: &method_on_type_or_bases,
        returned: &returned,
        type_named: &type_named,
    };
    let mut parked = Vec::new();
    let mut added = vec![];
    let push = |added: &mut Vec<Value>, caller: &str, target: &str, raw: &P::R, exact: bool| {
        added.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":if exact{"EXTRACTED"}else{"INFERRED"},"confidence_score":if exact{1.0}else{0.85},"source_file":rs(raw,"source_file"),"source_location":raw.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    };
    for raw in raw_calls_of(per) {
        if rs(raw, "lang") != "java" {
            continue;
        }
        let (receiver, callee, caller) = (
            rs(raw, "receiver"),
            rs(raw, "callee"),
            rs(raw, "caller_nid"),
        );
        if !raw.rec_get("is_member_call").is_some_and(truthy) {
            // A bare `m()` the caller's file could not bind: a member the
            // class inherits from a supertype in another file. `new Base()`
            // names a constructor, which is not inherited.
            let Some(class_nid) = enclosing.get(caller).filter(|_| !callee.is_empty()) else {
                continue;
            };
            let Some(method) = inherited_method(class_nid, &java_key(callee)) else {
                continue;
            };
            let owner_label = enclosing
                .get(&method)
                .and_then(|o| node_by_id.get(o.as_str()))
                .map_or("", |o| s(o, "label"));
            if method != caller
                && java_key(owner_label) != java_key(callee)
                && seen.insert(caller, &method)
            {
                push(&mut added, caller, &method, raw, true);
            }
            continue;
        }
        let chain = at(raw, "java_receiver_chain");
        if truthy(chain) && receiver.is_empty() && !callee.is_empty() && !caller.is_empty() {
            let type_nid = tables.chain_type(chain, caller, 0);
            if let Some(method) =
                type_nid.and_then(|t| method_on_type_or_bases(&t, &java_key(callee)))
                && method != caller
                && seen.insert(caller, &method)
            {
                push(&mut added, caller, &method, raw, false);
            }
            continue;
        }
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let mut exact = false;
        let type_nid = if receiver == "this" {
            exact = true;
            let Some(owner) = enclosing.get(caller) else {
                continue;
            };
            owner.clone()
        } else if receiver == "super" {
            // `super.m()` names the single direct superclass, exactly.
            let Some(base) = jvm_bases(enclosing.get(caller).map(String::as_str).unwrap_or(""))
                .filter(|b| b.len() == 1)
            else {
                continue;
            };
            exact = true;
            base[0].clone()
        } else {
            let owner = enclosing.get(caller).map(String::as_str).unwrap_or("");
            let mut ty = rs(raw, "receiver_type").to_string();
            if ty.is_empty() && raw.rec_get("receiver_unbound").is_some_and(truthy) {
                // A field the caller's file could not bind, typed through the
                // class's supertypes in other files.
                ty = inherited_field(&class_fields, &supertypes, owner, receiver)
                    .unwrap_or_default();
            }
            if ty.is_empty() && upper(receiver) {
                ty = receiver.into();
                exact = true;
            }
            if ty.is_empty()
                && let Some(field) = receiver.strip_prefix("this.")
            {
                ty = inherited_field(&class_fields, &inherits_bases, owner, field)
                    .unwrap_or_default();
            }
            if ty.is_empty() {
                continue;
            }
            let defs = type_defs.get(&java_key(&ty)).cloned().unwrap_or_default();
            if defs.is_empty() {
                // Declared nowhere in this corpus: park it for a merged graph
                // (#3152). An ambiguous name stays dropped.
                parked.push((caller.to_string(), callee.to_string(), ty, raw));
                continue;
            }
            if defs.len() != 1 {
                continue;
            }
            defs[0].clone()
        };
        let Some(method) = method_on_type_or_bases(&type_nid, &java_key(callee)) else {
            continue;
        };
        if method == caller || !seen.insert(caller, &method) {
            continue;
        }
        push(&mut added, caller, &method, raw, exact);
    }
    drop(node_by_id);
    drop(seen);
    drop(contained);
    append(edges, added);
    apply_parks(nodes, parked, "java");
}
/// `_resolve_php_member_calls` (#3872): `Class::m()`, `self::`/`static::`
/// (the enclosing class) and `parent::` (its single base) bound to the
/// class's own method, case-insensitively and by the last namespace segment.
/// A `$var::m()` scope stays unresolved; a class declared nowhere in the
/// corpus is parked for a merged graph.
pub fn php_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    let php_key = |label: &str| {
        let label = py_strip(label);
        let label = label.strip_prefix('.').unwrap_or(label);
        label.strip_suffix("()").unwrap_or(label).to_lowercase()
    };
    let class_key = |name: &str| {
        py_strip(name)
            .trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or("")
            .to_lowercase()
    };
    let nodes_ro: &[Value] = nodes;
    let edges_ro: &[E] = edges;
    let contained = edges_ro
        .iter()
        .filter(|e| rs(*e, "relation") == "contains")
        .map(|e| rs(e, "target"))
        .collect::<FxHashSet<_>>();
    let mut node_by_id: FxHashMap<&str, &Value> = FxHashMap::default();
    for n in nodes_ro {
        node_by_id.insert(s(n, "id"), n);
    }
    let mut type_defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes_ro {
        // Only PHP declarations: a same-named class in another language must
        // neither answer nor hide that nothing here declares it (0.9.89).
        if family(s(n, "source_file")) == Some("php")
            && contained.contains(s(n, "id"))
            && is_type_like_definition(n)
        {
            type_defs
                .entry(class_key(s(n, "label")))
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let mut method_index: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut enclosing: BTreeMap<String, String> = BTreeMap::new();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        let Some(method) = node_by_id.get(rs(e, "target")) else {
            continue;
        };
        enclosing
            .entry(rs(e, "target").into())
            .or_insert_with(|| rs(e, "source").into());
        method_index
            .entry((rs(e, "source").into(), php_key(s(method, "label"))))
            .or_default()
            .insert(rs(e, "target").into());
    }
    let mut inherits_bases: Index = BTreeMap::new();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "inherits") {
        inherits_bases
            .entry(rs(e, "source").into())
            .or_default()
            .push(rs(e, "target").into());
    }
    // `php_type_table`: each file's receiver annotations (0.9.89).
    let mut tables: FxHashMap<String, &serde_json::Map<String, Value>> = FxHashMap::default();
    for r in per {
        let entry = r.at("php_type_table");
        let path = s(entry, "path");
        if let Some(table) = entry["table"].as_object()
            && !path.is_empty()
        {
            tables.insert(path.to_string(), table);
        }
    }
    let mut seen: Seen = pairs(edges_ro, false);
    let mut parked = Vec::new();
    let mut added = vec![];
    for raw in raw_calls_of(per) {
        if !raw.rec_get("is_member_call").is_some_and(truthy)
            || !raw
                .rec_get("source_file")
                .map(string)
                .unwrap_or_default()
                .ends_with(".php")
        {
            continue;
        }
        let (receiver, callee, caller) = (
            rs(raw, "receiver"),
            rs(raw, "callee"),
            rs(raw, "caller_nid"),
        );
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let lowered = receiver.to_lowercase();
        let mut inferred = false;
        let type_nid = if matches!(lowered.as_str(), "self" | "static") {
            let Some(owner) = enclosing.get(caller) else {
                continue;
            };
            owner.clone()
        } else if lowered == "parent" {
            let Some(base) = enclosing
                .get(caller)
                .and_then(|owner| inherits_bases.get(owner))
                .filter(|b| b.len() == 1)
            else {
                continue;
            };
            base[0].clone()
        } else if !raw.rec_get("php_scoped").is_some_and(truthy) && !receiver.starts_with('$') {
            // An instance receiver (`$g->m()`, `$this->prop->m()`): typed by
            // the file's annotation table, never the call site, so INFERRED.
            // An untyped, union or longer-chain receiver stays unresolved.
            let Some(typed) = tables
                .get(rs(raw, "source_file"))
                .and_then(|t| t.get(receiver))
                .and_then(Value::as_str)
            else {
                continue;
            };
            inferred = true;
            let defs = type_defs
                .get(&class_key(typed))
                .cloned()
                .unwrap_or_default();
            if defs.is_empty() {
                parked.push((
                    caller.to_string(),
                    callee.to_string(),
                    typed.to_string(),
                    raw,
                ));
                continue;
            }
            if defs.len() != 1 {
                continue;
            }
            defs[0].clone()
        } else if !receiver.starts_with('$') {
            let defs = type_defs
                .get(&class_key(receiver))
                .cloned()
                .unwrap_or_default();
            if defs.is_empty() {
                parked.push((
                    caller.to_string(),
                    callee.to_string(),
                    receiver.to_string(),
                    raw,
                ));
                continue;
            }
            if defs.len() != 1 {
                continue;
            }
            defs[0].clone()
        } else {
            // A dynamic scope (`$var::m()`) has no declared type to bind.
            continue;
        };
        let Some(method) = method_index
            .get(&(type_nid, php_key(callee)))
            .filter(|m| m.len() == 1)
            .and_then(|m| m.iter().next())
        else {
            continue;
        };
        if method == caller || !seen.insert(caller, method) {
            continue;
        }
        // A table-typed receiver is the INFERRED rung (0.85); a source-named
        // class scope stays EXTRACTED.
        added.push(json!({"source":caller,"target":method,"relation":"calls","context":"call","confidence":if inferred {"INFERRED"} else {"EXTRACTED"},"confidence_score":if inferred {0.85} else {1.0},"source_file":rs(raw,"source_file"),"source_location":raw.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    drop(node_by_id);
    drop(seen);
    drop(contained);
    append(edges, added);
    apply_parks(nodes, parked, "php");
}
/// `_resolve_elixir_qualified_calls` (#4206): a remote call whose module
/// the extractor expanded (`elixir_module`) binds to that module's own def
/// when exactly one top-level module of the corpus carries the name. A
/// module outside the corpus (`Repo`, `Enum`) gets no edge.
pub fn elixir_qualified_calls<P: PerFile, E: Rec>(per: &[P], nodes: &[Value], edges: &mut Vec<E>) {
    let raw = raw_calls_of(per)
        .filter(|rc| {
            rc.rec_get("elixir_module").is_some_and(truthy)
                && rc.rec_get("callee").is_some_and(truthy)
                && rc.rec_get("caller_nid").is_some_and(truthy)
        })
        .collect::<Vec<_>>();
    if raw.is_empty() {
        return;
    }
    let text = |v: Option<&Value>| v.filter(|v| truthy(v)).map(string).unwrap_or_default();
    let mut labels: FxHashMap<&str, String> = FxHashMap::default();
    let mut modules: FxHashMap<String, Vec<&str>> = FxHashMap::default();
    for n in nodes {
        let label = text(n.get("label"));
        if n.get("_elixir_module").is_some_and(truthy) && n.get("source_file").is_some_and(truthy) {
            modules.entry(label.clone()).or_default().push(s(n, "id"));
        }
        labels.insert(s(n, "id"), label);
    }
    let edges_ro: &[E] = edges;
    let mut defs: FxHashMap<(&str, &str), &str> = FxHashMap::default();
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        let name = labels
            .get(rs(e, "target"))
            .map(String::as_str)
            .unwrap_or("");
        if let Some(name) = name.strip_suffix("()") {
            defs.entry((rs(e, "source"), name))
                .or_insert(rs(e, "target"));
        }
    }
    let mut seen: Seen<&str> = pairs(edges_ro, true);
    let mut added = vec![];
    for rc in raw {
        let Some(module) = at(rc, "elixir_module").as_str() else {
            continue;
        };
        let Some(candidates) = modules.get(module).filter(|c| c.len() == 1) else {
            continue;
        };
        let caller = rs(rc, "caller_nid");
        let Some(target) = defs.get(&(candidates[0], rs(rc, "callee"))).copied() else {
            continue;
        };
        if target == caller || !seen.insert(caller, target) {
            continue;
        }
        added.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":rs(rc,"source_file"),"source_location":rc.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    drop((seen, defs));
    drop((modules, labels));
    append(edges, added);
}
/// Python's `_key()` of the member resolvers.
fn pkey(label: &str) -> String {
    label_key(label, true)
}
/// `_resolve_python_member_calls` (#1446, #1883): `ClassName.method()`
/// against the one class of that name, `module.func()` against the one
/// module the caller's file imports under that name (or alias), an external
/// module bound by a plain import (EXTRACTED); `self.m()`/`cls.m()`/
/// `super().m()` on a single-inheritance chain across files (#4222); a
/// `self.X.m()` field bound to one class (#2860) and a typed local or
/// parameter (#4198), both behind the origin gate (INFERRED); a property
/// read (`obj.items`) only as a `references` edge to a property getter of
/// exactly that name (#4337).
pub fn python_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    let nodes: &[Value] = nodes;
    let added = {
        let edges: &[E] = edges;
        // Lookups only (the last of equal ids, as a dict collects them).
        let by_id = nodes
            .iter()
            .map(|n| (s(n, "id"), n))
            .collect::<FxHashMap<_, _>>();
        let inherits = inherits_pairs(nodes, edges, &by_id);
        python_member_edges(per, edges, &by_id, &inherits)
    };
    // Every table borrowed `edges` and is gone: grow by exactly the new
    // edges, not by doubling the whole edge list for a handful of calls.
    append(edges, added);
}
/// The (class, base) ids of the `inherits` edges, in edge order, for the
/// base walk of [`python_members`]. A changed-files rebuild still has a base
/// class from an unchanged file as a name stub, which a full build rewired
/// already: that rewire runs on copies of the `inherits` edges so both
/// builds walk the same chain, and only the copies' endpoints are kept.
/// Without a stub base nothing is copied and the ids borrow from `edges`.
fn inherits_pairs<'a, E: Rec>(
    nodes: &[Value],
    edges: &'a [E],
    by_id: &FxHashMap<&str, &Value>,
) -> Vec<(Cow<'a, str>, Cow<'a, str>)> {
    fn ends<R: Rec>(e: &R) -> Option<(&str, &str)> {
        Some((
            e.rec_get("source")?.as_str()?,
            e.rec_get("target")?.as_str()?,
        ))
    }
    let inherits = edges.iter().filter(|e| rs(*e, "relation") == "inherits");
    let stub_base = inherits.clone().any(|e| {
        by_id
            .get(rs(e, "target"))
            .is_none_or(|n| !n.get("source_file").is_some_and(truthy))
    });
    if !stub_base {
        return inherits
            .filter_map(ends)
            .map(|(src, tgt)| (Cow::Borrowed(src), Cow::Borrowed(tgt)))
            .collect();
    }
    let mut copies = inherits.map(Rec::to_value).collect::<Vec<Value>>();
    rewire_unique_stub_edges(nodes, &mut copies);
    copies
        .iter()
        .filter_map(ends)
        .map(|(src, tgt)| (Cow::Owned(src.into()), Cow::Owned(tgt.into())))
        .collect()
}
/// Interned `_key()` texts of the Python member tables: each distinct key is
/// one shared allocation however many classes, methods and children carry it.
#[derive(Default)]
struct PyKeys(FxHashSet<Rc<str>>);
impl PyKeys {
    fn key(&mut self, label: &str) -> Rc<str> {
        let key = pkey(label);
        if let Some(shared) = self.0.get(key.as_str()) {
            return shared.clone();
        }
        let shared: Rc<str> = key.into();
        self.0.insert(shared.clone());
        shared
    }
    /// The interned `_key()` of `label`; `None` when no table holds it.
    fn find(&self, label: &str) -> Option<Rc<str>> {
        self.0.get(pkey(label).as_str()).cloned()
    }
}
/// The edges of [`python_members`]. Every id in its tables borrows from the
/// records, nodes and edges it reads (the `_key()` texts are interned), so
/// the tables copy no id text; the ordered tables order `&str` exactly as
/// they ordered `String`, and the hashed ones are only looked up.
fn python_member_edges<'a, P: PerFile, E: Rec>(
    per: &'a [P],
    edges: &'a [E],
    by_id: &FxHashMap<&'a str, &'a Value>,
    inherits: &'a [(Cow<'_, str>, Cow<'_, str>)],
) -> Vec<Value> {
    let mut keys = PyKeys::default();
    let mut classes: BTreeMap<Rc<str>, BTreeSet<&'a str>> = BTreeMap::new();
    let mut methods: BTreeMap<(&'a str, Rc<str>), &'a str> = BTreeMap::new();
    // A method hangs off its class (`method` edge): the last edge wins.
    let mut method_class: FxHashMap<&'a str, &'a str> = FxHashMap::default();
    let mut children: BTreeMap<&'a str, BTreeMap<Rc<str>, Vec<&'a str>>> = BTreeMap::new();
    let mut files: BTreeMap<&'a str, &'a str> = BTreeMap::new();
    let mut imports: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
    let mut aliases: BTreeMap<(&'a str, &'a str), String> = BTreeMap::new();
    for e in edges {
        let (src, tgt) = (rs(e, "source"), rs(e, "target"));
        match rs(e, "relation") {
            "method" => {
                if !src.is_empty() && !tgt.is_empty() {
                    method_class.insert(tgt, src);
                }
                if let Some(n) = by_id.get(src) {
                    classes
                        .entry(keys.key(s(n, "label")))
                        .or_default()
                        .insert(src);
                }
                if let Some(n) = by_id.get(tgt) {
                    methods.insert((src, keys.key(s(n, "label"))), tgt);
                }
            }
            "contains" => {
                if let Some(n) = by_id.get(tgt) {
                    children
                        .entry(src)
                        .or_default()
                        .entry(keys.key(s(n, "label")))
                        .or_default()
                        .push(tgt);
                    files.insert(tgt, src);
                }
            }
            "imports" | "imports_from" => {
                imports.entry(src).or_default().insert(tgt);
                if !rs(e, "local_alias").is_empty() {
                    aliases.insert((src, tgt), pkey(rs(e, "local_alias")));
                }
            }
            _ => {}
        }
    }
    // Plain `import x` bindings of modules no parsed file defines (Graphify
    // 6868fff and 33edd89). Each binding keeps every target so a receiver two
    // imports answer to stays ambiguous.
    let mut external: BTreeMap<&'a str, BTreeMap<&'a str, Vec<&'a str>>> = BTreeMap::new();
    for e in edges {
        if e.rec_get("relation").and_then(Value::as_str) != Some("imports")
            || e.rec_get("_python_plain_import") != Some(&Value::Bool(true))
        {
            continue;
        }
        let (Some(src), Some(target)) = (
            e.rec_get("source").and_then(Value::as_str),
            e.rec_get("target").and_then(Value::as_str),
        ) else {
            continue;
        };
        if by_id.contains_key(target) || children.contains_key(target) || files.contains_key(target)
        {
            continue;
        }
        let alias = e.rec_get("local_alias").unwrap_or(&Value::Null);
        let binding = if crate::build::truthy(alias) {
            alias.as_str()
        } else {
            Some(target)
        };
        if let Some(binding) = binding {
            external
                .entry(src)
                .or_default()
                .entry(binding)
                .or_default()
                .push(target);
        }
    }
    // The class owning the method `callee` (by `_key()`), if any.
    let method_of = |class: &'a str, callee: &str| -> Option<&'a str> {
        let key = keys.find(callee)?;
        methods.get(&(class, key)).copied()
    };
    // Climb `contains` / `method` parents to the top, which is the file: a
    // nested function's `contains` parent is its enclosing function (#4227).
    let file_node = |nid: &'a str| -> Option<&'a str> {
        let mut seen = FxHashSet::default();
        let mut parent: Option<&'a str> = None;
        let mut nid = nid;
        while !nid.is_empty() && seen.insert(nid) {
            let Some(&up) = files
                .get(nid)
                .filter(|up| !up.is_empty())
                .or_else(|| method_class.get(nid).filter(|up| !up.is_empty()))
            else {
                return parent;
            };
            parent = Some(up);
            nid = up;
        }
        None
    };
    let mut bases_of: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
    for (src, tgt) in inherits {
        let (src, tgt) = (&**src, &**tgt);
        let bases = bases_of.entry(src).or_default();
        if !bases.contains(&tgt) {
            bases.push(tgt);
        }
    }
    // `self.m()` / `cls.m()` / `super().m()` whose `m` lives on a base class
    // in another file: a single-inheritance chain only, the caller's own base
    // defined in or imported by its file; the first class owning `m` wins.
    let inherited_method = |caller: &'a str, callee: &str, skip_own: bool| -> Option<&'a str> {
        let mut cls = method_class.get(caller).copied();
        let mut seen = FxHashSet::default();
        let mut first_hop = true;
        let mut skip_own = skip_own;
        while let Some(current) = cls.filter(|c| !c.is_empty() && !seen.contains(c)) {
            seen.insert(current);
            if !skip_own && let Some(hit) = method_of(current, callee).filter(|h| !h.is_empty()) {
                return Some(hit);
            }
            skip_own = false;
            if by_id
                .get(current)
                .is_some_and(|n| n["metadata"].get("python_opaque_bases").is_some_and(truthy))
            {
                return None;
            }
            let bases = bases_of.get(current).map(Vec::as_slice).unwrap_or_default();
            let [base] = bases else {
                return None;
            };
            let base = *base;
            if base == current {
                return None;
            }
            let (cls_file, base_file) = (file_node(current), file_node(base));
            let (Some(cls_file), Some(base_file)) = (cls_file, base_file) else {
                return None;
            };
            if !by_id
                .get(base)
                .is_some_and(|n| n.get("source_file").is_some_and(truthy))
            {
                return None;
            }
            if first_hop {
                let imported = imports.get(cls_file);
                if !(base_file == cls_file
                    || imported.is_some_and(|i| i.contains(base) || i.contains(base_file)))
                {
                    return None;
                }
            }
            first_hop = false;
            cls = Some(base);
        }
        None
    };
    let mut seen: Seen<&'a str> = pairs(edges, false);
    let mut added = vec![];
    // `_emit_call`: an attribute read only runs a property getter, and only
    // the one it spells (`_key()` folds `_items` into `items`), as a
    // `references` edge; INFERRED for the typed-receiver arms.
    let mut emit_call =
        |raw: &'a P::R, target: Option<&'a str>, inferred: bool, added: &mut Vec<Value>| {
            let caller = rs(raw, "caller_nid");
            let Some(target) = target.filter(|t| !t.is_empty() && *t != caller) else {
                return;
            };
            if seen.contains(caller, target) {
                return;
            }
            let attr_read = raw.rec_get("_python_attr_read").is_some_and(truthy);
            if attr_read {
                let node = by_id.get(target).copied().unwrap_or(&Value::Null);
                let label = py_strip(s(node, "label")).trim_start_matches('.');
                let label = label.strip_suffix("()").unwrap_or(label);
                if !node.get("_py_property").is_some_and(truthy) || label != rs(raw, "callee") {
                    return;
                }
            }
            seen.insert(caller, target);
            added.push(json!({
                "source": caller,
                "target": target,
                "relation": if attr_read { "references" } else { "calls" },
                "context": if attr_read { "property_read" } else { "call" },
                "confidence": if inferred { "INFERRED" } else { "EXTRACTED" },
                "confidence_score": if inferred { 0.85 } else { 1.0 },
                "source_file": rs(raw, "source_file"),
                "source_location": raw.rec_get("source_location").cloned().unwrap_or(Value::Null),
                "weight": 1.0,
            }));
        };
    for raw in raw_calls_of(per) {
        // The receiver is bound by an ambiguous absolute import (#3729).
        if raw.rec_get("_ambiguous_python_import").is_some_and(truthy)
            || raw.rec_get("is_member_call") != Some(&Value::Bool(true))
        {
            continue;
        }
        let (receiver, callee, caller) = (
            rs(raw, "receiver"),
            rs(raw, "callee"),
            rs(raw, "caller_nid"),
        );
        if matches!(receiver, "self" | "cls" | "super")
            && !callee.is_empty()
            && !caller.is_empty()
            && !raw.rec_get("_python_super_args").is_some_and(truthy)
            && let Some(target) = inherited_method(caller, callee, receiver == "super")
        {
            // Same rule as the in-file base walk, across files.
            emit_call(raw, Some(target), false, &mut added);
            continue;
        }
        let attr_type = rs(raw, "_python_self_attr_type");
        if !attr_type.is_empty() && !callee.is_empty() && !caller.is_empty() {
            // `self.X.m()` with X bound to one class (#2860): a unique class
            // the caller's own file defines or imports, owning `m`.
            let class = classes
                .get(pkey(attr_type).as_str())
                .filter(|c| c.len() == 1);
            let caller_file = method_class.get(caller).and_then(|owner| files.get(owner));
            if let (Some(class), Some(caller_file)) = (class, caller_file) {
                let class = *class.iter().next().unwrap();
                if files.get(class) == Some(caller_file)
                    || imports.get(caller_file).is_some_and(|i| i.contains(class))
                {
                    emit_call(raw, method_of(class, callee), true, &mut added);
                }
            }
            continue;
        }
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        // An external module has no parsed member definition: the extractor's
        // present-false lexical fact and one exact plain import are both
        // needed, so older cached facts and ambiguous bindings fail closed.
        if raw.rec_get("_python_receiver_shadowed") == Some(&Value::Bool(false)) {
            let targets = files
                .get(caller)
                .and_then(|file| external.get(file))
                .and_then(|bindings| bindings.get(receiver));
            match targets.map(Vec::as_slice) {
                Some(&[target]) => {
                    emit_call(raw, Some(target), false, &mut added);
                    continue;
                }
                Some([_, _, ..]) => continue,
                _ => {}
            }
        }
        let receiver_type = rs(raw, "receiver_type");
        if !receiver_type.is_empty() && !upper(receiver) {
            // A local or parameter typed by an annotation or a constructor
            // binding (#4198), behind the origin gate (#2553): the only class
            // of that name, defined in the caller's file, imported by name,
            // or inside a module the file imports. A typed local is never an
            // imported module, so the module arm is skipped.
            if let Some(class) = classes
                .get(pkey(receiver_type).as_str())
                .filter(|c| c.len() == 1)
                && let Some(caller_file) = file_node(caller)
            {
                let class = *class.iter().next().unwrap();
                let imported = imports.get(caller_file);
                let class_file = file_node(class);
                if class_file == Some(caller_file)
                    || imported.is_some_and(|i| {
                        i.contains(class) || class_file.is_some_and(|f| i.contains(f))
                    })
                {
                    emit_call(raw, method_of(class, callee), true, &mut added);
                }
            }
            continue;
        }
        let target = if upper(receiver) {
            classes
                .get(pkey(receiver).as_str())
                .filter(|c| c.len() == 1)
                .and_then(|c| c.first().and_then(|&class| method_of(class, callee)))
        } else {
            // The caller's file through its enclosing scopes: `mod.func()`
            // from a method or a nested function reads its file's imports
            // too (0.9.91, #2447).
            let Some(file) = file_node(caller) else {
                continue;
            };
            let rkey = pkey(receiver);
            let mods = imports
                .get(file)
                .into_iter()
                .flatten()
                .filter(|t| {
                    // A `contains` target is a nested def, not a module (#3887).
                    children.contains_key(*t) && !files.contains_key(*t) && {
                        let n = by_id.get(*t);
                        let stem = n
                            .map(|n| {
                                Path::new(s(n, "source_file"))
                                    .file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned()
                            })
                            .unwrap_or_default();
                        let label = if stem.is_empty() {
                            n.map(|n| s(n, "label")).unwrap_or("").to_string()
                        } else {
                            stem
                        };
                        pkey(&label) == rkey || aliases.get(&(file, **t)) == Some(&rkey)
                    }
                })
                .collect::<Vec<_>>();
            if mods.len() != 1 {
                continue;
            }
            children
                .get(mods[0])
                .and_then(|m| m.get(pkey(callee).as_str()))
                .filter(|c| c.len() == 1)
                .map(|c| c[0])
        };
        emit_call(raw, target, false, &mut added);
    }
    added
}
pub fn kotlin_imports<P: PerFile>(per: &[P], edges: &mut [Value]) {
    let mut symbols: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for r in per {
        let pkg = r
            .key("kotlin_package")
            .and_then(Value::as_str)
            .unwrap_or("");
        if pkg.is_empty() {
            continue;
        }
        for n in r.nodes().into_iter().flatten() {
            let label = rs(n, "label");
            if !rs(n, "source_file").is_empty()
                && rs(n, "type") != "namespace"
                && !label.is_empty()
                && !label.starts_with('.')
            {
                symbols
                    .entry((pkg.into(), label.trim_matches(['(', ')']).into()))
                    .or_default()
                    .push(rs(n, "id").into());
            }
        }
    }
    for e in edges {
        if s(e, "relation") == "imports"
            && [".kt", ".kts"]
                .iter()
                .any(|suf| s(e, "source_file").ends_with(suf))
            && let Some((pkg, name)) = s(&e["metadata"], "target_fqn").rsplit_once('.')
            && let Some(c) = symbols
                .get(&(pkg.into(), name.into()))
                .filter(|c| c.len() == 1)
        {
            e["target"] = json!(c[0]);
        }
    }
}
pub fn elixir_imports<E: Rec>(nodes: &[Value], edges: &mut [E]) {
    let ids = nodes.iter().map(|n| s(n, "id")).collect::<BTreeSet<_>>();
    let mut labels: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n["_elixir_module"] == true) {
        if !s(n, "label").is_empty() {
            labels.entry(make_id([s(n, "label")])).or_default().push(n);
        }
    }
    for e in edges {
        if rs(e, "relation") != "imports"
            || rs(e, "context") != "import"
            || ![".ex", ".exs"]
                .iter()
                .any(|sf| rs(e, "source_file").ends_with(sf))
            || ids.contains(rs(e, "target"))
        {
            continue;
        }
        if let Some(c) = labels.get(rs(e, "target")).filter(|c| c.len() == 1)
            && s(c[0], "source_file") != rs(e, "source_file")
        {
            e.rec_set("target", c[0]["id"].clone());
        }
    }
}
pub fn qualified_calls<P: PerFile, E: Rec>(
    lang: &str,
    per: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) {
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id").to_string(), n))
        .collect::<BTreeMap<_, _>>();
    let mut index: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut types: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut methods: Index = BTreeMap::new();
    let mut contains: Index = BTreeMap::new();
    let edges_ro: &[E] = edges;
    for e in edges_ro {
        if rs(e, "relation") == "method" {
            methods
                .entry(rs(e, "source").into())
                .or_default()
                .push(rs(e, "target").into());
        } else if rs(e, "relation") == "contains" {
            contains
                .entry(rs(e, "source").into())
                .or_default()
                .push(rs(e, "target").into());
        }
    }
    if lang == "csharp" {
        for n in nodes.iter() {
            let ns = s(&n["metadata"], "namespace");
            let label = s(n, "label").trim_matches(['(', ')']);
            if n["_callable_class"] == true
                && !s(n, "source_file").is_empty()
                && !ns.is_empty()
                && !label.is_empty()
            {
                index
                    .entry((ns.into(), label.into()))
                    .or_default()
                    .push(s(n, "id").into());
            }
        }
    } else {
        for r in per {
            let pkg = r.at("kotlin_package").as_str().unwrap_or("");
            if pkg.is_empty() {
                continue;
            }
            let Some(file) = r.nodes().into_iter().flatten().find(|n| {
                !rs(*n, "source_file").is_empty()
                    && Path::new(rs(*n, "source_file"))
                        .file_name()
                        .and_then(|s| s.to_str())
                        == Some(rs(*n, "label"))
            }) else {
                continue;
            };
            for tgt in contains.get(rs(file, "id")).into_iter().flatten() {
                if let Some(n) = by_id.get(tgt) {
                    let name = s(n, "label").trim_matches(['(', ')']);
                    if s(n, "source_file").is_empty() || name.is_empty() || name.starts_with('.') {
                        continue;
                    }
                    if n["_callable"] == true {
                        index
                            .entry((pkg.into(), name.into()))
                            .or_default()
                            .push(tgt.clone());
                    }
                    if n["_callable_class"] == true {
                        types
                            .entry((pkg.into(), name.into()))
                            .or_default()
                            .push(tgt.clone());
                    }
                }
            }
        }
    }
    let mut seen = pairs(edges_ro, lang == "csharp");
    let mut added = vec![];
    for raw in raw_calls_of(per) {
        let prefix = rs(raw, "qualified_prefix");
        let callee = rs(raw, "callee");
        if rs(raw, "lang") != lang
            || prefix.is_empty()
            || callee.is_empty()
            || rs(raw, "caller_nid").is_empty()
        {
            continue;
        }
        let mut candidates = index
            .get(&(prefix.into(), callee.into()))
            .cloned()
            .unwrap_or_default();
        if lang == "kotlin"
            && !index.keys().any(|k| k.0 == prefix)
            && let Some((pkg, name)) = prefix.rsplit_once('.')
            && let Some(ts) = types
                .get(&(pkg.into(), name.into()))
                .filter(|t| t.len() == 1)
        {
            candidates = methods
                .get(&ts[0])
                .into_iter()
                .flatten()
                .filter(|id| {
                    by_id.get(*id).is_some_and(|n| {
                        s(n, "label").trim_matches(['(', ')']) == format!(".{callee}")
                    })
                })
                .cloned()
                .collect();
        }
        if candidates.len() == 1 {
            emit(&mut added, &mut seen, raw, &candidates[0], "calls", true);
        }
    }
    drop((seen, by_id, index, types, methods, contains));
    append(edges, added);
}
pub fn kotlin_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    let nodes_ro: &[Value] = nodes;
    let by_id = nodes_ro
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<BTreeMap<_, _>>();
    let mut types: Index = BTreeMap::new();
    let mut methods: Index = BTreeMap::new();
    for n in nodes_ro {
        if n["_callable_class"] == true
            && [".kt", ".kts"]
                .iter()
                .any(|su| s(n, "source_file").ends_with(su))
        {
            types
                .entry(s(n, "label").into())
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let edges_ro: &[E] = edges;
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        methods
            .entry(rs(e, "source").into())
            .or_default()
            .push(rs(e, "target").into());
    }
    let mut parked = Vec::new();
    let mut seen = pairs(edges_ro, false);
    let mut added = vec![];
    for raw in raw_calls_of(per) {
        if rs(raw, "lang") != "kotlin"
            || rs(raw, "kotlin_object_receiver").is_empty()
            || rs(raw, "callee").is_empty()
            || rs(raw, "caller_nid").is_empty()
        {
            continue;
        }
        let receiver = rs(raw, "kotlin_object_receiver");
        let found = types.get(receiver).filter(|t| !t.is_empty());
        let Some(ts) = found.filter(|t| t.len() == 1) else {
            // Declared nowhere here: usually in a repo this build does not
            // contain, so park it for the merge (#3152, #3388); a builtin
            // receiver never crosses a repo boundary.
            if found.is_none() && !is_language_builtin(receiver) {
                parked.push((
                    rs(raw, "caller_nid").to_string(),
                    rs(raw, "callee").to_string(),
                    receiver.to_string(),
                    raw,
                ));
            }
            continue;
        };
        let wanted = format!(".{}", rs(raw, "callee"));
        let cs = methods
            .get(&ts[0])
            .into_iter()
            .flatten()
            .filter(|id| {
                by_id
                    .get(id.as_str())
                    .is_some_and(|n| s(n, "label").trim_matches(['(', ')']) == wanted)
            })
            .collect::<Vec<_>>();
        if cs.len() == 1 {
            emit(&mut added, &mut seen, raw, cs[0], "calls", true);
        }
    }
    drop((seen, by_id, types, methods));
    append(edges, added);
    apply_parks(nodes, parked, "kotlin");
}
/// `_resolve_go_member_calls`: `g.Greet()` bound through the receiver's
/// declared type (the method's own receiver, the function's scope, then the
/// file's flat table), to the single Go declaration of that type and its
/// method of exactly that name (Go exports by case). Always INFERRED. A type
/// declared nowhere in the corpus is parked for a merged graph (#3152).
pub fn go_members<P: PerFile, E: Rec>(per: &[P], nodes: &mut [Value], edges: &mut Vec<E>) {
    let raw = raw_calls_of(per)
        .filter(|rc| {
            rs(*rc, "language") == "go"
                && truthy(at(*rc, "is_member_call"))
                && !rs(*rc, "member_receiver").is_empty()
                && !rs(*rc, "callee").is_empty()
                && !rs(*rc, "caller_nid").is_empty()
        })
        .collect::<Vec<_>>();
    if raw.is_empty() {
        return;
    }
    let mut tables: FxHashMap<&str, &serde_json::Map<String, Value>> = FxHashMap::default();
    for r in per {
        let entry = r.at("go_type_table");
        let path = s(entry, "path");
        if let Some(table) = entry["table"].as_object()
            && !path.is_empty()
        {
            tables.insert(path, table);
        }
    }
    let mut defs: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut methods: FxHashMap<(String, String), Vec<String>> = FxHashMap::default();
    {
        let mut by_id: FxHashMap<&str, &Value> = FxHashMap::default();
        for n in nodes.iter() {
            by_id.insert(s(n, "id"), n);
            // Only Go declarations: a same-named class of another language
            // must neither answer a Go receiver nor hide that nothing here
            // declares it.
            if truthy(&n["_callable_class"]) && family(s(n, "source_file")) == Some("go") {
                defs.entry(java_key(s(n, "label")))
                    .or_default()
                    .push(s(n, "id").to_string());
            }
        }
        for e in edges.iter().filter(|e| rs(*e, "relation") == "method") {
            if let Some(target) = by_id.get(rs(e, "target")) {
                methods
                    .entry((rs(e, "source").to_string(), java_key(s(target, "label"))))
                    .or_default()
                    .push(rs(e, "target").to_string());
            }
        }
    }
    let mut parked = Vec::new();
    let mut seen = pairs(&edges[..], false);
    let mut added = vec![];
    for rc in raw {
        let (receiver, callee, caller) = (
            rs(rc, "member_receiver"),
            rs(rc, "callee"),
            rs(rc, "caller_nid"),
        );
        // The method's own receiver and the function's scope come first:
        // two types in one file may both name their receiver `s`.
        let mut ty = rs(rc, "receiver_type");
        if ty.is_empty() {
            ty = tables
                .get(rs(rc, "source_file"))
                .and_then(|t| t.get(receiver))
                .and_then(Value::as_str)
                .unwrap_or("");
        }
        if ty.is_empty() || is_language_builtin(ty) {
            continue;
        }
        let Some(found) = defs.get(&java_key(ty)).filter(|d| !d.is_empty()) else {
            parked.push((caller.to_string(), callee.to_string(), ty.to_string(), rc));
            continue;
        };
        if found.len() != 1 {
            continue;
        }
        let Some(targets) = methods.get(&(found[0].clone(), java_key(callee))) else {
            continue;
        };
        if targets.len() == 1 {
            emit(&mut added, &mut seen, rc, &targets[0], "calls", false);
        }
    }
    drop(seen);
    append(edges, added);
    apply_parks(nodes, parked, "go");
}
pub fn rust_self_calls<P: PerFile, E: Rec>(per: &[P], nodes: &[Value], edges: &mut Vec<E>) {
    let mut labels: Index = BTreeMap::new();
    let mut impls: Index = BTreeMap::new();
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    let mut generic: BTreeMap<String, i64> = BTreeMap::new();
    let mut methods: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let edges_ro: &[E] = edges;
    let contained = edges_ro
        .iter()
        .filter(|e| rs(*e, "relation") == "contains")
        .map(|e| rs(e, "target"))
        .collect::<BTreeSet<_>>();
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<BTreeMap<_, _>>();
    for n in nodes
        .iter()
        .filter(|n| s(n, "source_file").ends_with(".rs"))
    {
        let (label, id) = (s(n, "label"), s(n, "id"));
        labels.entry(label.into()).or_default().push(id.into());
        if !s(n, "_rust_impl_key").is_empty() {
            impls
                .entry(s(n, "_rust_impl_key").into())
                .or_default()
                .push(id.into());
        }
        *declared.entry(label.into()).or_default() += usize::from(contained.contains(id));
        *generic.entry(label.into()).or_default() +=
            n["_rust_declaration_count"].as_i64().unwrap_or(0).max(0);
    }
    for e in edges_ro.iter().filter(|e| rs(*e, "relation") == "method") {
        if let Some(n) = by_id.get(rs(e, "target")) {
            methods
                .entry((
                    rs(e, "source").into(),
                    s(n, "label")
                        .trim_matches(['(', ')'])
                        .trim_start_matches('.')
                        .into(),
                ))
                .or_default()
                .insert(rs(e, "target").into());
        }
    }
    let mut seen = pairs(edges_ro, true);
    let mut added = vec![];
    for raw in raw_calls_of(per) {
        let ty = rs(raw, "rust_self_type");
        if ty.is_empty() || rs(raw, "callee").is_empty() || rs(raw, "caller_nid").is_empty() {
            continue;
        }
        let ik = rs(raw, "rust_self_impl_key");
        let owners = if !ik.is_empty() {
            if generic.get(ty).copied().unwrap_or(0) != 1 {
                continue;
            }
            impls.get(ik)
        } else {
            if declared.get(ty).copied().unwrap_or(0) >= 2 {
                continue;
            }
            labels.get(ty)
        };
        let candidates = owners
            .into_iter()
            .flatten()
            .flat_map(|o| {
                methods
                    .get(&(o.clone(), rs(raw, "callee").into()))
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect::<BTreeSet<_>>();
        if candidates.len() == 1 {
            emit(
                &mut added,
                &mut seen,
                raw,
                candidates.iter().next().unwrap(),
                "calls",
                true,
            );
        }
    }
    drop((seen, labels, impls, declared, generic, methods));
    drop((contained, by_id));
    append(edges, added);
}

#[cfg(test)]
thread_local! {
    /// Tests: the member passes that ran on compact records, in order.
    pub(crate) static NATIVE_RUNS: std::cell::RefCell<Vec<&'static str>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
mod native_tests;
