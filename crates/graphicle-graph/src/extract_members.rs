// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Cross-file receiver binding for the application extraction pipeline.
use crate::{
    build::truthy,
    extract_merge::{family, label_key, rewire_unique_stub_edges},
    extractors::{
        base::{is_language_builtin, make_id},
        csharp::{CsharpNameResolver, is_cs_file},
        resolution::{is_type_like_definition, s},
    },
    python_compat::string,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
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
pub fn park(node: Option<&mut Value>, callee: &str, receiver: &str, lang: &str, raw: &Value) {
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
    if !s(raw, "source_location").is_empty() {
        v["line"] = raw["source_location"].clone();
    }
    calls.push(v);
}
/// Apply the parks a resolver collected, in its loop order: nothing a
/// resolver reads changes when a call is parked on its caller's metadata.
fn apply_parks(nodes: &mut [Value], parked: Vec<(String, String, String, &Value)>, lang: &str) {
    for (caller, callee, ty, raw) in parked {
        park(
            nodes.iter_mut().find(|n| s(n, "id") == caller),
            &callee,
            &ty,
            lang,
            raw,
        );
    }
}
/// Emitted (caller, target) pairs; membership only, so hashed.
type Seen = rustc_hash::FxHashSet<(String, String)>;
fn emit(
    edges: &mut Vec<Value>,
    seen: &mut Seen,
    raw: &Value,
    target: &str,
    relation: &str,
    exact: bool,
) {
    let caller = s(raw, "caller_nid");
    if target.is_empty() || target == caller || !seen.insert((caller.into(), target.into())) {
        return;
    }
    edges.push(json!({"source":caller,"target":target,"relation":relation,"context":"call","confidence":if exact{"EXTRACTED"}else{"INFERRED"},"confidence_score":if exact{1.0}else{0.85},"source_file":s(raw,"source_file"),"source_location":raw.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
}
fn raw_calls(per: &[Value]) -> impl Iterator<Item = &Value> {
    per.iter()
        .flat_map(|r| r["raw_calls"].as_array().into_iter().flatten())
}
fn pairs(edges: &[Value], calls_only: bool) -> Seen {
    edges
        .iter()
        .filter(|e| !calls_only || s(e, "relation") == "calls")
        .map(|e| (s(e, "source").into(), s(e, "target").into()))
        .collect()
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
pub fn bind_field_table_values(
    per: &[Value],
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
        .flat_map(|r| r["member_field_tables"].as_array().into_iter().flatten())
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
pub fn bind_field_tables(per: &[Value], nodes: &[Value], lang: &str) -> Fields {
    let mut labels: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for n in nodes {
        if !s(n, "source_file").is_empty() && !s(n, "label").is_empty() {
            labels.entry(s(n, "label")).or_default().push(n);
        }
    }
    let mut out = Fields::new();
    for entry in per
        .iter()
        .flat_map(|r| r["member_field_tables"].as_array().into_iter().flatten())
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
pub fn resolve_members(lang: &str, per: &[Value], nodes: &mut [Value], edges: &mut Vec<Value>) {
    match lang {
        "python" => return python_members(per, nodes, edges),
        "csharp" => return csharp_members(per, nodes, edges),
        "java" => return java_members(per, nodes, edges),
        "php" => return php_members(per, nodes, edges),
        _ => {}
    }
    let table_key = match lang {
        "swift" => "swift_type_table",
        "typescript" => "ts_type_table",
        "cpp" => "cpp_type_table",
        "objc" => "objc_type_table",
        _ => "",
    };
    let mut tables: Fields = BTreeMap::new();
    let mut factories = BTreeMap::new();
    for r in per {
        if let Some(t) = r[table_key]["table"].as_object() {
            let path = s(&r[table_key], "path");
            if !path.is_empty() {
                tables.insert(
                    path.into(),
                    t.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.into())))
                        .collect(),
                );
                if r[table_key]["factory"].is_object() {
                    factories.insert(path.to_string(), r[table_key]["factory"].clone());
                }
            }
        }
    }
    if matches!(lang, "swift" | "typescript") && tables.is_empty() {
        return;
    }
    // The nodes as they were on entry, by id (the last of equal ids): only
    // parking below changes a node, and parks are applied after the loop
    // that reads this, so it borrows instead of copying every node.
    let nodes_ro: &[Value] = nodes;
    let snapshot = nodes_ro
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<rustc_hash::FxHashMap<_, _>>();
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
        .collect::<BTreeSet<_>>();
    let mut defs: Index = BTreeMap::new();
    for n in nodes.iter() {
        if !s(n, "source_file").is_empty()
            && contained.contains(s(n, "id"))
            && is_type_like_definition(n)
            && !(lang == "objc" && s(n, "label").starts_with('<') && s(n, "label").ends_with('>'))
        {
            defs.entry(key(s(n, "label"), lang))
                .or_default()
                .push(s(n, "id").into());
        }
    }
    let mut methods: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut enclosing = BTreeMap::new();
    let mut bases: Index = BTreeMap::new();
    let mut file_of = BTreeMap::new();
    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for rel in if lang == "cpp" {
        vec!["defines", "method"]
    } else {
        vec!["method"]
    } {
        for e in edges.iter().filter(|e| s(e, "relation") == rel) {
            if let Some(n) = snapshot.get(s(e, "target")) {
                enclosing
                    .entry(s(e, "target").to_string())
                    .or_insert(s(e, "source").to_string());
                methods.insert(
                    (s(e, "source").to_string(), key(s(n, "label"), lang)),
                    s(e, "target").to_string(),
                );
            }
        }
    }
    for e in edges.iter() {
        match s(e, "relation") {
            "inherits" => {
                bases
                    .entry(s(e, "source").into())
                    .or_default()
                    .push(s(e, "target").into());
            }
            "contains" => {
                file_of.insert(s(e, "target").to_string(), s(e, "source").to_string());
            }
            "imports" | "imports_from" => {
                imports
                    .entry(s(e, "source").into())
                    .or_default()
                    .insert(s(e, "target").into());
            }
            _ => {}
        }
    }
    for (method, owner) in &enclosing {
        if let Some(file) = file_of.get(owner).cloned() {
            file_of.entry(method.clone()).or_insert(file);
        }
    }
    let mut fields = Fields::new();
    if lang == "objc" {
        let mut conflicts = BTreeSet::new();
        for r in per {
            if let Some(ts) = r["objc_field_types"]["tables"].as_object() {
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
    if lang == "swift" {
        let mut returns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for e in edges.iter() {
            if s(e, "relation") == "references"
                && s(e, "context") == "return_type"
                && e["metadata"]["swift_plain_return"] == true
            {
                returns
                    .entry(s(e, "source").into())
                    .or_default()
                    .insert(s(e, "target").into());
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
                let Some(ms) = methods.get(&(ds[0].clone(), key(method, lang))) else {
                    continue;
                };
                let Some(rs) = returns.get(ms).filter(|d| d.len() == 1) else {
                    continue;
                };
                let Some(n) = snapshot.get(rs.iter().next().unwrap().as_str()) else {
                    continue;
                };
                let label = s(n, "label");
                if label.is_empty()
                    || is_language_builtin(label)
                    || defs.get(&key(label, lang)).is_none_or(|d| d.len() != 1)
                {
                    continue;
                }
                tables
                    .entry(path.clone())
                    .or_default()
                    .entry(receiver.clone())
                    .or_insert(label.into());
            }
        }
    }
    // (caller, callee, type, raw call) to park, in loop order.
    let mut parked = Vec::new();
    let type_lookup = |name: &str| -> Option<String> {
        if name.is_empty() {
            return None;
        }
        defs.get(&key(name, lang))
            .filter(|d| d.len() == 1)
            .map(|d| d[0].clone())
    };
    let mut seen = pairs(edges, false);
    for raw in raw_calls(per) {
        if raw["is_member_call"] != true {
            continue;
        }
        if matches!(lang, "cpp" | "objc") && s(raw, "lang") != lang {
            continue;
        }
        let (receiver, callee, caller, file) = (
            s(raw, "receiver"),
            s(raw, "callee"),
            s(raw, "caller_nid"),
            s(raw, "source_file"),
        );
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let mut exact = upper(receiver);
        let mut ty = String::new();
        let mut direct = None;
        if lang == "cpp" && receiver == "this" {
            direct = enclosing.get(caller).cloned();
            exact = true;
            if direct.is_none() {
                continue;
            }
        } else if lang == "objc" && s(raw, "receiver_kind") == "self_field" {
            ty = inherited_field(
                &fields,
                &bases,
                enclosing.get(caller).map(String::as_str).unwrap_or(""),
                receiver,
            )
            .unwrap_or_default();
            exact = false;
        } else if lang == "objc" && matches!(receiver, "self" | "super") {
            direct = enclosing.get(caller).cloned();
            exact = true;
            if direct.is_none() {
                continue;
            }
        } else if upper(receiver) {
            ty = receiver.into();
        } else {
            ty = tables
                .get(file)
                .and_then(|t| t.get(receiver))
                .cloned()
                .unwrap_or_default();
            if ty.is_empty() && lang == "objc" {
                ty = inherited_field(
                    &fields,
                    &bases,
                    enclosing.get(caller).map(String::as_str).unwrap_or(""),
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
                // Origin gate (#2553), which also picks among same-named
                // definitions (#4233): the one the caller's file defines,
                // imports by name, or imports the module of. Exactly one
                // visible definition binds; none or several emit nothing.
                let cf = file_of.get(caller);
                let imported = cf.and_then(|f| imports.get(f));
                let visible = defs
                    .get(&key(&ty, lang))
                    .into_iter()
                    .flatten()
                    .filter(|nid| {
                        let tf = file_of.get(*nid);
                        (cf.is_some() && cf == tf)
                            || imported.is_some_and(|i| {
                                i.contains(*nid) || tf.is_some_and(|f| i.contains(f))
                            })
                    })
                    .collect::<Vec<_>>();
                if visible.len() != 1 {
                    continue;
                }
                direct = Some(visible[0].clone());
            } else {
                direct = type_lookup(&ty);
            }
        }
        let Some(owner) = direct else {
            let absent = defs.get(&key(&ty, lang)).is_none_or(Vec::is_empty);
            if absent
                && (lang == "cpp" || (lang == "swift" && file.to_lowercase().ends_with(".swift")))
            {
                parked.push((caller.to_string(), callee.to_string(), ty, raw));
            }
            continue;
        };
        let mut target = methods.get(&(owner.clone(), key(callee, lang))).cloned();
        if lang == "typescript"
            && let Some(method) = target.as_deref().and_then(|t| snapshot.get(t))
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
            let candidates = nodes_ro
                .iter()
                .filter(|n| !s(n, "source_file").is_empty() && s(n, "label") == label)
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                target = Some(s(candidates[0], "id").into());
            }
        }
        if let Some(target) = target {
            emit(edges, &mut seen, raw, &target, "calls", exact);
        } else if matches!(lang, "swift" | "cpp" | "objc") {
            emit(edges, &mut seen, raw, &owner, "references", exact);
        }
    }
    // Nothing above reads what parking writes (a node's metadata list of
    // unresolved calls), so the parks apply now, in the same order.
    drop(snapshot);
    apply_parks(nodes, parked, lang);
}
/// `_resolve_csharp_member_calls` (#1609, #1620): a member call bound to its
/// receiver's declared type with the namespace/using/alias scoping of the C#
/// type-reference pass, the method looked up through the `inherits` chain
/// (an out-of-corpus base poisons the lookup). Receivers: `this`/`base`
/// (constructor bodies count as the type, #4246), an explicitly named type
/// whose same-named field or property shadows it (#3797), a field chain
/// `x.F.M()` (#4246), a `var` typed from the called method's declared return
/// type (#4266), or the extractor's method-scoped type table.
pub fn csharp_members(per: &[Value], nodes: &mut [Value], edges: &mut Vec<Value>) {
    let ckey = |label: &str| label_key(label, true);
    let nodes_ro: &[Value] = nodes;
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
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
    let resolver = CsharpNameResolver::new(nodes_ro, edges);
    let mut method_index: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut enclosing: BTreeMap<String, String> = BTreeMap::new();
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        let Some(method) = node_by_id.get(s(e, "target")) else {
            continue;
        };
        enclosing
            .entry(s(e, "target").into())
            .or_insert_with(|| s(e, "source").into());
        method_index.insert(
            (s(e, "source").into(), ckey(s(method, "label"))),
            s(e, "target").into(),
        );
    }
    let mut bases_of: Index = BTreeMap::new();
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    for e in edges.iter().filter(|e| s(e, "relation") == "inherits") {
        if !is_cs_file(&e["source_file"]) || !e["source"].is_string() || !e["target"].is_string() {
            continue;
        }
        let (src, tgt) = (s(e, "source"), s(e, "target"));
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
    let mut seen = pairs(edges, false);
    for rc in raw_calls(per) {
        if s(rc, "lang") != "csharp" || !rc.get("is_member_call").is_some_and(truthy) {
            continue;
        }
        let (receiver, callee, caller) = (s(rc, "receiver"), s(rc, "callee"), s(rc, "caller_nid"));
        let chain = rc["receiver_chain"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if (receiver.is_empty() && chain.is_empty()) || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let file = s(rc, "source_file");
        let caller_node = node_by_id.get(caller).copied();
        let receiver_type = s(rc, "receiver_type");
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
        } else if rc.get("receiver_call").is_some_and(truthy) && receiver_type.is_empty() {
            let Some(type_nid) = call_return_type(&rc["receiver_call"], caller, caller_node, file)
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
        if method == caller || !seen.insert((caller.to_string(), method.clone())) {
            continue;
        }
        added.push(json!({"source":caller,"target":method,"relation":"calls","context":"call","confidence":if qualified{"EXTRACTED"}else{"INFERRED"},"confidence_score":if qualified{1.0}else{0.85},"source_file":file,"source_location":rc.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    drop(node_by_id);
    drop(resolver);
    edges.extend(added);
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
pub fn java_members(per: &[Value], nodes: &mut [Value], edges: &mut Vec<Value>) {
    let nodes_ro: &[Value] = nodes;
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
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
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        let Some(method) = node_by_id.get(s(e, "target")) else {
            continue;
        };
        enclosing
            .entry(s(e, "target").into())
            .or_insert_with(|| s(e, "source").into());
        method_index
            .entry((s(e, "source").into(), java_key(s(method, "label"))))
            .or_default()
            .insert(s(e, "target").into());
    }
    let mut seen = pairs(edges, false);
    let mut inherits_bases: Index = BTreeMap::new();
    let mut supertypes: Index = BTreeMap::new();
    for e in edges.iter() {
        let relation = s(e, "relation");
        if relation == "inherits" {
            inherits_bases
                .entry(s(e, "source").into())
                .or_default()
                .push(s(e, "target").into());
        }
        if matches!(relation, "inherits" | "implements") {
            supertypes
                .entry(s(e, "source").into())
                .or_default()
                .push(s(e, "target").into());
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
    let push = |added: &mut Vec<Value>, caller: &str, target: &str, raw: &Value, exact: bool| {
        added.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":if exact{"EXTRACTED"}else{"INFERRED"},"confidence_score":if exact{1.0}else{0.85},"source_file":s(raw,"source_file"),"source_location":raw.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    };
    for raw in raw_calls(per) {
        if s(raw, "lang") != "java" {
            continue;
        }
        let (receiver, callee, caller) =
            (s(raw, "receiver"), s(raw, "callee"), s(raw, "caller_nid"));
        if !raw.get("is_member_call").is_some_and(truthy) {
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
                && seen.insert((caller.to_string(), method.clone()))
            {
                push(&mut added, caller, &method, raw, true);
            }
            continue;
        }
        let chain = &raw["java_receiver_chain"];
        if truthy(chain) && receiver.is_empty() && !callee.is_empty() && !caller.is_empty() {
            let type_nid = tables.chain_type(chain, caller, 0);
            if let Some(method) =
                type_nid.and_then(|t| method_on_type_or_bases(&t, &java_key(callee)))
                && method != caller
                && seen.insert((caller.to_string(), method.clone()))
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
            let mut ty = s(raw, "receiver_type").to_string();
            if ty.is_empty() && raw.get("receiver_unbound").is_some_and(truthy) {
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
        if method == caller || !seen.insert((caller.to_string(), method.clone())) {
            continue;
        }
        push(&mut added, caller, &method, raw, exact);
    }
    drop(node_by_id);
    edges.extend(added);
    apply_parks(nodes, parked, "java");
}
/// `_resolve_php_member_calls` (#3872): `Class::m()`, `self::`/`static::`
/// (the enclosing class) and `parent::` (its single base) bound to the
/// class's own method, case-insensitively and by the last namespace segment.
/// A `$var::m()` scope stays unresolved; a class declared nowhere in the
/// corpus is parked for a merged graph.
pub fn php_members(per: &[Value], nodes: &mut [Value], edges: &mut Vec<Value>) {
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
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
        .collect::<FxHashSet<_>>();
    let mut node_by_id: FxHashMap<&str, &Value> = FxHashMap::default();
    for n in nodes_ro {
        node_by_id.insert(s(n, "id"), n);
    }
    let mut type_defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes_ro {
        if !s(n, "source_file").is_empty()
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
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        let Some(method) = node_by_id.get(s(e, "target")) else {
            continue;
        };
        enclosing
            .entry(s(e, "target").into())
            .or_insert_with(|| s(e, "source").into());
        method_index
            .entry((s(e, "source").into(), php_key(s(method, "label"))))
            .or_default()
            .insert(s(e, "target").into());
    }
    let mut inherits_bases: Index = BTreeMap::new();
    for e in edges.iter().filter(|e| s(e, "relation") == "inherits") {
        inherits_bases
            .entry(s(e, "source").into())
            .or_default()
            .push(s(e, "target").into());
    }
    let mut seen = pairs(edges, false);
    let mut parked = Vec::new();
    let mut added = vec![];
    for raw in raw_calls(per) {
        if !raw.get("is_member_call").is_some_and(truthy)
            || !raw
                .get("source_file")
                .map(string)
                .unwrap_or_default()
                .ends_with(".php")
        {
            continue;
        }
        let (receiver, callee, caller) =
            (s(raw, "receiver"), s(raw, "callee"), s(raw, "caller_nid"));
        if receiver.is_empty() || callee.is_empty() || caller.is_empty() {
            continue;
        }
        let lowered = receiver.to_lowercase();
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
        if method == caller || !seen.insert((caller.to_string(), method.clone())) {
            continue;
        }
        added.push(json!({"source":caller,"target":method,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":s(raw,"source_file"),"source_location":raw.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    drop(node_by_id);
    edges.extend(added);
    apply_parks(nodes, parked, "php");
}
/// `_resolve_elixir_qualified_calls` (#4206): a remote call whose module
/// the extractor expanded (`elixir_module`) binds to that module's own def
/// when exactly one top-level module of the corpus carries the name. A
/// module outside the corpus (`Repo`, `Enum`) gets no edge.
pub fn elixir_qualified_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let raw = raw_calls(per)
        .filter(|rc| {
            rc.get("elixir_module").is_some_and(truthy)
                && rc.get("callee").is_some_and(truthy)
                && rc.get("caller_nid").is_some_and(truthy)
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
    let mut defs: FxHashMap<(&str, &str), &str> = FxHashMap::default();
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        let name = labels.get(s(e, "target")).map(String::as_str).unwrap_or("");
        if let Some(name) = name.strip_suffix("()") {
            defs.entry((s(e, "source"), name)).or_insert(s(e, "target"));
        }
    }
    let mut seen = pairs(edges, true);
    let mut added = vec![];
    for rc in raw {
        let Some(module) = rc["elixir_module"].as_str() else {
            continue;
        };
        let Some(candidates) = modules.get(module).filter(|c| c.len() == 1) else {
            continue;
        };
        let caller = s(rc, "caller_nid");
        let Some(target) = defs.get(&(candidates[0], s(rc, "callee"))).copied() else {
            continue;
        };
        if target == caller || !seen.insert((caller.to_string(), target.to_string())) {
            continue;
        }
        added.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":s(rc,"source_file"),"source_location":rc.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0}));
    }
    edges.extend(added);
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
pub fn python_members(per: &[Value], nodes: &mut [Value], edges: &mut Vec<Value>) {
    // Lookups only (the last of equal ids, as a dict collects them).
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<FxHashMap<_, _>>();
    let mut classes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut methods = BTreeMap::new();
    // A method hangs off its class (`method` edge): the last edge wins.
    let mut method_class: FxHashMap<String, String> = FxHashMap::default();
    let mut children: BTreeMap<String, Index> = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut aliases: BTreeMap<(String, String), String> = BTreeMap::new();
    for e in edges.iter() {
        let (src, tgt) = (s(e, "source"), s(e, "target"));
        match s(e, "relation") {
            "method" => {
                if !src.is_empty() && !tgt.is_empty() {
                    method_class.insert(tgt.to_string(), src.to_string());
                }
                if let Some(n) = by_id.get(src) {
                    classes
                        .entry(pkey(s(n, "label")))
                        .or_default()
                        .insert(src.into());
                }
                if let Some(n) = by_id.get(tgt) {
                    methods.insert((src.to_string(), pkey(s(n, "label"))), tgt.to_string());
                }
            }
            "contains" => {
                if let Some(n) = by_id.get(tgt) {
                    children
                        .entry(src.into())
                        .or_default()
                        .entry(pkey(s(n, "label")))
                        .or_default()
                        .push(tgt.into());
                    files.insert(tgt.to_string(), src.to_string());
                }
            }
            "imports" | "imports_from" => {
                imports.entry(src.into()).or_default().insert(tgt.into());
                if !s(e, "local_alias").is_empty() {
                    aliases.insert((src.into(), tgt.into()), pkey(s(e, "local_alias")));
                }
            }
            _ => {}
        }
    }
    // Plain `import x` bindings of modules no parsed file defines (Graphify
    // 6868fff and 33edd89). Each binding keeps every target so a receiver two
    // imports answer to stays ambiguous.
    let mut external: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for e in edges.iter() {
        if e["relation"] != "imports" || e["_python_plain_import"] != true {
            continue;
        }
        let (Some(src), Some(target)) = (e["source"].as_str(), e["target"].as_str()) else {
            continue;
        };
        if by_id.contains_key(target) || children.contains_key(target) || files.contains_key(target)
        {
            continue;
        }
        let alias = &e["local_alias"];
        let binding = if crate::build::truthy(alias) {
            alias.as_str()
        } else {
            Some(target)
        };
        if let Some(binding) = binding {
            external
                .entry(src.into())
                .or_default()
                .entry(binding.into())
                .or_default()
                .push(target.into());
        }
    }
    // Climb `contains` / `method` parents to the top, which is the file: a
    // nested function's `contains` parent is its enclosing function (#4227).
    let file_node = |nid: &str| -> Option<String> {
        let mut seen = FxHashSet::default();
        let mut parent: Option<String> = None;
        let mut nid = nid.to_string();
        while !nid.is_empty() && seen.insert(nid.clone()) {
            let Some(up) = files
                .get(&nid)
                .filter(|up| !up.is_empty())
                .or_else(|| method_class.get(&nid).filter(|up| !up.is_empty()))
            else {
                return parent;
            };
            parent = Some(up.clone());
            nid = up.clone();
        }
        None
    };
    // A changed-files rebuild still has a base class from an unchanged file
    // as a name stub, which a full build rewired already: run that rewire on
    // copies of the `inherits` edges so both builds walk the same chain.
    let mut inherits = edges
        .iter()
        .filter(|e| s(e, "relation") == "inherits")
        .cloned()
        .collect::<Vec<_>>();
    if inherits.iter().any(|e| {
        by_id
            .get(s(e, "target"))
            .is_none_or(|n| !n.get("source_file").is_some_and(truthy))
    }) {
        rewire_unique_stub_edges(nodes, &mut inherits);
    }
    let mut bases_of: Index = BTreeMap::new();
    for e in &inherits {
        let (Some(src), Some(tgt)) = (e["source"].as_str(), e["target"].as_str()) else {
            continue;
        };
        let bases = bases_of.entry(src.into()).or_default();
        if !bases.iter().any(|b| b == tgt) {
            bases.push(tgt.into());
        }
    }
    drop(inherits);
    // `self.m()` / `cls.m()` / `super().m()` whose `m` lives on a base class
    // in another file: a single-inheritance chain only, the caller's own base
    // defined in or imported by its file; the first class owning `m` wins.
    let inherited_method = |caller: &str, callee: &str, skip_own: bool| -> Option<String> {
        let mut cls = method_class.get(caller).cloned();
        let mut seen = FxHashSet::default();
        let mut first_hop = true;
        let mut skip_own = skip_own;
        while let Some(current) = cls.filter(|c| !c.is_empty() && !seen.contains(c)) {
            seen.insert(current.clone());
            if !skip_own
                && let Some(hit) = methods
                    .get(&(current.clone(), pkey(callee)))
                    .filter(|h: &&String| !h.is_empty())
            {
                return Some(hit.clone());
            }
            skip_own = false;
            if by_id
                .get(current.as_str())
                .is_some_and(|n| n["metadata"].get("python_opaque_bases").is_some_and(truthy))
            {
                return None;
            }
            let bases = bases_of
                .get(&current)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let [base] = bases else {
                return None;
            };
            if *base == current {
                return None;
            }
            let (cls_file, base_file) = (file_node(&current), file_node(base));
            let (Some(cls_file), Some(base_file)) = (cls_file, base_file) else {
                return None;
            };
            if !by_id
                .get(base.as_str())
                .is_some_and(|n| n.get("source_file").is_some_and(truthy))
            {
                return None;
            }
            if first_hop {
                let imported = imports.get(&cls_file);
                if !(base_file == cls_file
                    || imported.is_some_and(|i| i.contains(base) || i.contains(&base_file)))
                {
                    return None;
                }
            }
            first_hop = false;
            cls = Some(base.clone());
        }
        None
    };
    let mut seen = pairs(edges, false);
    let mut added = vec![];
    // `_emit_call`: an attribute read only runs a property getter, and only
    // the one it spells (`_key()` folds `_items` into `items`), as a
    // `references` edge; INFERRED for the typed-receiver arms.
    let mut emit_call =
        |raw: &Value, target: Option<&str>, inferred: bool, added: &mut Vec<Value>| {
            let caller = s(raw, "caller_nid");
            let Some(target) = target.filter(|t| !t.is_empty() && *t != caller) else {
                return;
            };
            if seen.contains(&(caller.to_string(), target.to_string())) {
                return;
            }
            let attr_read = raw.get("_python_attr_read").is_some_and(truthy);
            if attr_read {
                let node = by_id.get(target).copied().unwrap_or(&Value::Null);
                let label = py_strip(s(node, "label")).trim_start_matches('.');
                let label = label.strip_suffix("()").unwrap_or(label);
                if !node.get("_py_property").is_some_and(truthy) || label != s(raw, "callee") {
                    return;
                }
            }
            seen.insert((caller.to_string(), target.to_string()));
            added.push(json!({
                "source": caller,
                "target": target,
                "relation": if attr_read { "references" } else { "calls" },
                "context": if attr_read { "property_read" } else { "call" },
                "confidence": if inferred { "INFERRED" } else { "EXTRACTED" },
                "confidence_score": if inferred { 0.85 } else { 1.0 },
                "source_file": s(raw, "source_file"),
                "source_location": raw.get("source_location").cloned().unwrap_or(Value::Null),
                "weight": 1.0,
            }));
        };
    for raw in raw_calls(per) {
        // The receiver is bound by an ambiguous absolute import (#3729).
        if crate::build::truthy(&raw["_ambiguous_python_import"]) || raw["is_member_call"] != true {
            continue;
        }
        let (receiver, callee, caller) =
            (s(raw, "receiver"), s(raw, "callee"), s(raw, "caller_nid"));
        if matches!(receiver, "self" | "cls" | "super")
            && !callee.is_empty()
            && !caller.is_empty()
            && !raw.get("_python_super_args").is_some_and(truthy)
            && let Some(target) = inherited_method(caller, callee, receiver == "super")
        {
            // Same rule as the in-file base walk, across files.
            emit_call(raw, Some(&target), false, &mut added);
            continue;
        }
        let attr_type = s(raw, "_python_self_attr_type");
        if !attr_type.is_empty() && !callee.is_empty() && !caller.is_empty() {
            // `self.X.m()` with X bound to one class (#2860): a unique class
            // the caller's own file defines or imports, owning `m`.
            let class = classes.get(&pkey(attr_type)).filter(|c| c.len() == 1);
            let caller_file = method_class.get(caller).and_then(|owner| files.get(owner));
            if let (Some(class), Some(caller_file)) = (class, caller_file) {
                let class = class.iter().next().unwrap();
                if files.get(class) == Some(caller_file)
                    || imports.get(caller_file).is_some_and(|i| i.contains(class))
                {
                    let target = methods.get(&(class.clone(), pkey(callee))).cloned();
                    emit_call(raw, target.as_deref(), true, &mut added);
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
        if raw.get("_python_receiver_shadowed") == Some(&Value::Bool(false)) {
            let targets = files
                .get(caller)
                .and_then(|file| external.get(file))
                .and_then(|bindings| bindings.get(receiver));
            match targets.map(Vec::as_slice) {
                Some([target]) => {
                    emit_call(raw, Some(target), false, &mut added);
                    continue;
                }
                Some([_, _, ..]) => continue,
                _ => {}
            }
        }
        let receiver_type = s(raw, "receiver_type");
        if !receiver_type.is_empty() && !upper(receiver) {
            // A local or parameter typed by an annotation or a constructor
            // binding (#4198), behind the origin gate (#2553): the only class
            // of that name, defined in the caller's file, imported by name,
            // or inside a module the file imports. A typed local is never an
            // imported module, so the module arm is skipped.
            if let Some(class) = classes.get(&pkey(receiver_type)).filter(|c| c.len() == 1)
                && let Some(caller_file) = file_node(caller)
            {
                let class = class.iter().next().unwrap();
                let imported = imports.get(&caller_file);
                let class_file = file_node(class);
                if class_file.as_ref() == Some(&caller_file)
                    || imported.is_some_and(|i| {
                        i.contains(class) || class_file.as_ref().is_some_and(|f| i.contains(f))
                    })
                {
                    let target = methods.get(&(class.clone(), pkey(callee))).cloned();
                    emit_call(raw, target.as_deref(), true, &mut added);
                }
            }
            continue;
        }
        let target = if upper(receiver) {
            classes
                .get(&pkey(receiver))
                .filter(|c| c.len() == 1)
                .and_then(|c| methods.get(&(c.iter().next().unwrap().clone(), pkey(callee))))
                .cloned()
        } else {
            let Some(file) = files.get(caller) else {
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
                        let n = by_id.get(t.as_str());
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
                        pkey(&label) == rkey
                            || aliases.get(&(file.clone(), (*t).clone())) == Some(&rkey)
                    }
                })
                .collect::<Vec<_>>();
            if mods.len() != 1 {
                continue;
            }
            children
                .get(mods[0])
                .and_then(|m| m.get(&pkey(callee)))
                .filter(|c| c.len() == 1)
                .map(|c| c[0].clone())
        };
        emit_call(raw, target.as_deref(), false, &mut added);
    }
    drop(by_id);
    edges.extend(added);
}
pub fn kotlin_imports(per: &[Value], edges: &mut [Value]) {
    let mut symbols: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for r in per {
        let pkg = s(r, "kotlin_package");
        if pkg.is_empty() {
            continue;
        }
        for n in r["nodes"].as_array().into_iter().flatten() {
            let label = s(n, "label");
            if !s(n, "source_file").is_empty()
                && s(n, "type") != "namespace"
                && !label.is_empty()
                && !label.starts_with('.')
            {
                symbols
                    .entry((pkg.into(), label.trim_matches(['(', ')']).into()))
                    .or_default()
                    .push(s(n, "id").into());
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
pub fn elixir_imports(nodes: &[Value], edges: &mut [Value]) {
    let ids = nodes.iter().map(|n| s(n, "id")).collect::<BTreeSet<_>>();
    let mut labels: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n["_elixir_module"] == true) {
        if !s(n, "label").is_empty() {
            labels.entry(make_id([s(n, "label")])).or_default().push(n);
        }
    }
    for e in edges {
        if s(e, "relation") != "imports"
            || s(e, "context") != "import"
            || ![".ex", ".exs"]
                .iter()
                .any(|sf| s(e, "source_file").ends_with(sf))
            || ids.contains(s(e, "target"))
        {
            continue;
        }
        if let Some(c) = labels.get(s(e, "target")).filter(|c| c.len() == 1)
            && s(c[0], "source_file") != s(e, "source_file")
        {
            e["target"] = c[0]["id"].clone();
        }
    }
}
pub fn qualified_calls(lang: &str, per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id").to_string(), n))
        .collect::<BTreeMap<_, _>>();
    let mut index: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut types: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut methods: Index = BTreeMap::new();
    let mut contains: Index = BTreeMap::new();
    for e in edges.iter() {
        if s(e, "relation") == "method" {
            methods
                .entry(s(e, "source").into())
                .or_default()
                .push(s(e, "target").into());
        } else if s(e, "relation") == "contains" {
            contains
                .entry(s(e, "source").into())
                .or_default()
                .push(s(e, "target").into());
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
            let pkg = s(r, "kotlin_package");
            if pkg.is_empty() {
                continue;
            }
            let Some(file) = r["nodes"].as_array().into_iter().flatten().find(|n| {
                !s(n, "source_file").is_empty()
                    && Path::new(s(n, "source_file"))
                        .file_name()
                        .and_then(|s| s.to_str())
                        == Some(s(n, "label"))
            }) else {
                continue;
            };
            for tgt in contains.get(s(file, "id")).into_iter().flatten() {
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
    let mut seen = pairs(edges, lang == "csharp");
    for raw in raw_calls(per) {
        let prefix = s(raw, "qualified_prefix");
        let callee = s(raw, "callee");
        if s(raw, "lang") != lang
            || prefix.is_empty()
            || callee.is_empty()
            || s(raw, "caller_nid").is_empty()
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
            emit(edges, &mut seen, raw, &candidates[0], "calls", true);
        }
    }
}
pub fn kotlin_members(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let by_id = nodes
        .iter()
        .map(|n| (s(n, "id"), n))
        .collect::<BTreeMap<_, _>>();
    let mut types: Index = BTreeMap::new();
    let mut methods: Index = BTreeMap::new();
    for n in nodes {
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
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        methods
            .entry(s(e, "source").into())
            .or_default()
            .push(s(e, "target").into());
    }
    let mut seen = pairs(edges, false);
    for raw in raw_calls(per) {
        if s(raw, "lang") != "kotlin"
            || s(raw, "kotlin_object_receiver").is_empty()
            || s(raw, "callee").is_empty()
            || s(raw, "caller_nid").is_empty()
        {
            continue;
        }
        let Some(ts) = types
            .get(s(raw, "kotlin_object_receiver"))
            .filter(|t| t.len() == 1)
        else {
            continue;
        };
        let wanted = format!(".{}", s(raw, "callee"));
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
            emit(edges, &mut seen, raw, cs[0], "calls", true);
        }
    }
}
pub fn rust_self_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let mut labels: Index = BTreeMap::new();
    let mut impls: Index = BTreeMap::new();
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    let mut generic: BTreeMap<String, i64> = BTreeMap::new();
    let mut methods: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let contained = edges
        .iter()
        .filter(|e| s(e, "relation") == "contains")
        .map(|e| s(e, "target"))
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
    for e in edges.iter().filter(|e| s(e, "relation") == "method") {
        if let Some(n) = by_id.get(s(e, "target")) {
            methods
                .entry((
                    s(e, "source").into(),
                    s(n, "label")
                        .trim_matches(['(', ')'])
                        .trim_start_matches('.')
                        .into(),
                ))
                .or_default()
                .insert(s(e, "target").into());
        }
    }
    let mut seen = pairs(edges, true);
    for raw in raw_calls(per) {
        let ty = s(raw, "rust_self_type");
        if ty.is_empty() || s(raw, "callee").is_empty() || s(raw, "caller_nid").is_empty() {
            continue;
        }
        let ik = s(raw, "rust_self_impl_key");
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
                    .get(&(o.clone(), s(raw, "callee").into()))
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect::<BTreeSet<_>>();
        if candidates.len() == 1 {
            emit(
                edges,
                &mut seen,
                raw,
                candidates.iter().next().unwrap(),
                "calls",
                true,
            );
        }
    }
}
