// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/csharp.py
//! Native translation of graphify/extractors/csharp.py.
//! This file resolves C# graph facts; the source parser remains in the extractor core.
use crate::{
    Result, html_compat, ids::make_id, python_compat::string, rcfile::effective_suffix,
    resolver_registry::Rec,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
pub const DOTNET_SOURCE_EXTS: &[&str] = &[".cs", ".razor", ".cshtml"];
pub type TypeIndex = BTreeMap<(String, String), String>;
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
fn strip(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn text_or_empty(value: &Value) -> String {
    if truthy(value) {
        string(value)
    } else {
        String::new()
    }
}
pub fn metadata(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
/// `_is_cs_file`: a C# source by its effective suffix (a `.graphiclerc`
/// language declaration counts, #2961).
pub fn is_cs_file(value: &Value) -> bool {
    value.as_str().is_some_and(|s| effective_suffix(s) == ".cs")
}
pub fn is_dotnet_source_file(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| DOTNET_SOURCE_EXTS.contains(&effective_suffix(s).as_str()))
}
fn sort_key(node: &Value) -> (String, String, String) {
    (
        text_or_empty(&node["source_file"]),
        text_or_empty(&node["source_location"]),
        text_or_empty(&node["id"]),
    )
}
/// `_build_csharp_type_def_index`. With `edges`, targets of `case_of`,
/// `defines` and `method` edges (enum members, properties, fields, methods)
/// are never type definitions, even when they share a type's name (#3815).
pub fn build_csharp_type_def_index(nodes: &[Value], edges: Option<&[Value]>) -> TypeIndex {
    type_def_index_of(nodes, edges)
}
/// `edge[key]` (`Null` when absent) on an edge of either record form.
fn edge_at<'a, E: Rec>(edge: &'a E, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    edge.rec_get(key).unwrap_or(&NULL)
}
/// [`build_csharp_type_def_index`] over edges in either record form.
pub fn type_def_index_of<E: Rec>(nodes: &[Value], edges: Option<&[E]>) -> TypeIndex {
    let non_type_ids: HashSet<&str> = edges
        .into_iter()
        .flatten()
        .filter(|e| {
            matches!(
                e.rec_get("relation").and_then(Value::as_str),
                Some("case_of" | "defines" | "method")
            )
        })
        .filter_map(|e| {
            let tgt = edge_at(e, "_tgt");
            let target = if truthy(tgt) {
                tgt
            } else {
                edge_at(e, "target")
            };
            target.as_str()
        })
        .collect();
    let mut candidates: BTreeMap<(String, String), &Value> = BTreeMap::new();
    for node in nodes {
        if node["type"] == "namespace" {
            continue;
        }
        let meta = metadata(&node["metadata"]);
        if meta.get("is_nested_type").is_some_and(truthy) {
            continue;
        }
        let (Some(id), Some(label)) = (node["id"].as_str(), node["label"].as_str()) else {
            continue;
        };
        if id.is_empty()
            || label.is_empty()
            || non_type_ids.contains(id)
            || !is_cs_file(&node["source_file"])
            || node["file_type"] != "code"
            || label.ends_with(')')
            || label.starts_with('.')
            || label.contains('.')
        {
            continue;
        }
        let namespace = meta.get("namespace").and_then(Value::as_str).unwrap_or("");
        let key = (namespace.to_string(), label.to_string());
        match candidates.get(&key) {
            Some(previous) if sort_key(previous) <= sort_key(node) => (),
            _ => {
                candidates.insert(key, node);
            }
        }
    }
    candidates
        .into_iter()
        .map(|(key, node)| (key, node["id"].as_str().unwrap().to_string()))
        .collect()
}
pub fn strip_trailing_csharp_generic_args(value: &str) -> String {
    let value = strip(value);
    if !value.ends_with('>') {
        return value.into();
    }
    let mut depth = 0;
    for (offset, c) in value.char_indices().rev() {
        if c == '>' {
            depth += 1;
        } else if c == '<' {
            depth -= 1;
            if depth == 0 {
                return strip(&value[..offset]).into();
            }
        }
    }
    value.into()
}
fn prune_repointed(nodes: &mut Vec<Value>, edges: &[Value], repointed: &BTreeSet<String>) {
    let referenced: BTreeSet<_> = edges
        .iter()
        .flat_map(|e| [e["source"].as_str(), e["target"].as_str()])
        .flatten()
        .collect();
    nodes.retain(|node| {
        node["id"]
            .as_str()
            .is_none_or(|id| !repointed.contains(id) || referenced.contains(id))
    });
}
pub fn resolve_cross_file_csharp_imports(nodes: &mut Vec<Value>, edges: &mut [Value]) {
    let mut sorted: Vec<_> = nodes.iter().collect();
    sorted.sort_by_key(|n| sort_key(n));
    let mut namespaces = BTreeMap::new();
    for node in sorted {
        if node["type"] != "namespace" {
            continue;
        }
        if let (Some(label), Some(id)) = (
            node["label"].as_str().filter(|s| !s.is_empty()),
            node["id"].as_str().filter(|s| !s.is_empty()),
        ) {
            namespaces
                .entry(label.to_string())
                .or_insert_with(|| id.to_string());
        }
    }
    let types = build_csharp_type_def_index(nodes, Some(edges));
    if namespaces.is_empty() && types.is_empty() {
        return;
    }
    let mut repointed = BTreeSet::new();
    for edge in edges.iter_mut() {
        if edge["relation"] != "imports" {
            continue;
        }
        let meta = metadata(&edge["metadata"]);
        let Some(target) = meta
            .get("target_fqn")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let resolved = match meta.get("using_kind").and_then(Value::as_str) {
            Some("namespace") => namespaces.get(target).cloned(),
            Some("alias") => {
                let base = strip_trailing_csharp_generic_args(&html_compat::unescape(target));
                base.rsplit_once('.').and_then(|(prefix, name)| {
                    namespaces
                        .contains_key(prefix)
                        .then(|| types.get(&(prefix.into(), name.into())).cloned())
                        .flatten()
                })
            }
            _ => None,
        };
        if let Some(resolved) = resolved.filter(|v| edge["target"].as_str() != Some(v)) {
            if let Some(old) = edge["target"].as_str().filter(|s| !s.is_empty()) {
                repointed.insert(old.into());
            }
            edge["target"] = resolved.into();
        }
    }
    if !repointed.is_empty() {
        prune_repointed(nodes, edges, &repointed);
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Using {
    pub target: String,
    pub scope_kind: Value,
    pub scope_id: Value,
}
#[derive(Clone, Debug)]
pub struct CsharpNameResolver {
    pub node_by_id: BTreeMap<String, Value>,
    pub type_def_index: TypeIndex,
    pub known_namespaces: BTreeSet<String>,
    pub namespace_usings_by_file: BTreeMap<String, Vec<Using>>,
    pub aliases_by_file: BTreeMap<String, Vec<(String, Vec<Using>)>>,
    /// `using static Holder;` directives per file (#4235).
    pub static_usings_by_file: BTreeMap<String, Vec<Using>>,
    /// Holder type id -> nested type label -> the canonical nested type id.
    pub nested_types_by_holder: BTreeMap<String, BTreeMap<String, String>>,
}
impl CsharpNameResolver {
    pub fn new(nodes: &[Value], edges: &[Value]) -> Self {
        Self::new_of(nodes, edges)
    }
    /// [`Self::new`] over edges in either record form.
    pub fn new_of<E: Rec>(nodes: &[Value], edges: &[E]) -> Self {
        let node_by_id = nodes
            .iter()
            .filter_map(|node| {
                node["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|id| (id.to_string(), node.clone()))
            })
            .collect();
        let type_def_index = type_def_index_of(nodes, Some(edges));
        let known_namespaces = nodes
            .iter()
            .filter(|n| n["type"] == "namespace")
            .filter_map(|n| n["label"].as_str().map(str::to_string))
            .collect();
        let mut out = Self {
            node_by_id,
            type_def_index,
            known_namespaces,
            namespace_usings_by_file: BTreeMap::new(),
            aliases_by_file: BTreeMap::new(),
            static_usings_by_file: BTreeMap::new(),
            nested_types_by_holder: BTreeMap::new(),
        };
        for edge in edges {
            if *edge_at(edge, "relation") != "imports" {
                continue;
            }
            let Some(source) = edge_at(edge, "source")
                .as_str()
                .and_then(|id| out.node_by_id.get(id))
            else {
                continue;
            };
            if !is_dotnet_source_file(&source["label"])
                || !is_dotnet_source_file(&source["source_file"])
            {
                continue;
            }
            let source_file = source["source_file"].as_str().unwrap().to_string();
            let meta = metadata(edge_at(edge, "metadata"));
            let Some(target) = meta
                .get("target_fqn")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let entry = Using {
                target: target.into(),
                scope_kind: meta
                    .get("scope_kind")
                    .filter(|v| truthy(v))
                    .cloned()
                    .unwrap_or(json!("file")),
                scope_id: meta.get("scope_id").cloned().unwrap_or(Value::Null),
            };
            let bucket = match meta.get("using_kind").and_then(Value::as_str) {
                Some("namespace") => out.namespace_usings_by_file.entry(source_file).or_default(),
                Some("static") => out.static_usings_by_file.entry(source_file).or_default(),
                Some("alias") => {
                    let Some(alias) = meta
                        .get("alias")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let names = out.aliases_by_file.entry(source_file).or_default();
                    let index = names
                        .iter()
                        .position(|(name, _)| name == alias)
                        .unwrap_or_else(|| {
                            names.push((alias.into(), vec![]));
                            names.len() - 1
                        });
                    &mut names[index].1
                }
                _ => continue,
            };
            if !bucket.contains(&entry) {
                bucket.push(entry);
            }
        }
        // Nested types per holder, from `contains` edges: the canonical one of
        // several same-labelled nested nodes is the first by (source_file,
        // source_location, id).
        let mut nested: BTreeMap<(String, String), BTreeMap<String, &Value>> = BTreeMap::new();
        for edge in edges {
            if *edge_at(edge, "relation") != "contains" {
                continue;
            }
            let Some(target) = edge_at(edge, "target").as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(node) = out.node_by_id.get(target) else {
                continue;
            };
            if node["file_type"] != "code"
                || metadata(&node["metadata"]).get("is_nested_type") != Some(&Value::Bool(true))
            {
                continue;
            }
            let Some(label) = node["label"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(holder) = edge_at(edge, "source").as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            nested
                .entry((holder.to_string(), label.to_string()))
                .or_default()
                .insert(target.to_string(), node);
        }
        let mut by_holder: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for ((holder, label), nodes) in nested {
            let canonical = nodes
                .values()
                .min_by_key(|n| {
                    (
                        nested_sort_text(&n["source_file"]),
                        nested_sort_text(&n["source_location"]),
                        nested_sort_text(&n["id"]),
                    )
                })
                .and_then(|n| n["id"].as_str())
                .unwrap_or_default()
                .to_string();
            by_holder
                .entry(holder)
                .or_default()
                .insert(label, canonical);
        }
        out.nested_types_by_holder = by_holder;
        out
    }
    /// `_resolve_holder_nid`: the type a `using static` target names, a
    /// top-level type or a type nested (at any depth) in one.
    pub fn resolve_holder(&self, target: &str) -> Option<String> {
        let base = strip(&strip_trailing_csharp_generic_args(&html_compat::unescape(
            target,
        )))
        .to_string();
        if base.is_empty() {
            return None;
        }
        let Some((prefix, name)) = base.rsplit_once('.') else {
            return self.type_def_index.get(&(String::new(), base)).cloned();
        };
        if let Some(hit) = self.type_def_index.get(&(prefix.into(), name.into())) {
            return Some(hit.clone());
        }
        let parent = self.resolve_holder(prefix)?;
        self.nested_types_by_holder
            .get(&parent)
            .and_then(|types| types.get(name))
            .cloned()
    }
    pub fn namespace(node: &Value) -> String {
        metadata(&node["metadata"])
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into()
    }
    pub fn scope_chain(node: &Value) -> Vec<Value> {
        metadata(&node["metadata"])
            .get("scope_chain")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }
    pub fn using_in_scope(kind: &Value, id: &Value, node: &Value) -> bool {
        kind == "file" || (!id.is_null() && Self::scope_chain(node).contains(id))
    }
    pub fn scopes_for(&self, node: &Value, file: &str) -> Vec<String> {
        let mut scopes = vec![Self::namespace(node)];
        if !scopes.contains(&String::new()) {
            scopes.push(String::new());
        }
        for entry in self
            .namespace_usings_by_file
            .get(file)
            .into_iter()
            .flatten()
        {
            if Self::using_in_scope(&entry.scope_kind, &entry.scope_id, node)
                && !scopes.contains(&entry.target)
            {
                scopes.push(entry.target.clone());
            }
        }
        scopes
    }
    fn aliases(&self, file: &str, label: &str) -> Option<&Vec<Using>> {
        self.aliases_by_file
            .get(file)?
            .iter()
            .find(|(name, _)| name == label)
            .map(|(_, v)| v)
    }
    pub fn resolve_alias(&self, label: &str, node: &Value, file: &str) -> Option<String> {
        let mut hits = BTreeSet::new();
        for entry in self.aliases(file, label).into_iter().flatten() {
            if !Self::using_in_scope(&entry.scope_kind, &entry.scope_id, node) {
                continue;
            }
            let base = strip_trailing_csharp_generic_args(&html_compat::unescape(&entry.target));
            // Preserve the pinned rpartition behavior for an unqualified alias.
            let (namespace, name) = base.rsplit_once('.').unwrap_or(("", ""));
            if name.is_empty() {
                continue;
            }
            if let Some(hit) = self.type_def_index.get(&(namespace.into(), name.into())) {
                hits.insert(hit.clone());
            }
        }
        (hits.len() == 1).then(|| hits.into_iter().next().unwrap())
    }
    pub fn resolve_type_name(
        &self,
        label: &str,
        node: &Value,
        file: &str,
    ) -> (Option<String>, bool) {
        if self.aliases(file, label).is_some() {
            return (self.resolve_alias(label, node, file), true);
        }
        let mut candidates = BTreeSet::new();
        for namespace in self.scopes_for(node, file) {
            if let Some(hit) = self.type_def_index.get(&(namespace, label.into())) {
                candidates.insert(hit.clone());
            }
        }
        // `using static Holder;` brings Holder's nested types into scope (#4235).
        for entry in self.static_usings_by_file.get(file).into_iter().flatten() {
            if Self::using_in_scope(&entry.scope_kind, &entry.scope_id, node)
                && let Some(holder) = self.resolve_holder(&entry.target)
                && let Some(hit) = self
                    .nested_types_by_holder
                    .get(&holder)
                    .and_then(|types| types.get(label))
            {
                candidates.insert(hit.clone());
            }
        }
        if !candidates.is_empty() {
            return (
                (candidates.len() == 1).then(|| candidates.into_iter().next().unwrap()),
                true,
            );
        }
        // A type declared in an enclosing namespace is visible from a nested
        // one, innermost first (#4204).
        let namespace = Self::namespace(node);
        let parts = namespace.split('.').collect::<Vec<_>>();
        if parts.len() > 1 {
            for i in (1..parts.len()).rev() {
                if let Some(hit) = self
                    .type_def_index
                    .get(&(parts[..i].join("."), label.into()))
                {
                    return (Some(hit.clone()), true);
                }
            }
        }
        (None, false)
    }
    pub fn resolve_label(&self, label: &str, node: &Value, file: &str) -> Option<String> {
        self.resolve_type_name(label, node, file).0
    }
    pub fn resolve_qualified(
        &self,
        label: &str,
        qualifier: &Value,
        node: &Value,
        file: &str,
    ) -> Option<String> {
        let qualifier = qualifier.as_str().filter(|s| !s.is_empty())?;
        let in_scope: Vec<_> = self
            .aliases(file, qualifier)
            .into_iter()
            .flatten()
            .filter(|entry| Self::using_in_scope(&entry.scope_kind, &entry.scope_id, node))
            .collect();
        if !in_scope.is_empty() {
            let hits: BTreeSet<_> = in_scope
                .into_iter()
                .filter_map(|entry| {
                    let namespace =
                        strip_trailing_csharp_generic_args(&html_compat::unescape(&entry.target));
                    self.type_def_index.get(&(namespace, label.into())).cloned()
                })
                .collect();
            return (hits.len() == 1).then(|| hits.into_iter().next().unwrap());
        }
        if self.known_namespaces.contains(qualifier) {
            self.type_def_index
                .get(&(qualifier.into(), label.into()))
                .cloned()
        } else {
            None
        }
    }
}
/// `str(n.get(key) or "")`, the sort text of the canonical nested type.
fn nested_sort_text(value: &Value) -> String {
    if truthy(value) {
        string(value)
    } else {
        String::new()
    }
}
fn placeholder(node: &Value) -> bool {
    truthy(node) && !truthy(&node["source_file"])
}
fn label_for_target(target: &Value, file: &str, resolver: &CsharpNameResolver) -> Option<String> {
    let label = target["label"].as_str().filter(|s| !s.is_empty())?;
    let Some(stem) = label.strip_suffix(".cs") else {
        return Some(label.into());
    };
    for (alias, _) in resolver.aliases_by_file.get(file).into_iter().flatten() {
        if alias.to_lowercase() == stem.to_lowercase() || make_id([alias]) == make_id([stem]) {
            return Some(alias.clone());
        }
    }
    (!stem.is_empty()).then(|| stem.into())
}
/// `_dangling_stub_id`: one canonical sourceless placeholder per undeclared
/// type name, shared by every file that references it (#4247). A placeholder
/// already keyed by the bare name is reused; a real or differently labelled
/// node there is never shadowed.
fn dangling_stub(
    label: &str,
    nodes: &mut Vec<Value>,
    resolver: &mut CsharpNameResolver,
    canonical: &mut BTreeMap<String, String>,
) -> String {
    if let Some(id) = canonical.get(label) {
        return id.clone();
    }
    let matches = |id: &str, resolver: &CsharpNameResolver| {
        resolver
            .node_by_id
            .get(id)
            .is_some_and(|n| placeholder(n) && n["label"] == label)
    };
    let mut id = make_id([label]);
    if resolver.node_by_id.contains_key(&id) && !matches(&id, resolver) {
        id = make_id(["csharp_type_ref", label]);
        let mut suffix = 2;
        while resolver.node_by_id.contains_key(&id) && !matches(&id, resolver) {
            id = make_id(["csharp_type_ref", label, &suffix.to_string()]);
            suffix += 1;
        }
    }
    if !resolver.node_by_id.contains_key(&id) {
        let node =
            json!({"id":id,"label":label,"file_type":"code","source_file":"","source_location":""});
        nodes.push(node.clone());
        resolver.node_by_id.insert(id.clone(), node);
    }
    canonical.insert(label.to_string(), id.clone());
    id
}
pub fn resolve_csharp_type_references(nodes: &mut Vec<Value>, edges: &mut [Value]) -> Result<()> {
    let mut resolver = CsharpNameResolver::new(nodes, edges);
    let mut repointed = BTreeSet::new();
    let mut canonical = BTreeMap::new();
    for edge in edges.iter_mut() {
        if !matches!(
            edge["relation"].as_str(),
            Some("implements" | "inherits" | "references")
        ) || !is_dotnet_source_file(&edge["source_file"])
        {
            continue;
        }
        let file = edge["source_file"].as_str().unwrap().to_string();
        let Some(source) = edge["source"]
            .as_str()
            .and_then(|id| resolver.node_by_id.get(id))
            .cloned()
        else {
            continue;
        };
        let Some(target) = edge["target"]
            .as_str()
            .and_then(|id| resolver.node_by_id.get(id))
            .cloned()
        else {
            continue;
        };
        if target["type"] != "namespace"
            && truthy(&target["source_file"])
            && !is_cs_file(&target["source_file"])
        {
            continue;
        }
        let meta = metadata(&edge["metadata"]);
        let label = if let Some(value) = meta.get("ref_token").filter(|v| truthy(v)) {
            Some(
                value
                    .as_str()
                    .ok_or("C# reference token must be a string")?
                    .to_string(),
            )
        } else {
            label_for_target(&target, &file, &resolver)
        };
        let Some(label) = label.filter(|s| !s.is_empty()) else {
            continue;
        };
        let resolved = if meta.get("qualified").is_some_and(truthy) {
            resolver.resolve_qualified(
                &label,
                meta.get("ref_qualifier").unwrap_or(&Value::Null),
                &source,
                &file,
            )
        } else {
            resolver.resolve_label(&label, &source, &file)
        };
        let previous = edge["target"].clone();
        let desired =
            resolved.unwrap_or_else(|| dangling_stub(&label, nodes, &mut resolver, &mut canonical));
        if previous.as_str() != Some(&desired) {
            edge["target"] = desired.into();
            if placeholder(&target)
                && let Some(id) = previous.as_str()
            {
                repointed.insert(id.into());
            }
        }
    }
    if !repointed.is_empty() {
        prune_repointed(nodes, edges, &repointed);
    }
    Ok(())
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let operation = request["operation"].as_str().unwrap_or("");
    if operation == "csharp.unescape" {
        return Ok(json!(html_compat::unescape(
            request["text"].as_str().ok_or("Expected text")?
        )));
    }
    if operation == "csharp.strip_generic" {
        return Ok(json!(strip_trailing_csharp_generic_args(
            request["text"].as_str().ok_or("Expected type name")?
        )));
    }
    if operation == "csharp.is_cs" {
        return Ok(json!(is_cs_file(&request["value"])));
    }
    if operation == "csharp.is_dotnet" {
        return Ok(json!(is_dotnet_source_file(&request["value"])));
    }
    if operation == "csharp.metadata" {
        return Ok(json!(metadata(&request["value"])));
    }
    let mut nodes = request["nodes"].as_array().ok_or("Expected nodes")?.clone();
    let mut edges = request["edges"].as_array().cloned().unwrap_or_default();
    if operation == "csharp.index" {
        return Ok(json!(
            build_csharp_type_def_index(&nodes, request["edges"].as_array().map(Vec::as_slice))
                .into_iter()
                .collect::<Vec<_>>()
        ));
    }
    if operation == "csharp.imports" {
        resolve_cross_file_csharp_imports(&mut nodes, &mut edges);
        return Ok(json!({"nodes":nodes,"edges":edges}));
    }
    if operation == "csharp.references" {
        resolve_csharp_type_references(&mut nodes, &mut edges)?;
        return Ok(json!({"nodes":nodes,"edges":edges}));
    }
    let resolver = CsharpNameResolver::new(&nodes, &edges);
    let node = &request["source_node"];
    let label = request["label"].as_str().unwrap_or("");
    let file = request["source_file"].as_str().unwrap_or("");
    match operation {
        "csharp.resolve_type" => Ok(json!(resolver.resolve_type_name(label, node, file))),
        "csharp.resolve_label" => Ok(json!(resolver.resolve_label(label, node, file))),
        "csharp.resolve_alias" => Ok(json!(resolver.resolve_alias(label, node, file))),
        "csharp.resolve_qualified" => Ok(json!(resolver.resolve_qualified(
            label,
            &request["qualifier"],
            node,
            file
        ))),
        "csharp.scopes" => Ok(json!(resolver.scopes_for(node, file))),
        "csharp.scope_chain" => Ok(json!(CsharpNameResolver::scope_chain(node))),
        "csharp.namespace" => Ok(json!(CsharpNameResolver::namespace(node))),
        "csharp.using_in_scope" => Ok(json!(CsharpNameResolver::using_in_scope(
            &request["kind"],
            &request["id"],
            node
        ))),
        _ => Err("Unknown C# resolution operation".into()),
    }
}
