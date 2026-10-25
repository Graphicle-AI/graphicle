// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/ruby_resolution.py
//! Native translation of graphify/ruby_resolution.py.
//!
//! The pass reads the edges and the per-file raw calls in whichever form
//! assembly holds them ([`Rec`], [`PerFile`]), so neither list converts for
//! it; the JSON forms give what they always gave.
use crate::{
    Result,
    build_labels::is_file_node_label,
    extract::per_file::PerFile,
    python_compat::{ScalarKey, scalar_key, string},
    resolver_registry::{LanguageResolver, Rec, append_edges, at, edge_record},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, LazyLock},
};
static BARE_CONST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[A-Z][A-Za-z0-9_]*(?:::[A-Z][A-Za-z0-9_]*)*(?:\n)?\z").unwrap()
});
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn text(node: &Value, field: &str) -> String {
    node.get(field).map(string).unwrap_or_default()
}
/// [`text`] of an edge or raw-call record.
fn rtext<R: Rec>(record: &R, field: &str) -> String {
    record.rec_get(field).map(string).unwrap_or_default()
}
fn id(value: &Value) -> Result<ScalarKey> {
    scalar_key(value).ok_or_else(|| "unhashable identifier".into())
}
fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn ruby_source(source: &str) -> bool {
    source.ends_with(".rb") || source.ends_with(".rake")
}
pub fn key(label: &Value) -> String {
    string(label)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}
pub fn method_name(label: &Value) -> String {
    let s = if truthy(label) {
        string(label)
    } else {
        String::new()
    };
    s.trim_matches(['(', ')']).trim_start_matches('.').into()
}
pub fn ruby_method_kind(node: &Value) -> Option<String> {
    node.get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| m.get("ruby_method_kind"))
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "instance" | "singleton"))
        .map(str::to_string)
}
pub fn ruby_lookup_unsafe(node: &Value) -> bool {
    node.get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.get("ruby_lookup_unsafe").is_some_and(truthy)
                || m.get("ruby_reopened").is_some_and(truthy)
        })
}
pub fn ruby_raw_calls(per_file: &[Value]) -> Result<Vec<&Value>> {
    ruby_raw_calls_any(per_file)
}
/// The raw calls of Ruby sources, in result order, from the per-file results
/// in either form. A result that is no object, or has no `raw_calls`, a
/// string or an object there, adds none; any other non-array is an error
/// (Python iterates it). A call that is no object has no `source_file` and
/// is skipped.
pub fn ruby_raw_calls_any<P: PerFile>(per_file: &[P]) -> Result<Vec<&P::R>> {
    let mut calls = vec![];
    for result in per_file {
        let Some(raw) = result.raw_calls() else {
            match result.raw_calls_other() {
                Some(value) if !value.is_string() && !value.is_object() => {
                    return Err("raw_calls is not iterable".into());
                }
                _ => continue,
            }
        };
        calls.extend(
            raw.iter()
                .filter(|v| ruby_source(&rtext(*v, "source_file"))),
        );
    }
    Ok(calls)
}
fn segments(label: &str) -> Vec<String> {
    label
        .split("::")
        .map(strip)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
fn path_parts(label: &str) -> Vec<&str> {
    label.split("::").filter(|s| !s.is_empty()).collect()
}
type StringSet = BTreeSet<String>;
/// The tables of the pass. Nodes are borrowed (the last of equal ids, as a
/// dict collects them), not copied.
struct Lookup<'a> {
    nodes: BTreeMap<String, &'a Value>,
    context_complete: bool,
    unsafe_files: StringSet,
    external_owners: StringSet,
    classes: BTreeMap<String, Vec<String>>,
    class_ids: StringSet,
    class_labels: BTreeMap<String, StringSet>,
    method_index: BTreeMap<(String, String), String>,
    method_owners: BTreeMap<String, StringSet>,
    inheritance: BTreeMap<String, StringSet>,
    methods_by_kind: BTreeMap<(String, String, String), StringSet>,
    ambiguous: BTreeSet<(String, String)>,
    fq: BTreeMap<Vec<String>, Vec<String>>,
    last: BTreeMap<String, Vec<String>>,
}
impl<'a> Lookup<'a> {
    fn new<E: Rec>(nodes: &'a [Value], edges: &[E]) -> Self {
        let node_map: BTreeMap<_, _> = nodes
            .iter()
            .filter(|n| truthy(&n["id"]))
            .map(|n| (string(&n["id"]), n))
            .collect();
        let file_nodes: Vec<_> = nodes
            .iter()
            .filter(|n| {
                ruby_source(&text(n, "source_file"))
                    && is_file_node_label(&n["label"], &n["source_file"])
            })
            .collect();
        let context_complete = !file_nodes.is_empty()
            && file_nodes.iter().all(|n| {
                n["metadata"].as_object().is_some_and(|m| {
                    m.get("ruby_resolution_schema").and_then(scalar_key)
                        == Some(ScalarKey::Integer(1))
                })
            });
        let unsafe_files = file_nodes
            .iter()
            .filter(|n| ruby_lookup_unsafe(n))
            .map(|n| text(n, "source_file"))
            .collect();
        let external_owners = file_nodes
            .iter()
            .flat_map(|n| {
                n["metadata"]
                    .get("ruby_external_method_owners")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let mut out = Self {
            nodes: node_map,
            context_complete,
            unsafe_files,
            external_owners,
            classes: BTreeMap::new(),
            class_ids: StringSet::new(),
            class_labels: BTreeMap::new(),
            method_index: BTreeMap::new(),
            method_owners: BTreeMap::new(),
            inheritance: BTreeMap::new(),
            methods_by_kind: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            fq: BTreeMap::new(),
            last: BTreeMap::new(),
        };
        let mut inheritance_edges = vec![];
        for edge in edges {
            if at(edge, "relation") == "inherits" {
                if truthy(at(edge, "source")) && truthy(at(edge, "target")) {
                    let metadata = at(edge, "metadata");
                    let raw = metadata
                        .get("ruby_superclass_ref")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let scopes = metadata
                        .get("ruby_lexical_scopes")
                        .and_then(Value::as_array)
                        .and_then(|a| {
                            a.iter()
                                .map(|v| v.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        });
                    inheritance_edges.push((
                        rtext(edge, "source"),
                        rtext(edge, "target"),
                        raw,
                        scopes,
                    ));
                }
                continue;
            }
            if at(edge, "relation") != "method"
                || !truthy(at(edge, "source"))
                || !truthy(at(edge, "target"))
            {
                continue;
            }
            let source = rtext(edge, "source");
            let target = rtext(edge, "target");
            if let Some(class) = out.nodes.get(&source).copied() {
                if !ruby_source(&text(class, "source_file"))
                    || !ruby_source(
                        &out.nodes
                            .get(&target)
                            .map(|n| text(n, "source_file"))
                            .unwrap_or_default(),
                    )
                {
                    continue;
                }
                let label = text(class, "label");
                out.add_class(&source, &label);
            }
            if let Some(method) = out.nodes.get(&target) {
                out.method_index.insert(
                    (source.clone(), method_name(&method["label"])),
                    target.clone(),
                );
                out.method_owners.entry(target).or_default().insert(source);
            }
        }
        for node in nodes {
            let label = text(node, "label");
            if truthy(&node["id"])
                && ruby_source(&text(node, "source_file"))
                && BARE_CONST.is_match(&label)
            {
                out.add_class(&text(node, "id"), &label);
            }
        }
        for ids in out.classes.values_mut() {
            ids.sort();
            ids.dedup();
        }
        out.class_ids = out.classes.values().flatten().cloned().collect();
        for nid in &out.class_ids {
            let label = out
                .nodes
                .get(nid)
                .map(|n| text(n, "label"))
                .unwrap_or_default();
            out.class_labels
                .entry(label.clone())
                .or_default()
                .insert(nid.clone());
            let parts = segments(&label);
            if let Some(last) = parts.last() {
                out.last.entry(last.clone()).or_default().push(nid.clone());
                out.fq.entry(parts).or_default().push(nid.clone());
            }
        }
        for (source, target, raw, scopes) in inheritance_edges {
            if !out.class_ids.contains(&source) {
                continue;
            }
            let resolved = out.resolve_base(raw.as_deref(), scopes.as_deref());
            let target_node = out.nodes.get(&target);
            let label = target_node.map(|n| text(n, "label")).unwrap_or_default();
            let file = target_node
                .map(|n| text(n, "source_file"))
                .unwrap_or_default();
            let raw_tail = raw
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("::")
                .split("::")
                .last()
                .unwrap_or("");
            let agrees = target_node.is_some()
                && !raw_tail.is_empty()
                && key(&json!(label.split("::").last().unwrap_or(""))) == key(&json!(raw_tail));
            if let Some(resolved) = resolved.filter(|r| r == &target || (file.is_empty() && agrees))
            {
                out.inheritance.entry(source).or_default().insert(resolved);
            }
        }
        for (method, owners) in &out.method_owners {
            let name = method_name(&out.nodes[method]["label"]);
            let kind = ruby_method_kind(out.nodes[method]);
            if owners.len() != 1 || kind.is_none() {
                for owner in owners {
                    out.ambiguous.insert((owner.clone(), name.clone()));
                }
                continue;
            }
            out.methods_by_kind
                .entry((owners.first().unwrap().clone(), name, kind.unwrap()))
                .or_default()
                .insert(method.clone());
        }
        out
    }
    fn add_class(&mut self, id: &str, label: &str) {
        self.classes
            .entry(key(&json!(label)))
            .or_default()
            .push(id.into());
        if label.contains("::") {
            self.classes
                .entry(key(&json!(label.split("::").last().unwrap())))
                .or_default()
                .push(id.into());
        }
    }
    fn resolve_base(&self, raw: Option<&str>, lexical: Option<&[String]>) -> Option<String> {
        let raw = raw.filter(|s| !s.is_empty())?;
        let lexical = lexical?;
        let parts = path_parts(raw.strip_prefix("::").unwrap_or(raw));
        if parts.is_empty() {
            return None;
        }
        let scopes: Vec<&str> = if raw.starts_with("::") || lexical.is_empty() {
            vec![""]
        } else {
            lexical.iter().map(String::as_str).collect()
        };
        for scope in scopes {
            let mut candidate = path_parts(scope);
            candidate.extend_from_slice(&parts);
            let label = candidate.join("::");
            if let Some(ids) = self.class_labels.get(&label) {
                if ids.len() == 1 {
                    return ids.first().cloned();
                }
                if ids.len() > 1 {
                    return None;
                }
            }
            if !scope.is_empty() && parts.len() > 1 {
                let mut first = path_parts(scope);
                first.push(parts[0]);
                if self.class_labels.contains_key(&first.join("::")) {
                    return None;
                }
            }
        }
        None
    }
    fn has_external_owner(&self, label: &str) -> bool {
        let label = path_parts(label);
        for raw in &self.external_owners {
            let prefix = raw.ends_with("::*");
            let owner = raw.strip_suffix("::*").unwrap_or(raw);
            let parts = path_parts(owner.strip_prefix("::").unwrap_or(owner));
            if parts.is_empty() {
                continue;
            }
            if prefix {
                if owner.starts_with("::") {
                    if label.starts_with(&parts) {
                        return true;
                    }
                } else if label.windows(parts.len()).any(|window| window == parts) {
                    return true;
                }
            } else if (owner.starts_with("::") && label == parts)
                || (!owner.starts_with("::") && label.ends_with(&parts))
            {
                return true;
            }
        }
        false
    }
    fn inherited_method(&self, owner: &str, name: &str, kind: &str) -> Option<String> {
        let mut current = owner.to_string();
        let mut seen = StringSet::new();
        let mut first = true;
        while seen.insert(current.clone()) {
            let node = self.nodes.get(&current).copied().unwrap_or(&Value::Null);
            let label = text(node, "label");
            if !self.class_ids.contains(&current)
                || self.class_labels.get(&label).map_or(0, BTreeSet::len) != 1
                || ruby_lookup_unsafe(node)
                || self.has_external_owner(&label)
                || self.ambiguous.contains(&(current.clone(), name.into()))
            {
                return None;
            }
            if let Some(ids) = self
                .methods_by_kind
                .get(&(current.clone(), name.into(), kind.into()))
                .filter(|v| !v.is_empty())
            {
                return (!first && ids.len() == 1).then(|| ids.first().unwrap().clone());
            }
            let bases = self.inheritance.get(&current).filter(|v| v.len() == 1)?;
            current = bases.first().unwrap().clone();
            first = false;
        }
        None
    }
    fn unique_class(&self, name: &str) -> Option<String> {
        self.classes
            .get(&key(&json!(name)))
            .filter(|v| v.len() == 1)
            .map(|v| v[0].clone())
    }
    fn class_by_const_path(&self, raw: &str) -> Option<String> {
        let parts = segments(raw);
        if parts.is_empty() {
            return None;
        }
        if strip(raw).starts_with("::") {
            return self
                .fq
                .get(&parts)
                .filter(|v| v.len() == 1)
                .map(|v| v[0].clone());
        }
        let hits: StringSet = self
            .fq
            .iter()
            .filter(|(path, _)| path.ends_with(&parts))
            .flat_map(|(_, ids)| ids)
            .cloned()
            .collect();
        (hits.len() == 1).then(|| hits.first().unwrap().clone())
    }
}
/// Add the edge `caller` -> `target` for `call` unless the pair has one.
/// New edges collect in `added` (appended after the pass): no pass step reads
/// them back, as only `promote` scans the list and it matches `INFERRED`
/// `calls` edges, which this pass never adds.
fn emit<E: Rec, C: Rec>(
    added: &mut Vec<E>,
    pairs: &mut BTreeSet<(ScalarKey, ScalarKey)>,
    caller: &str,
    target: &str,
    call: &C,
    relation: &str,
    context: &str,
) {
    if caller.is_empty()
        || target.is_empty()
        || caller == target
        || !pairs.insert((
            ScalarKey::Text(caller.into()),
            ScalarKey::Text(target.into()),
        ))
    {
        return;
    }
    added.push(edge_record(json!({"source":caller,"target":target,"relation":relation,"context":context,"confidence":"EXTRACTED","confidence_score":1.0,"source_file":call.rec_get("source_file").cloned().unwrap_or(json!("")),"source_location":at(call, "source_location"),"weight":1.0})));
}
fn promote<E: Rec, C: Rec>(
    edges: &mut [E],
    lookup: &Lookup,
    caller: &str,
    target: &str,
    callee: &str,
    call: &C,
) {
    let empty = json!("");
    let call_file = call.rec_get("source_file").unwrap_or(&empty);
    let call_location = at(call, "source_location");
    let matches: Vec<_> = edges
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            at(*e, "source") == caller
                && at(*e, "target") == target
                && at(*e, "relation") == "calls"
                && at(*e, "context") == "call"
                && at(*e, "confidence") == "INFERRED"
                && at(*e, "source_file") == call_file
                && at(*e, "source_location") == call_location
                && method_name(&lookup.nodes.get(target).copied().unwrap_or(&Value::Null)["label"])
                    == callee
        })
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 {
        edges[matches[0]].rec_set("confidence", json!("EXTRACTED"));
        edges[matches[0]].rec_set("confidence_score", json!(1.0));
    }
}
pub fn resolve_ruby_member_calls(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    resolve_ruby_member_calls_any(per_file, nodes, edges)
}
/// [`resolve_ruby_member_calls`] over the per-file results and the edges in
/// either form.
pub fn resolve_ruby_member_calls_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) -> Result<()> {
    let lookup = Lookup::new(nodes, edges);
    let calls = ruby_raw_calls_any(per_file)?;
    let mut pairs: BTreeSet<_> = edges
        .iter()
        .map(|e| Ok((id(at(e, "source"))?, id(at(e, "target"))?)))
        .collect::<Result<_>>()?;
    let mut added: Vec<E> = vec![];
    for &call in &calls {
        if !truthy(at(call, "is_mixin")) {
            continue;
        }
        let caller = rtext(call, "caller_nid");
        if caller.is_empty() || !truthy(at(call, "callee")) {
            continue;
        }
        let raw = string(at(call, "callee"));
        let absolute = raw.starts_with("::");
        let parts = segments(&raw);
        let caller_parts = if absolute {
            vec![]
        } else {
            segments(
                &lookup
                    .nodes
                    .get(&caller)
                    .map(|n| text(n, "label"))
                    .unwrap_or_default(),
            )
        };
        let mut target = None;
        for i in (0..=caller_parts.len()).rev() {
            let mut path = caller_parts[..i].to_vec();
            path.extend_from_slice(&parts);
            if let Some(ids) = lookup.fq.get(&path) {
                if ids.len() == 1 {
                    target = Some(ids[0].clone());
                    break;
                }
                if ids.len() > 1 {
                    break;
                }
            }
        }
        if target.is_none() && parts.len() == 1 && !absolute {
            target = lookup
                .last
                .get(&parts[0])
                .filter(|v| v.len() == 1)
                .map(|v| v[0].clone());
        }
        if let Some(target) = target {
            emit(
                &mut added, &mut pairs, &caller, &target, call, "mixes_in", "mixin",
            );
        }
    }
    if lookup.context_complete {
        for &call in &calls {
            if truthy(at(call, "is_mixin")) || *at(call, "is_member_call") != Value::Bool(false) {
                continue;
            }
            let caller = rtext(call, "caller_nid");
            let callee = rtext(call, "callee");
            let node = lookup.nodes.get(&caller).copied().unwrap_or(&Value::Null);
            let kind = ruby_method_kind(node);
            let owners = lookup.method_owners.get(&caller);
            if caller.is_empty()
                || callee.is_empty()
                || kind.is_none()
                || owners.map_or(0, BTreeSet::len) != 1
                || lookup.unsafe_files.contains(&text(node, "source_file"))
            {
                continue;
            }
            if let Some(target) = lookup
                .inherited_method(owners.unwrap().first().unwrap(), &callee, &kind.unwrap())
                .filter(|v| v != &caller)
            {
                promote(edges, &lookup, &caller, &target, &callee, call);
            }
        }
    }
    for call in calls {
        if !truthy(at(call, "is_member_call")) {
            continue;
        }
        let caller = rtext(call, "caller_nid");
        if caller.is_empty() || !truthy(at(call, "callee")) {
            continue;
        }
        let callee = string(at(call, "callee"));
        let receiver = at(call, "receiver");
        if truthy(receiver)
            && string(receiver)
                .trim_start_matches(':')
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
        {
            let raw = string(receiver);
            let class = if raw.contains("::") {
                lookup.class_by_const_path(&raw)
            } else {
                lookup.unique_class(&raw)
            };
            if let Some(class) = class {
                let target = if callee == "new" {
                    class.clone()
                } else {
                    lookup
                        .method_index
                        .get(&(class.clone(), callee.clone()))
                        .cloned()
                        .unwrap_or(class)
                };
                emit(
                    &mut added, &mut pairs, &caller, &target, call, "calls", "call",
                );
            }
            continue;
        }
        if !truthy(at(call, "receiver_type")) {
            continue;
        }
        let Some(class) = lookup.unique_class(&string(at(call, "receiver_type"))) else {
            continue;
        };
        let mut method = lookup
            .method_index
            .get(&(class.clone(), callee.clone()))
            .cloned();
        if method.is_none()
            && lookup.context_complete
            && lookup.unsafe_files.is_empty()
            && lookup.external_owners.is_empty()
        {
            method = lookup.inherited_method(&class, &callee, "instance");
        }
        if let Some(target) = method {
            emit(
                &mut added, &mut pairs, &caller, &target, call, "calls", "call",
            );
        }
    }
    drop((lookup, pairs));
    append_edges(edges, added);
    Ok(())
}
pub fn resolver() -> LanguageResolver {
    LanguageResolver {
        name: "ruby_member_calls".into(),
        suffixes: [".rb".into(), ".rake".into()].into(),
        resolve: Arc::new(|p, n, e| resolve_ruby_member_calls(p, n, e)),
    }
}
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "ruby.key" => Ok(json!(key(&request["value"]))),
        "ruby.method_name" => Ok(json!(method_name(&request["value"]))),
        "ruby.method_kind" => Ok(json!(ruby_method_kind(&request["node"]))),
        "ruby.unsafe" => Ok(json!(ruby_lookup_unsafe(&request["node"]))),
        "ruby.file_label" => Ok(json!(is_file_node_label(
            &request["label"],
            &request["source_file"]
        ))),
        "ruby.raw_calls" => Ok(json!(ruby_raw_calls(
            request["per_file"]
                .as_array()
                .ok_or("Expected per-file results")?
        )?)),
        "ruby.resolve" => {
            let p = request["per_file"]
                .as_array()
                .ok_or("Expected per-file results")?;
            let n = request["nodes"].as_array().ok_or("Expected nodes")?;
            let mut e = request["edges"].as_array().ok_or("Expected edges")?.clone();
            resolve_ruby_member_calls(p, n, &mut e)?;
            Ok(json!({"nodes":n,"edges":e}))
        }
        _ => Err("Unknown Ruby resolution operation".into()),
    }
}
