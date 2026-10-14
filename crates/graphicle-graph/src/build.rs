// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py
//! Native graph construction and conservative normalization of extraction output.
pub use crate::build_global::{distinct_repo_tags, prefix_graph_for_global, prune_repo_from_graph};
pub use crate::build_labels::is_file_node_label;
pub use crate::build_merge::{
    MergeOptions, build_merge, build_merge_owned, load_existing_graph, merge_raw_extraction,
};
pub use crate::build_normalize::*;
use crate::{
    Result,
    dedup::{self, DedupOptions, TiebreakClient},
    dedup_strings,
    extractors::{
        base,
        resolution::{field, is_python_stdlib_module, map_field, map_field_mut},
    },
    fsutil,
    graph::Graph,
    hyperedge_members, ids,
    python_compat::{self, scalar_key},
    validate,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

pub const GENERIC_RELATIONS: &[&str] = &["references", "uses", "mentions"];
/// Relations whose undeclared target gets a typed external stub node so every
/// edge endpoint resolves (#2873). `calls` is deliberately absent: a call to an
/// external target survives only when its module was imported too, since the
/// import edge pre-mints the stub (#3793); a call to a never-imported target is
/// still dropped as a phantom (#3156).
pub const EXTERNAL_STUB_RELATIONS: &[&str] = &["imports", "imports_from", "re_exports"];
/// Membership of a type or file (0.9.83, #4256). On an undirected graph these
/// share a node pair with a `calls` edge in the opposite direction when a
/// member calls or constructs its owner; the membership edge wins that
/// collapse, so the member stays reachable from its type. Directed graphs
/// keep both, and same-direction `contains`/`method` vs `calls` is not ranked.
pub const STRUCTURAL_MEMBER_RELATIONS: &[&str] = &["method", "contains"];
#[derive(Default, Debug, Serialize)]
pub struct Diagnostics {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildOptions {
    pub directed: bool,
    pub dedup: bool,
    pub dedup_llm_backend: Option<String>,
    pub root: Option<PathBuf>,
    pub protected_ids: BTreeSet<String>,
    /// Keep parallel edges with different relations between the same pair,
    /// keyed by relation. The parity default (`false`) folds them into one
    /// edge exactly as the Python `build_graph` does.
    pub multigraph: bool,
}
impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            directed: false,
            dedup: true,
            dedup_llm_backend: None,
            root: None,
            protected_ids: BTreeSet::new(),
            multigraph: false,
        }
    }
}
pub fn edge_datas<'a>(
    graph: &'a Graph,
    source: &Value,
    target: &Value,
) -> Result<Vec<&'a crate::record::Attributes>> {
    let edges = graph.edges_between(source, target);
    if edges.is_empty() {
        return Err(format!(
            "Missing graph edge {} → {}",
            python_compat::repr(source),
            python_compat::repr(target)
        )
        .into());
    }
    Ok(edges.into_iter().map(|e| &e.attributes).collect())
}
pub fn edge_data<'a>(
    graph: &'a Graph,
    source: &Value,
    target: &Value,
) -> Result<&'a crate::record::Attributes> {
    Ok(edge_datas(graph, source, target)?[0])
}
fn external_stub(nid: &str) -> Value {
    json!({"id":nid,"label":nid,"file_type":"concept","type":"external","external":true,"source_file":""})
}
pub fn mint_external_stub(graph: &mut Graph, nid: &str) -> Result<()> {
    if graph.node_index(&json!(nid)).is_none() {
        let mut attributes = external_stub(nid).as_object().unwrap().clone();
        attributes.shift_remove("id");
        graph.add_node(json!(nid), attributes)?;
    }
    Ok(())
}
pub fn mint_external_stubs_in_data(data: &mut Value) {
    let Some(nodes) = data["nodes"].as_array() else {
        return;
    };
    let links = data["links"]
        .as_array()
        .or_else(|| data["edges"].as_array());
    let Some(links) = links else {
        return;
    };
    let declared: BTreeSet<_> = nodes
        .iter()
        .filter(|n| n.is_object())
        .filter_map(|n| scalar_key(&n["id"]))
        .collect();
    let mut minted = BTreeSet::new();
    let mut additions = Vec::new();
    for edge in links {
        if !edge["relation"]
            .as_str()
            .is_some_and(|s| EXTERNAL_STUB_RELATIONS.contains(&s))
        {
            continue;
        }
        let Some(target) = edge["target"].as_str() else {
            continue;
        };
        if scalar_key(&edge["source"]).is_some_and(|k| declared.contains(&k))
            && !declared.contains(&scalar_key(&edge["target"]).unwrap())
            && minted.insert(target)
        {
            additions.push(external_stub(target));
        }
    }
    data["nodes"].as_array_mut().unwrap().extend(additions);
}
/// `finalize_raw_graph_endpoints` (0.9.80, #4189/#4181): apply
/// [`build_from_json`]'s endpoint rules to a raw (`--no-cluster`) graph. An
/// import of an external module gets its typed stub; an endpoint that differs
/// from a declared id only by normalization is repointed to it; every other
/// edge without two declared endpoints is dropped; and a stub no edge or
/// hyperedge references any more is dropped, as a fresh build would not have it.
pub fn finalize_raw_graph_endpoints(data: &mut Value) {
    mint_external_stubs_in_data(data);
    let key = if data.get("links").is_some_and(Value::is_array) {
        "links"
    } else {
        "edges"
    };
    if !data.get("nodes").is_some_and(Value::is_array)
        || !data.get(key).is_some_and(Value::is_array)
    {
        return;
    }
    let mut declared = data["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_object()?.get("id")?.as_str())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    declared.sort();
    declared.dedup();
    // Two ids with one normalized spelling: the later in sorted order wins,
    // the same stable choice build_from_json makes (the reference iterates a set).
    let norm_to_id: HashMap<String, String> = declared
        .iter()
        .map(|nid| (ids::normalize_id(nid), nid.clone()))
        .collect();
    let declared: std::collections::HashSet<String> = declared.into_iter().collect();
    let mut referenced = BTreeSet::new();
    let links = std::mem::take(data[key].as_array_mut().unwrap());
    let mut kept = Vec::with_capacity(links.len());
    for mut edge in links {
        let Some(object) = edge.as_object_mut() else {
            kept.push(edge);
            continue;
        };
        let mut attached = true;
        for (primary, alias) in [("source", "from"), ("target", "to")] {
            let field = if object.contains_key(primary) {
                primary
            } else {
                alias
            };
            match object.get(field).and_then(Value::as_str) {
                Some(endpoint) if declared.contains(endpoint) => {}
                Some(endpoint) => match norm_to_id.get(&ids::normalize_id(endpoint)) {
                    Some(canonical) => {
                        object.insert(field.into(), json!(canonical));
                    }
                    None => {
                        attached = false;
                        break;
                    }
                },
                None => {
                    attached = false;
                    break;
                }
            }
        }
        if attached {
            for (primary, alias) in [("source", "from"), ("target", "to")] {
                let endpoint = object.get(primary).or_else(|| object.get(alias));
                if let Some(key) = scalar_key(endpoint.unwrap_or(&Value::Null)) {
                    referenced.insert(key);
                }
            }
            kept.push(edge);
        }
    }
    data[key] = Value::Array(kept);
    for hyperedge in data
        .get("hyperedges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        let members = hyperedge
            .get("nodes")
            .or_else(|| hyperedge.get("members"))
            .or_else(|| hyperedge.get("node_ids"));
        for member in members
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            referenced.insert(python_compat::ScalarKey::Text(member.to_owned()));
        }
    }
    if let Some(nodes) = data["nodes"].as_array_mut() {
        nodes.retain(|n| {
            !is_external_stub(n)
                || scalar_key(n.get("id").unwrap_or(&Value::Null))
                    .is_some_and(|id| referenced.contains(&id))
        });
    }
}
/// `_fold_type_only` for a stored edge and an incoming one ([`execution_wins`]).
fn fold_type_only(prior: &mut crate::record::Attributes, incoming: &mut Map<String, Value>) {
    if prior.get("type_only").is_some_and(truthy) != incoming.get("type_only").is_some_and(truthy) {
        prior.shift_remove("type_only");
        incoming.shift_remove("type_only");
    }
}
pub fn disambiguate_file_node_labels(graph: &mut Graph) {
    let items: Vec<_> = graph
        .nodes
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                n.attributes.get("label").cloned().unwrap_or(Value::Null),
                n.attributes
                    .get("source_file")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        })
        .collect();
    for (id, label) in file_label_reassignments(&items) {
        if let Some(i) = graph.node_index(&id) {
            graph.nodes[i]
                .attributes
                .insert("label".into(), json!(label));
        }
    }
}
fn family(extension: &str) -> Option<&'static str> {
    Some(match extension {
        ".py" | ".pyi" => "py",
        ".js" | ".mjs" | ".cjs" | ".jsx" | ".ts" | ".tsx" | ".mts" | ".cts" => "js",
        ".go" => "go",
        ".rs" => "rs",
        ".java" | ".kt" | ".scala" | ".groovy" => "jvm",
        ".c" | ".h" | ".cc" | ".cpp" | ".hpp" | ".cxx" | ".hh" | ".hxx" | ".cu" | ".cuh"
        | ".metal" | ".m" | ".mm" => "c",
        ".rb" | ".rake" => "rb",
        ".php" => "php",
        ".cs" => "cs",
        ".swift" => "swift",
        ".lua" => "lua",
        _ => return None,
    })
}
fn normalized_file_type(value: &Value) -> Result<Value> {
    if value.is_null() || value.as_str() == Some("") {
        return Ok(json!("concept"));
    }
    let ft = value.as_str().ok_or("Expected file_type string")?;
    if validate::VALID_FILE_TYPES.contains(&ft) {
        return Ok(value.clone());
    }
    Ok(json!(match ft {
        "markdown" | "text" => "document",
        "tool" | "library" => "code",
        _ => "concept",
    }))
}
fn numeric_field(value: &Value) -> f64 {
    let number = match value {
        Value::Number(v) => v.as_f64(),
        Value::Bool(v) => Some(u8::from(*v) as f64),
        Value::String(s) => {
            let s: String = s
                .trim_matches(dedup_strings::whitespace)
                .chars()
                .map(|c| {
                    let ranges = crate::decimal_data::DECIMAL;
                    let i = ranges.partition_point(|&(lo, _)| lo <= c as u32);
                    if i > 0 && c as u32 <= ranges[i - 1].1 {
                        char::from_u32('0' as u32 + (c as u32 - ranges[i - 1].0) % 10).unwrap()
                    } else {
                        c
                    }
                })
                .collect();
            let bytes = s.as_bytes();
            if bytes.iter().enumerate().any(|(i, &b)| {
                b == b'_'
                    && (i == 0
                        || i + 1 == bytes.len()
                        || !bytes[i - 1].is_ascii_digit()
                        || !bytes[i + 1].is_ascii_digit())
            }) {
                None
            } else {
                s.replace('_', "").parse::<f64>().ok()
            }
        }
        _ => None,
    };
    number.filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(1.0)
}
fn value_string(attributes: &impl crate::record::Properties, key: &str) -> String {
    attributes
        .get(key)
        .map(python_compat::string)
        .unwrap_or_default()
}
fn remap_extraction(extraction: &mut Value, remap: &BTreeMap<String, String>, remove_nodes: bool) {
    if remap.is_empty() {
        return;
    }
    if let Some(nodes) = extraction.get_mut("nodes").and_then(Value::as_array_mut) {
        if remove_nodes {
            nodes.retain(|n| !n["id"].as_str().is_some_and(|s| remap.contains_key(s)));
        } else {
            for node in nodes {
                if let Some(new) = node["id"].as_str().and_then(|s| remap.get(s)) {
                    node["id"] = json!(new);
                }
            }
        }
    }
    if let Some(edges) = extraction.get_mut("edges").and_then(Value::as_array_mut) {
        edges.retain_mut(|e| {
            let mut changed = false;
            for field in ["source", "target"] {
                if let Some(new) = e[field].as_str().and_then(|s| remap.get(s)) {
                    e[field] = json!(new);
                    changed = true;
                }
            }
            !(remove_nodes && changed && e["source"] == e["target"])
        });
    }
    for he in extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for member in he
            .get_mut("nodes")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(new) = member.as_str().and_then(|s| remap.get(s)) {
                *member = json!(new);
            }
        }
    }
}
/// Whether a new edge with the same relation as `old` should be dropped:
/// lower confidence rank, or equal rank while losing a source location or a
/// lower confidence score.
fn same_relation_is_weaker(
    old: &dyn crate::record::Properties,
    new: &dyn crate::record::Properties,
) -> bool {
    let rank = |attributes: &dyn crate::record::Properties| match attributes
        .get("confidence")
        .and_then(Value::as_str)
    {
        Some("EXTRACTED") => 3,
        Some("INFERRED") => 2,
        Some("AMBIGUOUS") => 1,
        _ => 0,
    };
    let old_rank = rank(old);
    let new_rank = rank(new);
    old_rank > new_rank
        || (old_rank == new_rank
            && (old.get("source_location").is_some_and(truthy)
                && !new.get("source_location").is_some_and(truthy)
                || old
                    .get("confidence_score")
                    .and_then(Value::as_f64)
                    .zip(new.get("confidence_score").and_then(Value::as_f64))
                    .is_some_and(|(old, new)| old > new)))
}
pub fn build_from_json(
    extraction: &mut Value,
    directed: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
) -> Result<Graph> {
    build_from_json_with(extraction, directed, false, root, messages)
}
/// `build_from_json` with the multigraph choice explicit: when `multigraph`
/// is set, edges are keyed by relation so that distinct relations between the
/// same pair survive; same-relation duplicates still fold by confidence.
pub fn build_from_json_with(
    extraction: &mut Value,
    directed: bool,
    multigraph: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
) -> Result<Graph> {
    build_from_json_dedup(extraction, directed, multigraph, true, root, messages)
}
/// `build_from_json(..., dedup=False)` (0.9.77): with `dedup` off, distinct
/// non-AST IDs that merely share a file and label are kept apart.
pub fn build_from_json_dedup(
    extraction: &mut Value,
    directed: bool,
    multigraph: bool,
    dedup: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
) -> Result<Graph> {
    build_from_json_core(
        extraction, directed, multigraph, root, messages, false, dedup,
    )
}
/// Shared body of [`build_from_json_dedup`] and [`build_owned`]. With
/// `consume`, the node and edge records are moved out of `extraction` into
/// the graph instead of cloned; callers that read the extraction afterwards
/// pass `false` and see it exactly as before. `dedup: false` (0.9.77) keeps
/// distinct non-AST IDs that merely share a file and label; AST twins and
/// document-file twins still reconcile.
fn build_from_json_core(
    extraction: &mut Value,
    directed: bool,
    multigraph: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
    consume: bool,
    dedup: bool,
) -> Result<Graph> {
    if !extraction.is_object() {
        return Err("Expected extraction object".into());
    }
    let root = root
        .filter(|p| !p.as_os_str().is_empty())
        .map(fsutil::resolve_missing)
        .transpose()
        .map_err(crate::Error::from)?;
    if extraction.get("edges").is_none()
        && let Some(links) = extraction.get("links").cloned()
    {
        extraction["edges"] = links;
    }
    if extraction.get("hyperedges").is_none()
        && let Some(hyper) = extraction["graph"]["hyperedges"].as_array()
    {
        extraction["hyperedges"] = json!(hyper);
    }
    let profiled = crate::profile::span("build.core.prepare");
    coerce_non_string_ids(extraction);
    // Only the rare `source`-instead-of-`source_file` warning needs the edge
    // list here; take it out for the duration of the node pass rather than
    // cloning every edge record up front.
    let edges = extraction
        .get_mut("edges")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    let node_pass = (|| -> Result<()> {
        for node in extraction
            .get_mut("nodes")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(n) = node.as_object_mut() else {
                continue;
            };
            if n.contains_key("source") && !n.contains_key("source_file") {
                let nid = n.get("id").cloned().unwrap_or(json!("?"));
                let affected = edges
                    .iter()
                    .filter(|e| e["source"] == nid || e["target"] == nid)
                    .count();
                messages.stderr.push(format!("[graphicle] WARNING: node '{}' uses field 'source' instead of 'source_file' — {affected} edge(s) may be misrouted. Rename the field to 'source_file' to silence this warning.",python_compat::string(&nid)));
                let source = n.shift_remove("source").unwrap();
                n.insert("source_file".into(), source);
            }
            fold_node_aliases(node);
            node["file_type"] = normalized_file_type(&node["file_type"])?;
        }
        Ok(())
    })();
    if let Some(slot) = extraction.get_mut("edges").and_then(Value::as_array_mut) {
        *slot = edges;
    }
    node_pass?;
    for he in extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        messages.stderr.extend(hyperedge_members::normalize(he));
    }
    for edge in extraction
        .get_mut("edges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        fold_edge_aliases(edge);
    }
    drop(profiled);
    let profiled = crate::profile::span("build.core.validate");
    // Dangling endpoints are expected here (they become external stubs or
    // are dropped below), so their diagnostics are not built at all.
    let errors = validate::validate_extraction_with(extraction, false)?;
    drop(profiled);
    let errors: Vec<_> = errors
        .into_iter()
        .filter(|e| !e.contains("does not match any node id"))
        .collect();
    if !errors.is_empty() {
        let mut causes: Vec<(String, Vec<&str>)> = Vec::new();
        let mut indices = HashMap::new();
        for error in &errors {
            let cause = error
                .find("missing required field '")
                .map(|i| {
                    let tail = &error[i..];
                    let end = tail[24..]
                        .find('\'')
                        .map(|j| 24 + j + 1)
                        .unwrap_or(tail.len());
                    tail[..end].to_owned()
                })
                .unwrap_or_else(|| "other schema issue".into());
            let next = causes.len();
            let i = *indices.entry(cause.clone()).or_insert_with(|| {
                causes.push((cause, Vec::new()));
                next
            });
            causes[i].1.push(error);
        }
        let breakdown = causes
            .iter()
            .map(|(cause, errs)| format!("{}x {cause} (e.g. {})", errs.len(), errs[0]))
            .collect::<Vec<_>>()
            .join("; ");
        messages.stderr.push(format!(
            "[graphicle] Extraction warning ({} issues): {breakdown}",
            errors.len()
        ));
    }
    let profiled = crate::profile::span("build.core.remap");
    let rekey = semantic_id_remap(
        extraction["nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        root.as_deref(),
    );
    remap_extraction(extraction, &rekey, false);
    let twins = doc_twin_remap(
        extraction["nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    remap_extraction(extraction, &twins, true);
    drop(profiled);
    let profiled = crate::profile::span("build.core.add_nodes");
    let mut graph = Graph::new(directed, multigraph);
    for node in extraction
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let n = node.as_object_mut().ok_or("Node must be an object")?;
        let Some(nid) = map_field(n, "id").cloned() else {
            continue;
        };
        if scalar_key(&nid).is_none() {
            messages.stderr.push(format!(
                "[graphicle] WARNING: skipping node with non-hashable id {} (must be a string).",
                python_compat::repr(&nid)
            ));
            continue;
        }
        for key in ["source_file", "definition_file"] {
            if let Some(value) = map_field_mut(n, key) {
                *value = norm_source_file(value, root.as_deref())?;
            }
        }
        let mut attrs = if consume {
            std::mem::take(n)
        } else {
            n.clone()
        };
        attrs.shift_remove("id");
        graph.add_node(nid, attrs)?;
    }
    drop(profiled);
    let profiled = crate::profile::span("build.core.ghosts_aliases");
    // Node indices are stable until `remove_nodes_from` below, so the
    // sorted pass can address attributes directly instead of re-hashing ids.
    let mut sorted_nodes = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            n.id.as_str()
                .map(|id| (id.to_owned(), i))
                .ok_or("Graph node IDs must be strings")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    sorted_nodes.sort_by(|a, b| a.0.cmp(&b.0));
    // `located`, `collisions`, `ast_methods`, `norm_to_id` and `aliases` are
    // only looked up (or visited where each key is handled once), so hash
    // tables give the same result as ordered maps without string comparisons;
    // `ghosts` stays ordered because its visiting order decides overrides.
    let mut located = FxHashMap::<(String, String), String>::default();
    let mut collisions = FxHashSet::default();
    let mut ghosts_by_key = BTreeMap::<(String, String), String>::new();
    let mut ast_files = Vec::new();
    // #3705: (source_file, normalized method name) -> AST `.name()` nodes.
    let mut ast_methods = FxHashMap::<(String, String), Vec<String>>::default();
    let method_name = |label: &str| {
        let stem = label.strip_prefix('.').unwrap_or(label);
        ids::make_id([stem.strip_suffix("()").unwrap_or(stem)])
    };
    let mut method_ghosts = BTreeMap::<String, String>::new();
    for (nid, index) in &sorted_nodes {
        let attributes = &graph.nodes[*index].attributes;
        let label = value_string(attributes, "label")
            .trim_matches(dedup_strings::whitespace)
            .to_owned();
        let sf = value_string(attributes, "source_file");
        if label.is_empty() || sf.is_empty() {
            continue;
        }
        let ast = attributes.get("_origin").is_some_and(|v| v == "ast");
        if attributes.get("source_location").is_some_and(truthy) || ast {
            let key = (sf.clone(), label.clone());
            if ast {
                if located
                    .get(&key)
                    .and_then(|id| graph.node_index(&json!(id)))
                    .is_some_and(|i| {
                        graph.nodes[i]
                            .attributes
                            .get("_origin")
                            .is_some_and(|v| v == "ast")
                    })
                {
                    collisions.insert(key.clone());
                }
                located.insert(key, nid.clone());
                if label.starts_with('.') && label.ends_with("()") {
                    ast_methods
                        .entry((sf.clone(), method_name(&label)))
                        .or_default()
                        .push(nid.clone());
                }
                if crate::build_labels::is_file_node_label_str(&label, &sf) {
                    ast_files.push((nid.clone(), sf));
                }
            } else {
                located.entry(key).or_insert_with(|| nid.clone());
            }
        }
    }
    for (nid, index) in &sorted_nodes {
        let attributes = &graph.nodes[*index].attributes;
        if attributes.get("_origin").is_some_and(|v| v == "ast") {
            continue;
        }
        let label = value_string(attributes, "label")
            .trim_matches(dedup_strings::whitespace)
            .to_owned();
        let sf = value_string(attributes, "source_file");
        if label.is_empty() || sf.is_empty() {
            continue;
        }
        let key = (sf, label);
        if collisions.contains(&key) {
            continue;
        }
        if let Some(canonical) = located.get(&key).filter(|n| *n != nid) {
            if !dedup
                && graph.node_index(&json!(canonical)).is_none_or(|i| {
                    graph.nodes[i]
                        .attributes
                        .get("_origin")
                        .is_none_or(|v| v != "ast")
                })
            {
                continue;
            }
            ghosts_by_key.insert(key, nid.clone());
        } else if !located.contains_key(&key) {
            // #3705: spec-conformant method ghost that omits the class
            // segment / leading dot; remap only on one unambiguous candidate.
            let candidates = ast_methods.get(&(key.0.clone(), method_name(&key.1)));
            if let Some([canonical]) = candidates.map(Vec::as_slice) {
                method_ghosts.insert(nid.clone(), canonical.clone());
            }
        }
    }
    let mut ghosts = method_ghosts;
    for (key, sem) in ghosts_by_key {
        if let Some(ast) = located.get(&key) {
            ghosts.insert(sem, ast.clone());
        }
    }
    for (nid, index) in &sorted_nodes {
        if ghosts.contains_key(nid) {
            continue;
        }
        let attributes = &graph.nodes[*index].attributes;
        if attributes.get("_origin").is_some_and(|v| v == "ast") {
            continue;
        }
        let label = value_string(attributes, "label")
            .trim_matches(dedup_strings::whitespace)
            .to_owned();
        if label.is_empty() {
            continue;
        }
        let matches: BTreeSet<_> = ast_files
            .iter()
            .filter(|(_, sf)| crate::build_labels::is_file_node_label_str(&label, sf))
            .map(|(n, _)| n.clone())
            .collect();
        if matches.len() == 1 {
            ghosts.insert(nid.clone(), matches.into_iter().next().unwrap());
        }
    }
    graph.remove_nodes_from(&ghosts.keys().map(|s| json!(s)).collect::<Vec<_>>())?;
    let mut norm_to_id = FxHashMap::<String, String>::default();
    // Stable order resolves the reference's hash-order ambiguity when two IDs
    // have the same normalized spelling; no insertion is allowed to mint a node.
    for (nid, _) in &sorted_nodes {
        if graph.node_index(&json!(nid)).is_some() {
            norm_to_id.insert(ids::normalize_id(nid), nid.clone());
        }
    }
    for (ghost, canonical) in &ghosts {
        norm_to_id.insert(ids::normalize_id(ghost), canonical.clone());
        norm_to_id.insert(ghost.clone(), canonical.clone());
    }
    let mut aliases = FxHashMap::<String, BTreeSet<String>>::default();
    // A file's stem id, name and former stems, derived once per file rather
    // than for each of its nodes.
    let mut files = HashMap::<String, (String, String, Vec<String>, bool)>::new();
    for node in &graph.nodes {
        let Some(sf) = node.attributes.get("source_file").filter(|v| truthy(v)) else {
            continue;
        };
        let sf = python_compat::string(sf);
        if crate::paths::is_absolute_any_platform(Some(&sf)) {
            continue;
        }
        let (stem, file_name, olds, py_stdlib_stem) = files.entry(sf).or_insert_with_key(|sf| {
            let rel = Path::new(sf);
            let name = rel
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            // A Python module whose bare stem is a stdlib name (`scripts/logging.py`)
            // never claims the bare-stem alias ("logging"): it would capture a
            // genuine external `import logging` edge (0.9.84, #4261). The
            // directory-scoped alias ("scripts_logging") is still registered.
            let py_stdlib_stem =
                matches!(crate::detect_types::suffix(rel).as_str(), ".py" | ".pyi")
                    && is_python_stdlib_module(&ids::make_id([crate::detect_types::stem(&name)]));
            (
                ids::make_id([base::file_stem(rel)]),
                name,
                old_file_stems(rel),
                py_stdlib_stem,
            )
        });
        let py_stdlib_stem = *py_stdlib_stem;
        let stem = &*stem;
        let nid = node.id.as_str().unwrap();
        let normalized_id = ids::normalize_id(nid);
        let suffix = if value_string(&node.attributes, "label") == *file_name {
            ""
        } else {
            normalized_id.strip_prefix(stem).unwrap_or("")
        };
        for old in olds.iter() {
            if old == stem || (py_stdlib_stem && is_python_stdlib_module(old)) {
                continue;
            }
            let alias = format!("{old}{suffix}");
            aliases
                .entry(ids::normalize_id(&alias))
                .or_default()
                .insert(nid.into());
            aliases.entry(alias).or_default().insert(nid.into());
        }
        if node.attributes.get("_origin").is_some_and(|v| v == "ast") {
            let label = value_string(&node.attributes, "label");
            if label.starts_with('.') {
                // #3705: `<file_stem>_<method>` alias for AST `.name()` nodes.
                let m_alias = format!(
                    "{stem}_{}",
                    method_name(label.trim_matches(dedup_strings::whitespace))
                );
                if normalized_id != ids::normalize_id(&m_alias) {
                    aliases
                        .entry(ids::normalize_id(&m_alias))
                        .or_default()
                        .insert(nid.into());
                    aliases.entry(m_alias).or_default().insert(nid.into());
                }
            }
        }
    }
    for (alias, candidates) in aliases {
        if candidates.len() == 1 {
            norm_to_id
                .entry(alias)
                .or_insert_with(|| candidates.into_iter().next().unwrap());
        }
    }
    drop(profiled);
    let profiled = crate::profile::span("build.core.edges");
    let mut edges = if consume {
        extraction
            .get_mut("edges")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap_or_default()
    } else {
        extraction["edges"].as_array().cloned().unwrap_or_default()
    };
    // Stable like `sort_by_key`, but the three-string key is built once per
    // edge instead of on every comparison.
    edges.sort_by_cached_key(|e| {
        [
            field(e, "source")
                .or_else(|| field(e, "from"))
                .map(python_compat::string)
                .unwrap_or_default(),
            field(e, "target")
                .or_else(|| field(e, "to"))
                .map(python_compat::string)
                .unwrap_or_default(),
            field(e, "relation")
                .map(python_compat::string)
                .unwrap_or_default(),
        ]
    });
    for mut edge in edges {
        let object = edge.as_object_mut().ok_or("Edge must be an object")?;
        for (key, alias) in [("source", "from"), ("target", "to")] {
            if map_field(object, key).is_none()
                && let Some(value) = map_field(object, alias).cloned()
            {
                object.insert(key.into(), value);
            }
        }
        let (Some(mut src), Some(mut tgt)) = (
            map_field(object, "source").cloned(),
            map_field(object, "target").cloned(),
        ) else {
            continue;
        };
        if scalar_key(&src).is_none() || scalar_key(&tgt).is_none() {
            messages.stderr.push(format!("[graphicle] WARNING: skipping edge with non-hashable endpoint (source={}, target={}).",python_compat::repr(&src),python_compat::repr(&tgt)));
            continue;
        }
        let mut u = graph.node_index(&src);
        let mut v = graph.node_index(&tgt);
        for (endpoint, index) in [(&mut src, &mut u), (&mut tgt, &mut v)] {
            if index.is_none() {
                let id = endpoint.as_str().ok_or("Expected string edge endpoint")?;
                if let Some(new) = norm_to_id.get(&ids::normalize_id(id)) {
                    *endpoint = json!(new);
                    *index = graph.node_index(endpoint);
                }
            }
        }
        let relation = map_field(object, "relation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let relation = relation.as_str();
        if u.is_none() || v.is_none() {
            if EXTERNAL_STUB_RELATIONS.contains(&relation)
                && u.is_some()
                && v.is_none()
                && tgt.is_string()
            {
                mint_external_stub(&mut graph, tgt.as_str().unwrap())?;
                v = graph.node_index(&tgt);
            } else {
                continue;
            }
        }
        let (Some(u), Some(v)) = (u, v) else {
            continue;
        };
        // `edge` is owned by this iteration: move its attributes instead of
        // copying every record.
        let mut attrs = std::mem::take(object);
        for key in ["source", "target", "target_file", "local_alias"] {
            // The last two are rare: skip hashing the key when absent.
            if map_field(&attrs, key).is_some() {
                attrs.shift_remove(key);
            }
        }
        for key in ["weight", "confidence_score"] {
            if let Some(value) = map_field_mut(&mut attrs, key) {
                *value = json!(numeric_field(value));
            }
        }
        if !map_field(&attrs, "source_file").is_some_and(truthy) {
            let sf = graph.nodes[u]
                .attributes
                .get("source_file")
                .filter(|v| truthy(v))
                .or_else(|| {
                    graph.nodes[v]
                        .attributes
                        .get("source_file")
                        .filter(|v| truthy(v))
                })
                .cloned()
                .unwrap_or(json!(""));
            attrs.insert("source_file".into(), sf);
        }
        if let Some(sf) = map_field_mut(&mut attrs, "source_file") {
            *sf = norm_source_file(sf, root.as_deref())?;
        }
        if let Some(sf) = map_field_mut(&mut attrs, "definition_file").filter(|v| truthy(v)) {
            *sf = norm_source_file(sf, root.as_deref())?;
        }
        if ["calls", "imports", "imports_from", "references"].contains(&relation) {
            let ext = |i: usize| {
                let path = graph.nodes[i]
                    .attributes
                    .get("source_file")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                crate::detect_types::suffix(Path::new(path))
            };
            let a = ext(u);
            let b = ext(v);
            let (fa, fb) = (family(&a), family(&b));
            if relation == "calls" {
                if map_field(&attrs, "confidence").is_some_and(|v| v == "INFERRED")
                    && !a.is_empty()
                    && !b.is_empty()
                    && fa != fb
                {
                    continue;
                }
            } else if fa.is_some() && fb.is_some() && fa != fb {
                continue;
            }
        }
        if src == tgt && EXTERNAL_STUB_RELATIONS.contains(&relation) {
            continue;
        }
        attrs.insert("_src".into(), src.clone());
        attrs.insert("_tgt".into(), tgt.clone());
        // Before the drops below, so a runtime import that folds into a
        // type-only edge still clears its marker (0.9.80, #3159). The reverse
        // duplicate an undirected graph drops (#1061) is another file's import:
        // the surviving edge keeps its own marker.
        if EXTERNAL_STUB_RELATIONS.contains(&relation) {
            let key = multigraph.then(|| json!(relation));
            if let Some(prior) = graph.edge_attributes_mut(&src, &tgt, key.as_ref()) {
                let reverse_duplicate = !directed
                    && prior.get("relation") == attrs.get("relation")
                    && prior.get("_src") == Some(&tgt)
                    && prior.get("_tgt") == Some(&src);
                if prior
                    .get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|r| EXTERNAL_STUB_RELATIONS.contains(&r))
                    && !reverse_duplicate
                {
                    fold_type_only(prior, &mut attrs);
                }
            }
        }
        if multigraph {
            let is_generic = |edge: &&crate::graph::Edge| {
                edge.attributes
                    .get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|r| GENERIC_RELATIONS.contains(&r))
            };
            let key = json!(relation);
            let parallel = graph.edges_between(&src, &tgt);
            // A generic relation adds nothing beside a specific one, whichever
            // arrives first; same-relation duplicates fold as in the parity path.
            if GENERIC_RELATIONS.contains(&relation) && parallel.iter().any(|e| !is_generic(e)) {
                continue;
            }
            if let Some(existing) = parallel.iter().find(|e| e.key == key) {
                let old = &existing.attributes;
                if !directed && old.get("_src") == Some(&tgt) && old.get("_tgt") == Some(&src) {
                    continue;
                }
                if same_relation_is_weaker(old, &attrs) {
                    continue;
                }
            }
            let superseded: Vec<Value> = if GENERIC_RELATIONS.contains(&relation) {
                Vec::new()
            } else {
                parallel
                    .iter()
                    .filter(|e| is_generic(e))
                    .map(|e| e.key.clone())
                    .collect()
            };
            for generic in &superseded {
                graph.remove_edge(&src, &tgt, Some(generic))?;
            }
            graph.add_edge(src, tgt, Some(key), attrs)?;
            continue;
        }
        if let Some(existing) = graph.edges_between(&src, &tgt).first() {
            let old = &existing.attributes;
            let reverse = old.get("_src") == Some(&tgt) && old.get("_tgt") == Some(&src);
            if !directed && reverse && old.get("relation") == attrs.get("relation") {
                continue;
            }
            // Different relations in opposite directions only share a pair
            // because the graph is undirected: a type's `method`/`contains`
            // edge beats its member's `calls` edge back (0.9.83, #4256). If
            // the call is stored first, the membership edge replaces it below.
            if !directed
                && reverse
                && old
                    .get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|r| STRUCTURAL_MEMBER_RELATIONS.contains(&r))
                && relation == "calls"
            {
                continue;
            }
            if GENERIC_RELATIONS.contains(&relation)
                && old.get("relation").is_some_and(|v| !v.is_null())
                && !old
                    .get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|s| GENERIC_RELATIONS.contains(&s))
            {
                continue;
            }
            if old.get("relation") == attrs.get("relation") && same_relation_is_weaker(old, &attrs)
            {
                continue;
            }
        }
        graph.add_edge(src, tgt, None, attrs)?;
    }
    drop(profiled);
    if let Some(hyperedges) = extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .filter(|h| !h.is_empty())
    {
        let mut kept = Vec::new();
        for he in hyperedges.iter_mut() {
            if let Some(sf) = he.get_mut("source_file").filter(|v| truthy(v)) {
                *sf = norm_source_file(sf, root.as_deref())?;
            }
            if let Some(members) = he["nodes"].as_array() {
                let mut valid = Vec::new();
                for raw in members {
                    if scalar_key(raw).is_none() {
                        continue;
                    }
                    let mut member = raw.clone();
                    if graph.node_index(&member).is_none()
                        && let Some(s) = member.as_str()
                        && let Some(id) = norm_to_id.get(&ids::normalize_id(s))
                    {
                        member = json!(id);
                    }
                    if graph.node_index(&member).is_some() {
                        valid.push(member);
                    }
                }
                if valid.is_empty() {
                    messages.stderr.push(format!("[graphicle] WARNING: dropping hyperedge {} — none of its members {} match built nodes.",python_compat::repr(he.get("id").unwrap_or(&json!("?"))),python_compat::repr(&he["nodes"])));
                    continue;
                }
                he["nodes"] = json!(valid);
            }
            kept.push(he.clone());
        }
        if kept.is_empty() {
            messages.stderr.push(format!("[graphicle] WARNING: all {} hyperedge(s) were dropped by member revalidation; graph.json's hyperedge set will be emptied on the next export.",hyperedges.len()));
        }
        graph.attributes["hyperedges"] = json!(kept);
    }
    let profiled = crate::profile::span("build.core.file_labels");
    disambiguate_file_node_labels(&mut graph);
    drop(profiled);
    Ok(graph)
}
pub fn build(
    extractions: &[Value],
    options: &BuildOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    build_owned(extractions.to_vec(), options, messages, client)
}
/// [`build`] over extractions the caller no longer needs: node and edge
/// records are moved into the graph rather than copied, which halves the peak
/// footprint of a large build. Output is identical to [`build`].
pub fn build_owned(
    extractions: Vec<Value>,
    options: &BuildOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut hyperedges = Vec::new();
    // Receiver-resolution leftovers (`ExtractOptions::receiver_resolution`)
    // ride along as a graph attribute so reports can list them; the parity
    // extraction never carries the key.
    let mut unresolved_calls: Vec<Value> = Vec::new();
    for extraction in extractions {
        let Value::Object(mut extraction) = extraction else {
            continue;
        };
        for (key, target) in [
            ("nodes", &mut nodes),
            ("edges", &mut edges),
            ("hyperedges", &mut hyperedges),
        ] {
            if let Some(values) = extraction.shift_remove(key) {
                let Value::Array(values) = values else {
                    return Err(format!("Expected {key} array").into());
                };
                if target.is_empty() {
                    *target = values;
                } else {
                    target.extend(values);
                }
            }
        }
        if let Some(Value::Array(values)) = extraction.shift_remove("unresolved_calls") {
            unresolved_calls.extend(values);
        }
    }
    let mut combined = Map::new();
    combined.insert("nodes".into(), Value::Array(nodes));
    combined.insert("edges".into(), Value::Array(edges));
    combined.insert("hyperedges".into(), Value::Array(hyperedges));
    let mut combined = Value::Object(combined);
    let root = options
        .root
        .as_ref()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| fsutil::resolve_missing(p))
        .transpose()
        .map_err(crate::Error::from)?;
    let mut dedup_collapsed = 0;
    if options.dedup && !combined["nodes"].as_array().unwrap().is_empty() {
        let profiled = crate::profile::span("build.dedup_prepare");
        coerce_non_string_ids(&mut combined);
        for node in combined["nodes"].as_array_mut().unwrap() {
            fold_node_aliases(node);
            if node.is_object() {
                for key in ["source_file", "definition_file"] {
                    if let Some(sf) = node.get_mut(key) {
                        *sf = norm_source_file(sf, root.as_deref())?;
                    }
                }
            }
        }
        let take = |combined: &mut Value, key: &str| match combined[key].take() {
            Value::Array(values) => values,
            _ => vec![],
        };
        let mut hyperedges = take(&mut combined, "hyperedges");
        let nodes = take(&mut combined, "nodes");
        let edges = take(&mut combined, "edges");
        let before_dedup = nodes.len();
        drop(profiled);
        let profiled = crate::profile::span("build.dedup");
        let result = dedup::deduplicate_entities_owned(
            nodes,
            edges,
            &DedupOptions {
                communities: Map::new(),
                dedup_llm_backend: options.dedup_llm_backend.clone(),
                root: root.clone(),
                protected_ids: options.protected_ids.iter().cloned().collect(),
            },
            Some(&mut hyperedges),
            client,
        )?;
        dedup_collapsed = before_dedup.saturating_sub(result.nodes.len());
        combined["nodes"] = Value::Array(result.nodes);
        combined["edges"] = Value::Array(result.edges);
        combined["hyperedges"] = Value::Array(hyperedges);
        messages.stdout.extend(result.stdout);
        messages.stderr.extend(result.stderr);
        drop(profiled);
    }
    let _profiled = crate::profile::span("build.core");
    let mut graph = build_from_json_core(
        &mut combined,
        options.directed,
        options.multigraph,
        root.as_deref(),
        messages,
        true,
        options.dedup,
    )?;
    // The extract CLI reads this to tell a dedup shrink from a file deletion
    // (#3774); `to_json` never stores it in graph.json.
    if dedup_collapsed > 0 {
        graph.attributes[DEDUP_COLLAPSED] = json!(dedup_collapsed);
    }
    if !unresolved_calls.is_empty() {
        graph.attributes["unresolved_calls"] = json!(unresolved_calls);
    }
    Ok(graph)
}
/// Graph attribute: nodes this build's dedup merged away (Graphify #3774).
pub const DEDUP_COLLAPSED: &str = "_dedup_collapsed";
/// Graph attribute: saved nodes a `build_merge` prune removed (Graphify #3774).
pub const PRUNED_NODE_COUNT: &str = "_pruned_node_count";
/// Read and remove the #3774 shrink counters `(dedup collapsed, pruned)` that
/// [`build`] and [`build_merge`] stash on the graph for the extract CLI.
pub fn take_shrink_accounting(graph: &mut Graph) -> (u64, u64) {
    let Some(attributes) = graph.attributes.as_object_mut() else {
        return (0, 0);
    };
    let mut take = |key: &str| {
        attributes
            .shift_remove(key)
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    (take(DEDUP_COLLAPSED), take(PRUNED_NODE_COUNT))
}
