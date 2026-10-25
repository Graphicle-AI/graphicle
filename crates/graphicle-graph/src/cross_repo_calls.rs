// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cross_repo_calls.py
//! Native translation of graphify/cross_repo_calls.py.
use crate::{
    Result,
    extractors::csharp::CsharpNameResolver,
    graph::Graph,
    python_compat::{scalar_key, string},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub const CROSS_REPO_CALL_MARKER: &str = "_cross_repo_call";
pub const UNRESOLVED_CALLS_KEY: &str = "unresolved_calls";
pub const MEMBER_RELATIONS: &[&str] = &["defines", "method"];
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
fn value<'a>(data: &'a impl crate::record::Properties, name: &str) -> &'a Value {
    data.get(name).unwrap_or(&Value::Null)
}
fn text(value: &Value) -> String {
    if truthy(value) {
        string(value)
    } else {
        String::new()
    }
}
pub fn key(value: &Value) -> String {
    let s = text(value);
    let s = s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    let s = s.strip_prefix('.').unwrap_or(s);
    s.strip_suffix("()").unwrap_or(s).into()
}
pub fn suffix(value: &Value) -> String {
    let path = text(value);
    let name = path.rsplit('/').next().unwrap_or("");
    if let Some(at) = name.rfind('.')
        && name[..at].chars().any(|c| c != '.')
    {
        return name[at..].to_lowercase();
    }
    String::new()
}
pub fn language_suffixes(lang: &str) -> &'static [&'static str] {
    match lang {
        "cpp" => &[
            ".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".h", ".cu", ".cuh",
        ],
        "csharp" => &[".cs"],
        "go" => &[".go"],
        "java" => &[".java"],
        "kotlin" => &[".kt", ".kts"],
        "objc" => &[".h", ".m", ".mm"],
        "swift" => &[".swift"],
        "php" => &[
            ".php", ".phtml", ".php3", ".php4", ".php5", ".php7", ".phps",
        ],
        // TS and JS share one key: a TS class answers a JS call site, and an
        // SFC script block is TS/JS too.
        "typescript" => &[
            ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte",
            ".astro",
        ],
        _ => &[],
    }
}
/// `_member_keys`: the member-index keys a parked callee may answer to. An
/// Objective-C selector carries no class/instance distinction, so both
/// sigils are tried (a type declaring `+greet` and `-greet` is ambiguous).
pub fn member_keys(lang: &str, callee: &str) -> Vec<String> {
    if lang == "objc" {
        let selector = callee.trim_start_matches(['+', '-']);
        vec![format!("-{selector}"), format!("+{selector}")]
    } else {
        vec![callee.to_string()]
    }
}
pub fn member_relations(lang: &str) -> &'static [&'static str] {
    if lang == "cpp" {
        &["method", "defines"]
    } else {
        &["method"]
    }
}
pub fn parked_entries(data: &impl crate::record::Properties) -> Vec<&Value> {
    data.get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| m.get(UNRESOLVED_CALLS_KEY))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|v| v.is_object())
        .collect()
}
pub fn drop_previous_output(graph: &mut Graph) {
    let stale = graph
        .iter_edges()
        .filter(|(_, _, e)| truthy(value(&e.attributes, CROSS_REPO_CALL_MARKER)))
        .map(|(u, v, _)| (graph.nodes[u].id.clone(), graph.nodes[v].id.clone(), None))
        .collect::<Vec<_>>();
    graph.remove_edges_from(&stale);
}
pub fn index_declarations(graph: &Graph) -> (BTreeMap<String, Vec<usize>>, BTreeSet<usize>) {
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        let data = &node.attributes;
        if !truthy(value(data, "_callable_class"))
            || !truthy(value(data, "source_file"))
            || !truthy(value(data, "repo"))
        {
            continue;
        }
        let name = key(value(data, "label"));
        if name.is_empty() {
            continue;
        }
        by_name.entry(name).or_default().push(i);
        ids.insert(i);
    }
    (by_name, ids)
}
pub type Members = BTreeMap<String, BTreeMap<(usize, String), Vec<usize>>>;
pub fn index_members(graph: &Graph, types: &BTreeSet<usize>) -> Members {
    let mut out: Members = MEMBER_RELATIONS
        .iter()
        .map(|s| (s.to_string(), BTreeMap::new()))
        .collect();
    for (u, v, edge) in graph.iter_edges() {
        let Some(relation) = edge.attributes.get("relation").and_then(Value::as_str) else {
            continue;
        };
        let Some(index) = out.get_mut(relation) else {
            continue;
        };
        let (owner, member) = match (types.contains(&u), types.contains(&v)) {
            (true, false) => (u, v),
            (false, true) => (v, u),
            _ => continue,
        };
        let name = key(value(&graph.nodes[member].attributes, "label"));
        if !name.is_empty() {
            index.entry((owner, name)).or_default().push(member);
        }
    }
    out
}
fn equivalent(left: &Value, right: &Value) -> bool {
    match (scalar_key(left), scalar_key(right)) {
        (Some(l), Some(r)) => l == r,
        _ => left == right,
    }
}
/// `_objc_union_definition`: the sole definition of an Objective-C selector
/// among same-named candidate classes in other repositories. A member defines
/// the selector when it lives in a `.m`/`.mm` or names its implementation in
/// `definition_file`; a bare header declaration does not. Two definitions are
/// two unrelated classes: no edge.
fn objc_union_definition(
    graph: &Graph,
    members: &BTreeMap<(usize, String), Vec<usize>>,
    candidates: &[usize],
    callee: &str,
) -> Option<usize> {
    let mut definitions: Vec<usize> = vec![];
    for &candidate in candidates {
        for member_key in member_keys("objc", callee) {
            for &member in members.get(&(candidate, member_key)).into_iter().flatten() {
                let data = &graph.nodes[member].attributes;
                if (truthy(value(data, "definition_file"))
                    || matches!(suffix(value(data, "source_file")).as_str(), ".m" | ".mm"))
                    && !definitions.contains(&member)
                {
                    definitions.push(member);
                }
            }
        }
    }
    (definitions.len() == 1).then(|| definitions[0])
}
pub fn link_cross_repo_member_calls(graph: &mut Graph) -> Result<usize> {
    drop_previous_output(graph);
    let parked: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| !parked_entries(&n.attributes).is_empty())
        .map(|(i, n)| (i, n.attributes.clone()))
        .collect();
    if parked.is_empty() {
        return Ok(0);
    }
    let (by_name, type_ids) = index_declarations(graph);
    if by_name.is_empty() {
        return Ok(0);
    }
    let members = index_members(graph, &type_ids);
    let resolver = if parked.iter().any(|(_, data)| {
        parked_entries(data)
            .iter()
            .any(|e| text(&e["lang"]) == "csharp")
    }) {
        let snapshot = graph.to_node_link("links");
        Some(CsharpNameResolver::new(
            snapshot["nodes"].as_array().unwrap(),
            snapshot["links"].as_array().unwrap(),
        ))
    } else {
        None
    };
    let mut added = 0;
    for (caller, data) in parked {
        let repo = value(&data, "repo");
        if !truthy(repo) {
            continue;
        }
        for entry in parked_entries(&data) {
            let lang = text(&entry["lang"]);
            let suffixes = language_suffixes(&lang);
            let receiver = key(&entry["receiver_type"]);
            let callee = key(&entry["callee"]);
            if suffixes.is_empty() || receiver.is_empty() || callee.is_empty() {
                continue;
            }
            let candidates: Vec<_> = by_name
                .get(&receiver)
                .into_iter()
                .flatten()
                .filter(|&&i| {
                    let candidate = &graph.nodes[i].attributes;
                    !equivalent(value(candidate, "repo"), repo)
                        && suffixes.contains(&suffix(value(candidate, "source_file")).as_str())
                })
                .copied()
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // An Objective-C class split `@interface` (.h) / `@implementation`
            // (.m) across repositories leaves several same-named candidates:
            // the selector binds to its sole definition (0.9.95, #4690).
            let target = if lang == "objc" && candidates.len() > 1 {
                match objc_union_definition(graph, &members["method"], &candidates, &callee) {
                    Some(target) => target,
                    None => continue,
                }
            } else {
                // The single-repo guard: two repos declaring the name are an
                // ambiguity, not a hit.
                if candidates.len() != 1 {
                    continue;
                }
                if lang == "csharp" {
                    let mut node = data.clone();
                    node.insert("id".into(), graph.nodes[caller].id.clone());
                    let (resolved, _) = resolver.as_ref().unwrap().resolve_type_name(
                        &receiver,
                        &Value::Object(node.into()),
                        &text(value(&data, "source_file")),
                    );
                    if resolved.as_ref().map(|s| json!(s)).as_ref()
                        != Some(&graph.nodes[candidates[0]].id)
                    {
                        continue;
                    }
                }
                let mut targets: Vec<usize> = Vec::new();
                for relation in member_relations(&lang) {
                    for member_key in member_keys(&lang, &callee) {
                        if let Some(found) = members[*relation].get(&(candidates[0], member_key)) {
                            targets.extend(found.iter().copied());
                        }
                    }
                    if !targets.is_empty() {
                        break;
                    }
                }
                if targets.len() != 1 {
                    continue;
                }
                targets[0]
            };
            if target == caller
                || graph.number_of_edges(&graph.nodes[caller].id, &graph.nodes[target].id) > 0
            {
                continue;
            }
            // The parked call-site file (the `.m`), not the caller node's own
            // file (often the `.h`): the location is a call-site line
            // (0.9.94, #4668). Older graphs have no `file`.
            let source_file = if truthy(&entry["file"]) {
                text(&entry["file"])
            } else {
                text(value(&data, "source_file"))
            };
            let attrs = json!({"relation":"calls","context":"cross_repo","confidence":"INFERRED","confidence_score":0.85,"source_file":source_file,"source_location":entry["line"],"weight":1.0,
                "_src":graph.nodes[caller].id,"_tgt":graph.nodes[target].id,CROSS_REPO_CALL_MARKER:true});
            graph.add_edge(
                graph.nodes[caller].id.clone(),
                graph.nodes[target].id.clone(),
                None,
                attrs.as_object().unwrap().clone(),
            )?;
            added += 1;
        }
    }
    Ok(added)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let operation = request["operation"].as_str().unwrap_or("");
    match operation {
        "cross_calls.key" => return Ok(json!(key(&request["value"]))),
        "cross_calls.suffix" => return Ok(json!(suffix(&request["value"]))),
        "cross_calls.entries" => {
            return Ok(json!(parked_entries(
                request["data"]
                    .as_object()
                    .ok_or("Expected node attributes")?
            )));
        }
        "cross_calls.member_relations" => {
            return Ok(json!(member_relations(
                request["language"].as_str().unwrap_or("")
            )));
        }
        "cross_calls.member_keys" => {
            return Ok(json!(member_keys(
                request["language"].as_str().unwrap_or(""),
                request["callee"].as_str().unwrap_or("")
            )));
        }
        "cross_calls.language_suffixes" => {
            let mut suffixes =
                language_suffixes(request["language"].as_str().unwrap_or("")).to_vec();
            suffixes.sort_unstable();
            return Ok(json!(suffixes));
        }
        _ => (),
    }
    let mut graph = Graph::from_node_link(&request["graph"], "links")?;
    if operation == "cross_calls.drop" {
        drop_previous_output(&mut graph);
        return Ok(graph.to_node_link("links"));
    }
    let added = link_cross_repo_member_calls(&mut graph)?;
    Ok(json!({"added":added,"graph":graph.to_node_link("links")}))
}
