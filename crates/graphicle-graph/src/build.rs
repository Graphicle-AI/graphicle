// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
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
        resolution::{is_python_stdlib_module, map_field, map_field_mut},
    },
    fsutil,
    graph::Graph,
    hyperedge_members, ids,
    python_compat::{self, scalar_key},
    record::{Attributes, EdgeItem, Properties, Record, item_field},
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
fn fold_type_only<R: Record>(prior: &mut Attributes, incoming: &mut R) {
    if prior.get("type_only").is_some_and(truthy) != incoming.get("type_only").is_some_and(truthy) {
        prior.shift_remove("type_only");
        incoming.take_key("type_only");
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
fn remap_extraction<R: Record, E: EdgeItem<Record = R>>(
    extraction: &mut Value,
    edges: &mut Vec<E>,
    remap: &BTreeMap<String, String>,
    remove_nodes: bool,
) {
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
    edges.retain_mut(|e| {
        let Some(e) = e.record_mut() else {
            return true;
        };
        let mut changed = false;
        for field in ["source", "target"] {
            if let Some(new) = e
                .get(field)
                .and_then(Value::as_str)
                .and_then(|s| remap.get(s))
            {
                e.put(field, json!(new));
                changed = true;
            }
        }
        let null = &Value::Null;
        !(remove_nodes
            && changed
            && e.get("source").unwrap_or(null) == e.get("target").unwrap_or(null))
    });
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
/// [`build_from_json`] of an extraction the caller no longer reads: its node
/// and edge records move into the graph instead of being copied, so the
/// graph is built without a second copy of them (the rebuild in `watch`).
/// The graph is the same; the extraction is left without its records.
pub fn build_from_json_consuming(
    extraction: &mut Value,
    directed: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
) -> Result<Graph> {
    build_from_json_core(extraction, directed, false, root, messages, true, true)
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
    let root = resolve_root(root)?;
    if extraction.get("edges").is_none()
        && let Some(links) = extraction.get("links").cloned()
    {
        extraction["edges"] = links;
    }
    // The passes hold the edge list beside the rest of the extraction
    // ([`build_core`]); it goes back afterwards, emptied when consumed.
    let mut edges = match extraction.get_mut("edges") {
        Some(Value::Array(edges)) => std::mem::take(edges),
        _ => Vec::new(),
    };
    let graph = build_core(
        extraction,
        &mut edges,
        directed,
        multigraph,
        root.as_deref(),
        messages,
        consume,
        dedup,
    );
    if let Some(Value::Array(slot)) = extraction.get_mut("edges") {
        *slot = edges;
    }
    graph
}
/// A build's root, resolved; `None` for an absent or empty one.
fn resolve_root(root: Option<&Path>) -> Result<Option<PathBuf>> {
    root.filter(|p| !p.as_os_str().is_empty())
        .map(fsutil::resolve_missing)
        .transpose()
        .map_err(crate::Error::from)
}
/// The build passes over an extraction whose edge list is held beside it:
/// JSON values, or the compact records an extract assembled. Every pass reads
/// and edits an edge record exactly as it would the same edge as a JSON
/// object, so both forms build the same graph; records move into the graph
/// with no JSON copy. `extraction` holds the nodes and hyperedges, and an
/// empty `edges` array when its edges are in `edges`.
#[allow(clippy::too_many_arguments)]
fn build_core<R: Record, E: EdgeItem<Record = R>>(
    extraction: &mut Value,
    edges: &mut Vec<E>,
    directed: bool,
    multigraph: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
    consume: bool,
    dedup: bool,
) -> Result<Graph> {
    if extraction.get("hyperedges").is_none()
        && let Some(hyper) = extraction["graph"]["hyperedges"].as_array()
    {
        extraction["hyperedges"] = json!(hyper);
    }
    let profiled = crate::profile::span("build.core.prepare");
    coerce_non_string_ids(extraction);
    for edge in edges.iter_mut() {
        if let Some(e) = edge.record_mut() {
            coerce_edge_record_ids(e);
        }
    }
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
                .filter(|e| *item_field(*e, "source") == nid || *item_field(*e, "target") == nid)
                .count();
            messages.stderr.push(format!("[graphicle] WARNING: node '{}' uses field 'source' instead of 'source_file' — {affected} edge(s) may be misrouted. Rename the field to 'source_file' to silence this warning.",python_compat::string(&nid)));
            let source = n.shift_remove("source").unwrap();
            n.insert("source_file".into(), source);
        }
        fold_node_aliases(node);
        node["file_type"] = normalized_file_type(&node["file_type"])?;
    }
    for he in extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        messages.stderr.extend(hyperedge_members::normalize(he));
    }
    for edge in edges.iter_mut() {
        if let Some(e) = edge.record_mut() {
            fold_edge_record_aliases(e);
        }
    }
    drop(profiled);
    let profiled = crate::profile::span("build.core.validate");
    // Dangling endpoints are expected here (they become external stubs or
    // are dropped below), so their diagnostics are not built at all.
    let errors = validate::validate_extraction_beside(extraction, edges, false)?;
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
        root,
    );
    remap_extraction(extraction, edges, &rekey, false);
    let twins = doc_twin_remap(
        extraction["nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    );
    remap_extraction(extraction, edges, &twins, true);
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
                *value = norm_source_file(value, root)?;
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
    let norm_to_id = ghosts_and_aliases(&mut graph, dedup)?;
    drop(profiled);
    let profiled = crate::profile::span("build.core.edges");
    let edges = if consume {
        std::mem::take(edges)
    } else {
        edges.clone()
    };
    add_edges(
        &mut graph,
        edges,
        &norm_to_id,
        directed,
        multigraph,
        root,
        messages,
    )?;
    drop(profiled);
    if let Some(hyperedges) = extraction
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
        .filter(|h| !h.is_empty())
    {
        let mut kept = Vec::new();
        for he in hyperedges.iter_mut() {
            if let Some(sf) = he.get_mut("source_file").filter(|v| truthy(v)) {
                *sf = norm_source_file(sf, root)?;
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
/// Fold semantic ghosts into the AST nodes they shadow (removing them from
/// `graph`), and the table edge endpoints resolve through: normalized ids,
/// ghost ids and file-stem aliases, each to its canonical node id.
fn ghosts_and_aliases(graph: &mut Graph, dedup: bool) -> Result<FxHashMap<String, String>> {
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
    Ok(norm_to_id)
}
/// The fields an edge is ordered by: source, target and relation, an
/// endpoint read from its alias (`from`, `to`) when absent.
const ORDER_FIELDS: [(&str, Option<&str>); 3] = [
    ("source", Some("from")),
    ("target", Some("to")),
    ("relation", None),
];
/// An edge's order texts as [`python_compat::string`] gives them, borrowed
/// from the edge (`""` for an absent field, and for an item that is no
/// record); `None` when one is no string, whose text is not stored.
fn order_texts<E: EdgeItem>(edge: &E) -> Option<[&str; 3]> {
    let Some(record) = edge.record() else {
        return Some([""; 3]);
    };
    let mut texts = [""; 3];
    for (text, (key, alias)) in texts.iter_mut().zip(ORDER_FIELDS) {
        match record
            .get(key)
            .or_else(|| alias.and_then(|a| record.get(a)))
        {
            None => {}
            Some(Value::String(s)) => *text = s.as_str(),
            Some(_) => return None,
        }
    }
    Some(texts)
}
/// The indices of `edges` in the order the build adds them: stable by the
/// source, target and relation texts (`sorted(edges, key=...)`). Each key is
/// built once and borrows the edge's own strings; ties keep the list order.
fn edge_order<E: EdgeItem>(edges: &[E]) -> Vec<usize> {
    let mut keyed = Vec::with_capacity(edges.len());
    for (i, edge) in edges.iter().enumerate() {
        let Some(texts) = order_texts(edge) else {
            break;
        };
        keyed.push((texts, i));
    }
    if keyed.len() == edges.len() {
        keyed.sort_unstable();
        return indices(&keyed);
    }
    drop(keyed);
    // Some text is no string: its Python spelling, built for every edge.
    let mut keyed = edges
        .iter()
        .enumerate()
        .map(|(i, edge)| {
            let texts = ORDER_FIELDS.map(|(key, alias)| {
                edge.record()
                    .and_then(|r| r.get(key).or_else(|| alias.and_then(|a| r.get(a))))
                    .map(python_compat::string)
                    .unwrap_or_default()
            });
            (texts, i)
        })
        .collect::<Vec<_>>();
    keyed.sort_unstable();
    indices(&keyed)
}
/// The indices of sorted `(key, index)` pairs, in a list of their own size
/// (collecting the pairs in place would keep the pairs' allocation).
fn indices<K>(keyed: &[(K, usize)]) -> Vec<usize> {
    let mut order = Vec::with_capacity(keyed.len());
    order.extend(keyed.iter().map(|(_, i)| *i));
    order
}
/// Add `edges` in the build's order ([`edge_order`]) under the endpoint,
/// provenance and duplicate rules.
fn add_edges<R: Record, E: EdgeItem<Record = R>>(
    graph: &mut Graph,
    mut edges: Vec<E>,
    norm_to_id: &FxHashMap<String, String>,
    directed: bool,
    multigraph: bool,
    root: Option<&Path>,
    messages: &mut Diagnostics,
) -> Result<()> {
    let order = edge_order(&edges);
    // Each record adds at most one edge slot, so the edge list is allocated
    // once instead of doubling (each doubling copies every slot while the
    // old and the new list are both live).
    graph.reserve_edges(edges.len());
    for i in order {
        // Each edge is taken out of the list once: its attributes move into
        // the graph instead of being copied.
        let edge = std::mem::take(&mut edges[i]);
        let mut attrs = edge.into_record().ok_or("Edge must be an object")?;
        for (key, alias) in [("source", "from"), ("target", "to")] {
            if attrs.get(key).is_none()
                && let Some(value) = attrs.get(alias).cloned()
            {
                attrs.put(key, value);
            }
        }
        let (Some(mut src), Some(mut tgt)) =
            (attrs.get("source").cloned(), attrs.get("target").cloned())
        else {
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
        let relation = attrs
            .get("relation")
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
                mint_external_stub(graph, tgt.as_str().unwrap())?;
                v = graph.node_index(&tgt);
            } else {
                continue;
            }
        }
        let (Some(u), Some(v)) = (u, v) else {
            continue;
        };
        for key in ["source", "target", "target_file", "local_alias"] {
            // The last two are rare: skip hashing the key when absent.
            if attrs.get(key).is_some() {
                attrs.take_key(key);
            }
        }
        for key in ["weight", "confidence_score"] {
            if let Some(value) = attrs.get(key) {
                let value = json!(numeric_field(value));
                attrs.put(key, value);
            }
        }
        if !attrs.get("source_file").is_some_and(truthy) {
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
            attrs.put("source_file", sf);
        }
        if let Some(sf) = attrs.get("source_file") {
            let sf = norm_source_file(sf, root)?;
            attrs.put("source_file", sf);
        }
        if let Some(sf) = attrs.get("definition_file").filter(|v| truthy(v)) {
            let sf = norm_source_file(sf, root)?;
            attrs.put("definition_file", sf);
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
                if attrs.get("confidence").is_some_and(|v| v == "INFERRED")
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
        attrs.put("_src", src.clone());
        attrs.put("_tgt", tgt.clone());
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
                    .map(|e| e.key.to_value())
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
    Ok(())
}
pub fn build(
    extractions: &[Value],
    options: &BuildOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    build_owned(extractions.to_vec(), options, messages, client)
}
/// The lists of the extractions a build combines, in order, apart from the
/// edges ([`combine`]).
struct Combined {
    nodes: Vec<Value>,
    hyperedges: Vec<Value>,
    /// Receiver-resolution leftovers (`ExtractOptions::receiver_resolution`)
    /// ride along as a graph attribute so reports can list them; the parity
    /// extraction never carries the key.
    unresolved_calls: Vec<Value>,
}
/// The extractions' lists combined in order, and their edges beside them.
fn combine(extractions: Vec<Value>) -> Result<(Combined, Vec<Value>)> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut hyperedges = Vec::new();
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
    Ok((
        Combined {
            nodes,
            hyperedges,
            unresolved_calls,
        },
        edges,
    ))
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
    let (combined, edges) = combine(extractions)?;
    build_combined(combined, edges, options, messages, client)
}
/// [`build_owned`] of one extraction whose edges start with `records`, the
/// compact edge records an extract assembled, held beside it: the graph is
/// the one [`build_owned`] builds with those records as JSON objects in front
/// of the extraction's own edges, but they move into it with no JSON copy,
/// and the extraction's own edges convert to records too.
pub(crate) fn build_owned_records(
    extraction: Value,
    records: Vec<Attributes>,
    options: &BuildOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    let (combined, json_edges) = combine(vec![extraction])?;
    if !json_edges.iter().all(Value::is_object) {
        // Only the JSON form holds an edge that is not an object.
        let mut edges = Vec::with_capacity(records.len() + json_edges.len());
        edges.extend(records.into_iter().map(Value::from));
        edges.extend(json_edges);
        return build_combined(combined, edges, options, messages, client);
    }
    let mut edges = records;
    edges.reserve_exact(json_edges.len());
    edges.extend(
        json_edges
            .into_iter()
            .filter_map(EdgeItem::into_record)
            .map(Attributes::from),
    );
    build_combined(combined, edges, options, messages, client)
}
/// The body of [`build_owned`] over combined lists, with the edges in either
/// form ([`build_core`]).
fn build_combined<R: Record, E: EdgeItem<Record = R>>(
    combined: Combined,
    mut edges: Vec<E>,
    options: &BuildOptions,
    messages: &mut Diagnostics,
    client: Option<&mut dyn TiebreakClient>,
) -> Result<Graph> {
    let Combined {
        nodes,
        hyperedges,
        unresolved_calls,
    } = combined;
    let mut combined = Map::new();
    combined.insert("nodes".into(), Value::Array(nodes));
    // The edges are held beside the rest (`build_core`).
    combined.insert("edges".into(), Value::Array(Vec::new()));
    combined.insert("hyperedges".into(), Value::Array(hyperedges));
    let mut combined = Value::Object(combined);
    let root = resolve_root(options.root.as_deref())?;
    let mut dedup_collapsed = 0;
    if options.dedup && !combined["nodes"].as_array().unwrap().is_empty() {
        let profiled = crate::profile::span("build.dedup_prepare");
        coerce_non_string_ids(&mut combined);
        for edge in edges.iter_mut() {
            if let Some(e) = edge.record_mut() {
                coerce_edge_record_ids(e);
            }
        }
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
        let before_dedup = nodes.len();
        drop(profiled);
        let profiled = crate::profile::span("build.dedup");
        let (result, deduped) = dedup::deduplicate_records(
            nodes,
            std::mem::take(&mut edges),
            &DedupOptions {
                communities: Map::new(),
                dedup_llm_backend: options.dedup_llm_backend.clone(),
                root: root.clone(),
                protected_ids: options.protected_ids.iter().cloned().collect(),
            },
            Some(&mut hyperedges),
            client,
        )?;
        edges = deduped;
        dedup_collapsed = before_dedup.saturating_sub(result.nodes.len());
        combined["nodes"] = Value::Array(result.nodes);
        combined["hyperedges"] = Value::Array(hyperedges);
        messages.stdout.extend(result.stdout);
        messages.stderr.extend(result.stderr);
        drop(profiled);
    }
    let _profiled = crate::profile::span("build.core");
    // The core resolves the root again, as `build_from_json` does.
    let root = resolve_root(root.as_deref())?;
    let mut graph = build_core(
        &mut combined,
        &mut edges,
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{self, ExtractOptions};

    /// A build's graph (node-link form) and messages, or its error.
    fn outcome(result: Result<Graph>, messages: &Diagnostics) -> String {
        let graph = match result {
            Ok(graph) => serde_json::to_string(&graph.to_node_link("links")).unwrap(),
            Err(e) => format!("error: {e}"),
        };
        format!("{graph}\n{:?}\n{:?}", messages.stdout, messages.stderr)
    }

    /// Semantic-tier nodes, edges and a hyperedge beside the AST ones, which
    /// reach every build pass that reads or edits an edge: endpoint and
    /// relation aliases, numeric ids and confidence, a document twin, a dedup
    /// merge, an extension collision and a path collision (with a reference
    /// that cannot be attributed), a schema warning, a field named `source`
    /// and an external stub. `code` is an AST node id they point at.
    fn semantic(code: &str) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let doc = |id: &str, label: &str, file: &str| json!({"id": id, "label": label, "file_type": "document", "source_file": file});
        let refs = |source: &str, target: &str, file: &str| {
            json!({"source": source, "target": target, "relation": "references",
                   "confidence": "EXTRACTED", "source_file": file})
        };
        let nodes = vec![
            doc("guide", "Guide", "docs/guide.md"),
            doc("guide_doc", "Guide doc", "docs/guide.md"),
            json!({"id": "payment_gateway", "label": "Payment Gateway Service",
                   "file_type": "concept", "source_file": "docs/notes.md"}),
            json!({"id": "payment_gateway_2", "label": "payment gateway service",
                   "file_type": "concept", "source_file": "docs/notes.md", "description": "dup"}),
            doc("x_overview", "Overview", "a/x.md"),
            doc("x_overview", "Overview", "b/x.md"),
            doc("site_page_intro", "Intro", "site/page.md"),
            doc("site_page_intro", "Intro", "site/page.html"),
            json!({"id": 42, "label": "Answer", "file_type": "concept", "source_file": "docs/notes.md"}),
            json!({"id": "legacy", "label": "Legacy", "file_type": "concept", "source": "docs/legacy.md"}),
        ];
        let edges = vec![
            refs("guide_doc", "guide", "docs/guide.md"),
            json!({"source": "payment_gateway", "target": "payment_gateway_2",
                   "relation": "semantically_similar_to", "confidence": "INFERRED",
                   "confidence_score": 0.9, "source_file": "docs/notes.md"}),
            json!({"from": "payment_gateway_2", "to": code, "type": "mentions",
                   "confidence": 0.6, "source_file": "docs/notes.md", "weight": "2"}),
            refs("x_overview", "guide", "a/x.md"),
            refs("x_overview", "guide", "b/x.md"),
            refs("guide", "x_overview", "docs/guide.md"),
            refs("site_page_intro", "guide", "site/page.html"),
            json!({"source": 42, "target": "guide", "relation": "references",
                   "confidence": "EXTRACTED", "source_file": "docs/notes.md"}),
            json!({"source": "legacy", "target": "guide", "relation": "references",
                   "source_file": "docs/legacy.md"}),
            json!({"source": "guide", "target": "external_mod", "relation": "imports",
                   "confidence": "EXTRACTED", "source_file": "docs/guide.md"}),
            json!({"source": code, "target": "guide", "relation": "references",
                   "confidence": "INFERRED", "source_file": ""}),
        ];
        let hyperedges = vec![
            json!({"id": "flow", "label": "Flow", "relation": "participate_in",
            "confidence": "INFERRED", "source_file": "docs/guide.md",
            "nodes": ["guide", "x_overview", "payment_gateway_2", 42]}),
        ];
        (nodes, edges, hyperedges)
    }

    fn with(extraction: &Value, key: &str, items: &[Value]) -> Value {
        let mut out = extraction.clone();
        let mut all = out[key].as_array().cloned().unwrap_or_default();
        all.extend_from_slice(items);
        out[key] = Value::Array(all);
        out
    }

    /// The JSON build of `json` against the record build of `beside` with
    /// `records` in front of its edges, under several build options.
    fn assert_same_builds(json: &Value, beside: &Value, records: &[Attributes], root: &Path) {
        for (dedup, directed, multigraph) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let options = BuildOptions {
                dedup,
                directed,
                multigraph,
                root: Some(root.to_path_buf()),
                ..Default::default()
            };
            let mut old = Diagnostics::default();
            let want = build_owned(vec![json.clone()], &options, &mut old, None);
            let mut new = Diagnostics::default();
            let got =
                build_owned_records(beside.clone(), records.to_vec(), &options, &mut new, None);
            assert_eq!(
                outcome(got, &new),
                outcome(want, &old),
                "dedup {dedup}, directed {directed}, multigraph {multigraph}"
            );
        }
    }

    /// The extract command builds the edge records assembly leaves (and the
    /// JSON edges after them) into exactly the graph, messages and errors
    /// that the JSON build of the same edges gives.
    #[test]
    fn assembled_records_build_exactly_like_json_edges() {
        let _serial = crate::extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write("go.mod", "module example.com/demo\n\ngo 1.22\n");
        let mut native = vec![
            write(
                "a/a.go",
                "package a\n\nfunc Alpha() int { return 1 }\n\ntype Thing struct{}\n\nfunc (t *Thing) Do() int { return Alpha() }\n",
            ),
            write(
                "a/a2.go",
                "package a\n\nfunc Beta() int { return Alpha() }\n",
            ),
            write(
                "b/b.go",
                "package b\n\nimport (\n\t\"fmt\"\n\t\"example.com/demo/a\"\n)\n\nfunc Bravo() { fmt.Println(a.Alpha()); Charlie() }\n\nfunc Charlie() {}\n",
            ),
            write(
                "py/models.py",
                "class Base:\n    def run(self):\n        return 1\n\n\nclass Child(Base):\n    def go(self):\n        return self.run()\n",
            ),
            write(
                "py/app.py",
                "import models\nfrom models import Child\n\n\ndef main():\n    c = Child()\n    models.Base().run()\n    return c.go()\n",
            ),
            write("sh/run.sh", "#!/bin/bash\nsource ./lib.sh\nhelper\n"),
            write("sh/lib.sh", "helper() {\n  echo hi\n}\n"),
            write(
                "c/main.c",
                "int util(void);\nint main(void) { return util(); }\n",
            ),
            write("c/util.c", "int util(void) { return 1; }\n"),
        ];
        native.sort();
        let mut all = native.clone();
        all.extend([
            write(
                "ts/a.ts",
                "export class Greeter {\n  greet(): string { return this.name(); }\n  name(): string { return 'x'; }\n}\n",
            ),
            write(
                "ts/b.ts",
                "import { Greeter } from './a';\n\nexport function run(): string {\n  const g = new Greeter();\n  return g.greet();\n}\n",
            ),
            write(
                "src/lib.rs",
                "pub struct S;\n\nimpl S {\n    pub fn f(&self) -> usize { self.g() }\n    fn g(&self) -> usize { 1 }\n}\n",
            ),
            write(
                "java/A.java",
                "package demo;\n\npublic class A {\n  public int f() { return g(); }\n  int g() { return 1; }\n}\n",
            ),
            write(
                "java/B.java",
                "package demo;\n\nimport demo.A;\n\npublic class B {\n  public int h() { return new A().f(); }\n}\n",
            ),
            write("rb/a.rb", "class Foo\n  def bar\n    baz\n  end\n\n  def baz\n    1\n  end\nend\n"),
            write(
                "php/a.php",
                "<?php\nclass P {\n  public function f() { return $this->g(); }\n  public function g() { return 1; }\n}\n",
            ),
            write(
                "kt/A.kt",
                "package demo\n\nclass K {\n  fun f(): Int = g()\n  fun g(): Int = 1\n}\n",
            ),
            write(
                "cs/A.cs",
                "namespace Demo {\n  public class C {\n    public int F() { return G(); }\n    int G() { return 1; }\n  }\n}\n",
            ),
            write(
                "swift/A.swift",
                "class W {\n  func f() -> Int { return g() }\n  func g() -> Int { return 1 }\n}\n",
            ),
            write(
                "cpp/a.cpp",
                "#include \"a.hpp\"\n\nint Q::f() { return g(); }\nint Q::g() { return 1; }\n",
            ),
            write("cpp/a.hpp", "class Q {\npublic:\n  int f();\n  int g();\n};\n"),
        ]);
        all.sort();
        let options = ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("cache")),
            use_cache: false,
            parallel: false,
            ..Default::default()
        };
        let code_node = |extraction: &Value| {
            extraction["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| {
                    n["source_file"]
                        .as_str()
                        .is_some_and(|f| f.ends_with(".go"))
                })
                .and_then(|n| n["id"].as_str())
                .unwrap()
                .to_owned()
        };

        // Go, Python, Bash and C keep the record form through assembly; the
        // other languages' JSON resolvers leave JSON edges, which convert.
        for paths in [&native, &all] {
            let json = extract::extract(paths, &options).unwrap();
            assert_eq!(
                serde_json::to_string(
                    &extract::extract_assembled(paths, &options)
                        .unwrap()
                        .into_value()
                )
                .unwrap(),
                serde_json::to_string(&json).unwrap()
            );
            let (beside, records) = extract::extract_assembled(paths, &options)
                .unwrap()
                .into_parts();
            assert!(!records.is_empty());
            assert_eq!(beside["edges"], json!([]));
            let (nodes, edges, hyperedges) = semantic(&code_node(&json));
            let plain = |e: &Value| with(&with(e, "nodes", &nodes), "hyperedges", &hyperedges);
            assert_same_builds(&json, &beside, &records, &root);
            assert_same_builds(
                &with(&plain(&json), "edges", &edges),
                &with(&plain(&beside), "edges", &edges),
                &records,
                &root,
            );
            // A JSON edge that is no object fails both builds the same way.
            let broken = [edges.clone(), vec![json!("not an edge")]].concat();
            assert_same_builds(
                &with(&plain(&json), "edges", &broken),
                &with(&plain(&beside), "edges", &broken),
                &records,
                &root,
            );
        }
    }
}
