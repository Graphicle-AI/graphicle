// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/diagnostics.py
//! Read-only graph-integrity and producer-suppression diagnostics.
use crate::{Result, build, fsutil, python_compat, python_json, security};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::LazyLock,
};
include!(concat!(env!("OUT_DIR"), "/producer_sources.rs"));

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => json!(a.iter().map(sorted).collect::<Vec<_>>()),
        v => v.clone(),
    }
}
fn serialized(value: &Value, compact: bool) -> String {
    let mut bytes = Vec::new();
    python_json::write(&mut bytes, &sorted(value), None, false).expect("serialize JSON value");
    let text = String::from_utf8(bytes).unwrap();
    if !compact {
        return text;
    }
    let mut output = String::new();
    let (mut quoted, mut escape) = (false, false);
    for ch in text.chars() {
        if quoted {
            output.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            output.push(ch);
        } else if !ch.is_ascii_whitespace() {
            output.push(ch);
        }
    }
    output
}
pub fn safe_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Object(_) | Value::Array(_) => serialized(value, false),
        _ => python_compat::string(value),
    }
}
fn edge_list(extraction: &Value) -> &[Value] {
    extraction
        .get("edges")
        .filter(|v| !v.is_null())
        .or_else(|| extraction.get("links"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn node_ids(extraction: &Value) -> BTreeSet<String> {
    extraction["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_object()?.get("id").filter(|v| !v.is_null()))
        .map(python_compat::string)
        .collect()
}
pub fn canonical_edge(edge: &Value) -> Value {
    let mut result = Map::new();
    for key in [
        "source",
        "target",
        "relation",
        "confidence",
        "source_file",
        "source_location",
        "context",
    ] {
        let value = edge
            .get(key)
            .or_else(|| match key {
                "source" => edge.get("from"),
                "target" => edge.get("to"),
                _ => None,
            })
            .unwrap_or(&Value::Null);
        result.insert(key.into(), json!(safe_text(value)));
    }
    result.insert(
        "_invalid".into(),
        json!(if edge.is_object() {
            ""
        } else {
            "non_object_edge"
        }),
    );
    Value::Object(result)
}
pub fn exact_signature(edge: &Value) -> String {
    let Some(mut edge) = edge.as_object().cloned() else {
        return "<non-object>".into();
    };
    for (key, old) in [("source", "from"), ("target", "to")] {
        if !edge.contains_key(key)
            && let Some(v) = edge.get(old).cloned()
        {
            edge.insert(key.into(), v);
        }
        edge.shift_remove(old);
    }
    serialized(&Value::Object(edge), true)
}
pub fn tuple_arity(line: &str, rust: bool) -> usize {
    static PY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"set\[tuple\[([^\]]+)\]\]").unwrap());
    static RS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:HashSet|BTreeSet)\s*<\s*\(([^)]*)\)\s*>").unwrap());
    let Some(m) = (if rust { &RS } else { &PY }).captures(line) else {
        return 0;
    };
    let inside = m[1].trim_matches(crate::dedup_strings::whitespace);
    if inside.is_empty() {
        0
    } else {
        inside.chars().filter(|c| *c == ',').count() + 1
    }
}
fn scan_text(text: &str, rust: bool) -> Vec<Value> {
    static PY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^{}*(?P<name>seen_[A-Za-z0-9_]+)\s*[:=]",
            fsutil::PY_WHITESPACE
        ))
        .unwrap()
    });
    static RS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:let\s+(?:mut\s+)?)?(?P<name>seen_[A-Za-z0-9_]+)\s*[:=]").unwrap()
    });
    fsutil::split_lines(text).enumerate().filter_map(|(i, line)| {
        let m = (if rust { &RS } else { &PY }).captures(line)?;
        Some(json!({"line":i+1,"name":&m["name"],"tuple_arity":tuple_arity(line,rust),
            "sample":line.trim_matches(crate::dedup_strings::whitespace).chars().take(120).collect::<String>()}))
    }).collect()
}
pub fn scan_producer_suppression_sites(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(
            json!({"path":fsutil::path_string(path),"total_sites":0,"sites":[],"error":"file not found"}),
        );
    }
    let text = std::fs::read_to_string(path).map_err(crate::Error::from)?;
    let sites = scan_text(&text, path.extension().is_some_and(|e| e == "rs"));
    Ok(json!({"path":fsutil::path_string(path),"total_sites":sites.len(),"sites":sites,"error":""}))
}
pub fn compiled_producer_suppression_sites() -> Value {
    let mut sites = Vec::new();
    for (file, source) in PRODUCER_SOURCES {
        for mut site in scan_text(source, true) {
            site["source_file"] = json!(file);
            sites.push(site);
        }
    }
    json!({"path":"native:compiled-producers","total_sites":sites.len(),"sites":sites,"error":""})
}
type Pair = (String, String);
fn variant_groups(groups: &[(Pair, Vec<Value>)], field: &str, relation_sensitive: bool) -> usize {
    groups
        .iter()
        .map(|(_, edges)| {
            let mut values: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            for edge in edges {
                let relation = if relation_sensitive {
                    edge["relation"].as_str().unwrap()
                } else {
                    ""
                };
                values
                    .entry(relation)
                    .or_default()
                    .insert(edge[field].as_str().unwrap());
            }
            values.values().filter(|v| v.len() > 1).count()
        })
        .sum()
}
fn error_category(error: String) -> String {
    if error.starts_with("TypeError:")
        || error.starts_with("ValueError:")
        || error.starts_with("OSError:")
    {
        error
    } else if error.ends_with("must be an object") {
        format!("TypeError: {error}")
    } else {
        format!("ValueError: {error}")
    }
}
#[derive(Clone, Debug)]
pub struct Options {
    pub directed: bool,
    pub root: Option<PathBuf>,
    pub max_examples: i64,
    pub extract_path: Option<PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            directed: true,
            root: None,
            max_examples: 5,
            extract_path: None,
        }
    }
}
pub fn diagnose_extraction(
    extraction: &Value,
    options: &Options,
    messages: &mut build::Diagnostics,
) -> Result<Value> {
    if !extraction.is_object() {
        return Err("ValueError: diagnostic input must be a JSON object".into());
    }
    let ids = node_ids(extraction);
    let edges = edge_list(extraction);
    let mut exact = HashMap::<String, usize>::new();
    let mut undirected = HashMap::<Pair, usize>::new();
    let mut indices = HashMap::<Pair, usize>::new();
    let mut groups = Vec::<(Pair, Vec<Value>)>::new();
    let (mut non_object, mut missing, mut dangling, mut external, mut self_loop, mut valid) =
        (0, 0, 0, 0, 0, 0);
    for edge in edges {
        *exact.entry(exact_signature(edge)).or_default() += 1;
        let edge = canonical_edge(edge);
        if edge["_invalid"] != "" {
            non_object += 1;
            continue;
        }
        let source = edge["source"].as_str().unwrap();
        let target = edge["target"].as_str().unwrap();
        if source.is_empty() || target.is_empty() {
            missing += 1;
            continue;
        }
        if !ids.contains(source) || !ids.contains(target) {
            if ids.contains(source) && target.starts_with("ref_") {
                external += 1;
            } else {
                dangling += 1;
            }
            continue;
        }
        if source == target {
            self_loop += 1;
        }
        valid += 1;
        let pair = (source.to_owned(), target.to_owned());
        let unordered = if source <= target {
            pair.clone()
        } else {
            (target.to_owned(), source.to_owned())
        };
        *undirected.entry(unordered).or_default() += 1;
        let next = groups.len();
        let index = *indices.entry(pair.clone()).or_insert_with(|| {
            groups.push((pair, Vec::new()));
            next
        });
        groups[index].1.push(edge);
    }
    let mut counts: Vec<_> = groups.iter().collect();
    counts.sort_by_key(|(_, edges)| std::cmp::Reverse(edges.len()));
    let examples: Vec<Value> = counts
        .iter()
        .filter(|(_, edges)| edges.len() >= 2)
        .take(options.max_examples.max(0) as usize)
        .map(|((s, t), edges)| {
            let mut example = json!({"source":s,"target":t,"edge_count":edges.len()});
            for (key, field) in [
                ("relations", "relation"),
                ("source_files", "source_file"),
                ("source_locations", "source_location"),
                ("contexts", "context"),
            ] {
                let set: BTreeSet<_> = edges.iter().map(|e| e[field].as_str().unwrap()).collect();
                example[key] = json!(set);
            }
            example
        })
        .collect();
    let mut cloned = extraction.clone();
    let (graph_type, node_count, edge_count, error) = match build::build_from_json(
        &mut cloned,
        options.directed,
        options.root.as_deref(),
        messages,
    ) {
        Ok(graph) => (
            if graph.directed { "DiGraph" } else { "Graph" },
            json!(graph.nodes.len()),
            json!(graph.edge_count()),
            String::new(),
        ),
        Err(error) => (
            "",
            Value::Null,
            Value::Null,
            error_category(error.to_string()),
        ),
    };
    let suppression = match options
        .extract_path
        .as_deref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        Some(path) => scan_producer_suppression_sites(path)?,
        None => compiled_producer_suppression_sites(),
    };
    Ok(json!({
        "node_count":ids.len(),
        "unverified_node_count":extraction["nodes"].as_array().into_iter().flatten().filter(|n| n.get("verification").is_some_and(|v| v == "unverified")).count(),
        "raw_edge_count":edges.len(),"non_object_edges":non_object,"missing_endpoint_edges":missing,
        "dangling_endpoint_edges":dangling,"external_reference_edges":external,"self_loop_edges":self_loop,"valid_candidate_edges":valid,
        "exact_duplicate_edges":exact.values().map(|n| n.saturating_sub(1)).sum::<usize>(),
        "directed_unique_endpoint_pairs":groups.len(),
        "directed_same_endpoint_collapsed_edges":groups.iter().map(|(_,e)| e.len().saturating_sub(1)).sum::<usize>(),
        "undirected_unique_endpoint_pairs":undirected.len(),
        "undirected_same_endpoint_collapsed_edges":undirected.values().map(|n| n.saturating_sub(1)).sum::<usize>(),
        "same_endpoint_group_count":groups.iter().filter(|(_,e)| e.len()>1).count(),
        "relation_variant_groups":variant_groups(&groups,"relation",false),
        "source_file_variant_groups":variant_groups(&groups,"source_file",true),
        "source_location_variant_groups":variant_groups(&groups,"source_location",true),
        "context_variant_groups":variant_groups(&groups,"context",true),
        "post_build_graph_type":graph_type,"post_build_node_count":node_count,"post_build_edge_count":edge_count,"post_build_error":error,
        "producer_suppression":suppression,"examples":examples
    }))
}
pub fn read_json_file(path: &Path, max_bytes: Option<u64>) -> Result<Value> {
    security::check_graph_file_size_cap_with(
        path,
        max_bytes.unwrap_or_else(security::max_graph_file_bytes),
    )?;
    let data: Value = std::fs::read_to_string(path).map_err(crate::Error::from)
        .and_then(|s| serde_json::from_str(&s).map_err(crate::Error::from))
        .map_err(|e| format!("RuntimeError: Cannot parse {}: {e}. The file may be corrupted — re-run '{} extract'.", path.display(), crate::cli::program()))?;
    if !data.is_object() {
        return Err("ValueError: diagnostic input must be a JSON object".into());
    }
    Ok(data)
}
pub fn diagnose_file(
    path: &Path,
    directed: Option<bool>,
    options: &Options,
    max_bytes: Option<u64>,
    messages: &mut build::Diagnostics,
) -> Result<Value> {
    let data = read_json_file(path, max_bytes)?;
    let mut effective = options.clone();
    effective.directed = directed.unwrap_or_else(|| data["directed"].as_bool().unwrap_or(true));
    let mut result = diagnose_extraction(&data, &effective, messages)?;
    result["input_path"] = json!(fsutil::path_string(path));
    result["effective_directed"] = json!(effective.directed);
    Ok(result)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let mut messages = build::Diagnostics::default();
    let path = || {
        r["path"]
            .as_str()
            .map(Path::new)
            .ok_or_else(|| "Expected path".to_string())
    };
    let options = Options {
        directed: r["directed"].as_bool().unwrap_or(true),
        root: r["root"].as_str().map(PathBuf::from),
        max_examples: r["max_examples"].as_i64().unwrap_or(5),
        extract_path: r["extract_path"].as_str().map(PathBuf::from),
    };
    let value = match r["function"].as_str().unwrap_or("diagnose_extraction") {
        "_safe_text" => json!(safe_text(&r["value"])),
        "_canonical_edge" => canonical_edge(&r["edge"]),
        "_exact_signature" => json!(exact_signature(&r["edge"])),
        "_node_ids" => json!(node_ids(&r["extraction"])),
        "_edge_list" => json!(edge_list(&r["extraction"])),
        "_tuple_arity_from_annotation" => {
            json!(tuple_arity(r["line"].as_str().unwrap_or(""), false))
        }
        "scan_producer_suppression_sites" => scan_producer_suppression_sites(path()?)?,
        "_read_json_file" => read_json_file(path()?, r["max_graph_bytes"].as_u64())?,
        "diagnose_extraction" => diagnose_extraction(&r["extraction"], &options, &mut messages)?,
        "diagnose_file" => diagnose_file(
            path()?,
            r["directed"].as_bool(),
            &options,
            r["max_graph_bytes"].as_u64(),
            &mut messages,
        )?,
        "format_diagnostic_json" => crate::diagnostics_report::format_json(&r["summary"]),
        "format_diagnostic_report" => {
            json!(crate::diagnostics_report::format_report(&r["summary"])?)
        }
        _ => return Err("Unknown diagnostic function".into()),
    };
    Ok(json!({"value":value,"stdout":messages.stdout,"stderr":messages.stderr}))
}
