// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/symbol_resolution.py
//! Port of graphify/symbol_resolution.py with native Python syntax parsing.
use crate::{
    Result, extractors::base::file_stem, fsutil, ids::make_id, path_syntax, paths, python_compat,
    security,
};
use rustpython_parser::{Parse, ast};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedSymbol {
    pub local_name: String,
    pub imported_name: String,
    pub module_stem: String,
    pub source_file: String,
    pub source_location: String,
}
type Labels = BTreeMap<String, Vec<String>>;
pub type PythonSymbolIndex = BTreeMap<(String, String), Vec<String>>;
fn text(node: &Value, field: &str) -> String {
    node.get(field)
        .map(python_compat::string)
        .unwrap_or_default()
}
fn strip(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}
fn get(node: &Value, field: &str, default: Value) -> Value {
    node.get(field).cloned().unwrap_or(default)
}
pub fn normalise_callable_label(label: &str) -> String {
    strip(label)
        .trim_matches(['(', ')'])
        .trim_start_matches('.')
        .to_lowercase()
}
pub fn node_is_resolvable_symbol(node: &Value) -> bool {
    if node["file_type"].as_str() != Some("code") {
        return false;
    }
    let label = text(node, "label");
    let label = strip(&label);
    !label.is_empty()
        && ![".py", ".js", ".ts", ".tsx", ".java", ".go", ".rs"]
            .iter()
            .any(|ext| label.ends_with(ext))
        && !normalise_callable_label(label).is_empty()
}
/// `build_label_index`: label -> node ids for conservative cross-file
/// resolution. With `edges`, an enum member (the target of a `case_of` edge)
/// is never indexed: it is not a call target, and its name can collide with
/// an unrelated bare name (#4245).
pub fn build_label_index(nodes: &[Value], edges: Option<&[Value]>) -> Labels {
    let case_of = edges
        .into_iter()
        .flatten()
        .filter(|e| e.is_object() && e["relation"] == "case_of")
        .filter_map(|e| e.get("target").and_then(python_compat::scalar_key))
        .collect::<std::collections::BTreeSet<_>>();
    let mut index = Labels::new();
    for node in nodes {
        if node_is_resolvable_symbol(node)
            && truthy(&node["id"])
            && python_compat::scalar_key(&node["id"]).is_none_or(|id| !case_of.contains(&id))
        {
            let label = normalise_callable_label(&text(node, "label"));
            if !label.is_empty() {
                index.entry(label).or_default().push(text(node, "id"));
            }
        }
    }
    index
}
pub fn existing_edge_pairs(edges: &[Value]) -> HashSet<(String, String, String)> {
    edges
        .iter()
        .filter(|edge| truthy(&edge["source"]) && truthy(&edge["target"]))
        .map(|edge| {
            (
                text(edge, "source"),
                text(edge, "target"),
                text(edge, "relation"),
            )
        })
        .collect()
}
pub fn iter_raw_calls(per_file: &[Value]) -> Vec<&Value> {
    per_file
        .iter()
        .filter_map(|result| result.get("raw_calls").and_then(Value::as_array))
        .flatten()
        .filter(|call| call.is_object())
        .collect()
}
pub fn module_stem(module: Option<&str>) -> String {
    module
        .unwrap_or("")
        .trim_matches('.')
        .rsplit('.')
        .next()
        .unwrap_or("")
        .into()
}
pub fn parse_python_import_aliases(path: &Path) -> BTreeMap<String, ImportedSymbol> {
    let mut aliases = BTreeMap::new();
    // `_read_source_text(path, warn=False)` (0.9.78): a file that is not UTF-8
    // is decoded as its extractor read it, not with replacement characters.
    let Ok(source) = crate::extractors::base::read_source_text(path, false) else {
        return aliases;
    };
    // A UTF-8 BOM is kept as text, then ast.parse rejects it. RustPython
    // accepts it as an encoding marker, so enforce the source contract.
    if source.starts_with('\u{feff}') {
        return aliases;
    }
    let source_file = fsutil::path_string(path);
    let Ok(tree) = ast::Suite::parse(&source, &source_file) else {
        return aliases;
    };
    for node in tree {
        let ast::Stmt::ImportFrom(import) = node else {
            continue;
        };
        let stem = module_stem(import.module.as_ref().map(|m| m.as_str()))
            .nfkc()
            .collect::<String>();
        if stem.is_empty() {
            continue;
        }
        let offset = u32::from(import.range.start()) as usize;
        let line = source.as_bytes()[..offset.min(source.len())]
            .iter()
            .filter(|&&c| c == b'\n')
            .count()
            + 1;
        for alias in import.names {
            let imported_name: String = alias.name.as_str().nfkc().collect();
            if imported_name == "*" {
                continue;
            }
            let local_name = alias.asname.as_ref().map_or_else(
                || imported_name.clone(),
                |name| name.as_str().nfkc().collect(),
            );
            aliases.insert(
                local_name.clone(),
                ImportedSymbol {
                    local_name,
                    imported_name,
                    module_stem: stem.clone(),
                    source_file: source_file.clone(),
                    source_location: format!("L{line}"),
                },
            );
        }
    }
    aliases
}
pub fn node_source_stem(node: &Value) -> String {
    let source = text(node, "source_file");
    if source.is_empty() {
        return String::new();
    }
    let parts = path_syntax::posix_parts(&source);
    let name = path_syntax::posix_name(&parts);
    let suffix = path_syntax::suffix(name);
    name[..name.len() - suffix.len()].into()
}
pub fn build_python_symbol_index(nodes: &[Value]) -> PythonSymbolIndex {
    let mut index = PythonSymbolIndex::new();
    for node in nodes {
        if !node_is_resolvable_symbol(node) || !truthy(&node["id"]) {
            continue;
        }
        let stem = node_source_stem(node);
        let label = normalise_callable_label(&text(node, "label"));
        if !stem.is_empty() && !label.is_empty() {
            index
                .entry((stem, label))
                .or_default()
                .push(text(node, "id"));
        }
    }
    index
}
pub fn find_unique_python_symbol(
    index: &PythonSymbolIndex,
    imported: &ImportedSymbol,
) -> Option<String> {
    let candidates = index.get(&(
        imported.module_stem.clone(),
        imported.imported_name.to_lowercase(),
    ))?;
    (candidates.len() == 1).then(|| candidates[0].clone())
}
/// A Python file by its effective suffix (`.graphiclerc` declarations, #2961).
fn is_py(path: &Path) -> bool {
    crate::rcfile::effective_suffix(path) == ".py"
}
pub fn resolve_python_import_guided_calls(
    per_file: &[Value],
    paths: &[PathBuf],
    nodes: &[Value],
    edges: &[Value],
) -> Vec<Value> {
    resolve_python_import_guided_calls_with(
        per_file,
        paths,
        nodes,
        edges,
        parse_python_import_aliases,
    )
}
pub fn resolve_python_import_guided_calls_with(
    per_file: &[Value],
    paths: &[PathBuf],
    nodes: &[Value],
    edges: &[Value],
    aliases_for: impl Fn(&Path) -> BTreeMap<String, ImportedSymbol>,
) -> Vec<Value> {
    let index = build_python_symbol_index(nodes);
    let mut known = existing_edge_pairs(edges);
    let mut by_file = HashMap::new();
    for (i, path) in paths.iter().enumerate().filter(|(_, path)| is_py(path)) {
        by_file.insert(
            fsutil::path_string(path),
            per_file.get(i).unwrap_or(&Value::Null),
        );
    }
    let mut resolved = Vec::new();
    for path in paths.iter().filter(|path| is_py(path)) {
        let file = fsutil::path_string(path);
        let aliases = aliases_for(path);
        if aliases.is_empty() {
            continue;
        }
        for call in by_file
            .get(&file)
            .and_then(|value| value.get("raw_calls"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            if truthy(&call["is_member_call"]) {
                continue;
            }
            let callee = text(call, "callee");
            let callee = strip(&callee);
            if callee.is_empty() {
                continue;
            }
            let Some(imported) = aliases.get(callee) else {
                continue;
            };
            let Some(target) = find_unique_python_symbol(&index, imported) else {
                continue;
            };
            let caller = text(call, "caller_nid");
            if caller.is_empty()
                || caller == target
                || !known.insert((caller.clone(), target.clone(), "calls".into()))
            {
                continue;
            }
            let metadata = security::sanitize_metadata(Some(
                &json!({"resolver":"python_import_guided","local_name":imported.local_name,"imported_name":imported.imported_name,"module_stem":imported.module_stem,"import_source_location":imported.source_location}),
            ));
            resolved.push(json!({"source":caller,"target":target,"relation":"calls","context":"import_guided_call","confidence":"EXTRACTED","confidence_score":1.0,
                "source_file":get(call,"source_file",json!(file)),"source_location":if truthy(&call["source_location"]){call["source_location"].clone()}else{json!(imported.source_location)},"weight":1.0,"metadata":metadata}));
        }
    }
    resolved
}
pub fn resolve_cross_file_raw_calls(
    per_file: &[Value],
    nodes: &[Value],
    edges: &[Value],
) -> Vec<Value> {
    let labels = build_label_index(nodes, Some(edges));
    let mut known = existing_edge_pairs(edges);
    let files: Map<String, Value> = nodes
        .iter()
        .filter(|n| truthy(&n["id"]))
        .map(|n| (text(n, "id"), json!(text(n, "source_file"))))
        .collect();
    let mut resolved = Vec::new();
    for call in iter_raw_calls(per_file) {
        let callee = text(call, "callee");
        let callee = strip(&callee);
        if callee.is_empty() || truthy(&call["is_member_call"]) {
            continue;
        }
        let Some(candidates) = labels.get(&callee.to_lowercase()) else {
            continue;
        };
        let target = if candidates.len() == 1 {
            Some(candidates[0].clone())
        } else {
            let candidates_files = candidates
                .iter()
                .map(|id| (id.clone(), files.get(id).cloned().unwrap_or(json!(""))))
                .collect();
            paths::disambiguate_ambiguous_candidates(
                candidates,
                &candidates_files,
                &text(call, "source_file"),
            )
        };
        let Some(target) = target else {
            continue;
        };
        let caller = text(call, "caller_nid");
        if caller.is_empty()
            || caller == target
            || !known.insert((caller.clone(), target.clone(), "calls".into()))
        {
            continue;
        }
        resolved.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"INFERRED","confidence_score":0.85,"source_file":get(call,"source_file",json!("")),"source_location":call["source_location"],"weight":1.0}));
    }
    resolved
}
pub fn bash_make_id(parts: &[String]) -> String {
    make_id(parts.iter().map(String::as_str))
}
pub fn file_node_id_for_path(path: &Path, root: &Path) -> Result<String> {
    let resolved = fsutil::resolve_missing(path).map_err(crate::Error::from)?;
    let root = fsutil::resolve_missing(root).map_err(crate::Error::from)?;
    match resolved.strip_prefix(root) {
        Ok(relative) => Ok(make_id([file_stem(relative).as_str()])),
        Err(_) => Ok(make_id([fsutil::path_string(path).as_str()])),
    }
}
pub fn resolve_bash_source_edges(
    per_file: &[Value],
    paths: &[PathBuf],
    root: &Path,
    edges: &[Value],
) -> Result<Vec<Value>> {
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|path| fsutil::resolve_missing(path).map_err(crate::Error::from))
        .collect::<Result<_>>()?;
    let ids: HashMap<PathBuf, String> = paths
        .iter()
        .map(|path| file_node_id_for_path(path, root).map(|id| (path.clone(), id)))
        .collect::<Result<_>>()?;
    let mut functions: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (result, path) in per_file.iter().zip(&paths) {
        for node in result
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            if node
                .get("metadata")
                .and_then(Value::as_object)
                .and_then(|map| map.get("kind"))
                .and_then(Value::as_str)
                != Some("bash_function")
            {
                continue;
            }
            let label = text(node, "label");
            let name = strip(label.strip_suffix("()").unwrap_or(&label));
            if !name.is_empty() && truthy(&node["id"]) {
                functions
                    .entry(ids[path].clone())
                    .or_default()
                    .insert(name.into(), text(node, "id"));
            }
        }
    }
    let mut sourced: HashMap<String, HashSet<String>> = HashMap::new();
    let mut resolved = Vec::new();
    // Only (source, target, relation) triples this resolver may emit are ever
    // looked up: a Bash file sourcing another, or a Bash call. Index just the
    // existing edges that could equal one (same source text and relation)
    // instead of every edge of the corpus.
    let mut sources = ids.values().cloned().collect::<HashSet<_>>();
    for result in per_file {
        for call in result
            .get("raw_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            if call["language"].as_str() == Some("bash") && truthy(&call["caller_nid"]) {
                sources.insert(text(call, "caller_nid"));
            }
        }
    }
    let mut existing = edges
        .iter()
        .filter(|edge| truthy(&edge["source"]) && truthy(&edge["target"]))
        .filter(|edge| {
            matches!(edge["relation"].as_str(), Some("imports_from" | "calls"))
                && sources.contains(&text(edge, "source"))
        })
        .map(|edge| {
            (
                text(edge, "source"),
                text(edge, "target"),
                text(edge, "relation"),
            )
        })
        .collect::<HashSet<_>>();
    for (result, path) in per_file.iter().zip(&paths) {
        let source_id = &ids[path];
        for source in result
            .get("bash_sources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            let Some(target) = source["target_path"]
                .as_str()
                .filter(|s| !strip(s).is_empty())
            else {
                continue;
            };
            let candidate = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                path.parent().unwrap_or(Path::new(".")).join(target)
            };
            let Ok(target) = fsutil::resolve_missing(&candidate) else {
                continue;
            };
            let Some(target_id) = ids.get(target.as_path()) else {
                continue;
            };
            sourced
                .entry(source_id.clone())
                .or_default()
                .insert(target_id.clone());
            if !existing.insert((source_id.clone(), target_id.clone(), "imports_from".into())) {
                continue;
            }
            resolved.push(json!({"source":source_id,"target":target_id,"relation":"imports_from","context":"import","confidence":"EXTRACTED","confidence_score":1.0,
                "source_file":get(source,"source_file",json!(fsutil::path_string(path))),"source_location":get(source,"source_location",json!("")),"weight":1.0}));
        }
    }
    for (result, path) in per_file.iter().zip(&paths) {
        let Some(imported) = sourced.get(&ids[path]) else {
            continue;
        };
        for call in result
            .get("raw_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            if call["language"].as_str() != Some("bash") {
                continue;
            }
            let Some(callee) = call["callee"].as_str().filter(|c| !c.is_empty()) else {
                continue;
            };
            if !truthy(&call["caller_nid"]) {
                continue;
            }
            let matches: Vec<_> = imported
                .iter()
                .filter_map(|file| functions.get(file).and_then(|map| map.get(callee)))
                .collect();
            if matches.len() != 1 {
                continue;
            }
            let caller = text(call, "caller_nid");
            let target = matches[0];
            if !existing.insert((caller.clone(), target.clone(), "calls".into())) {
                continue;
            }
            resolved.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":get(call,"source_file",json!(fsutil::path_string(path))),"source_location":get(call,"source_location",json!("")),"weight":1.0}));
        }
    }
    Ok(resolved)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let list = |field: &str| {
        request[field]
            .as_array()
            .ok_or_else(|| format!("Expected {field}"))
    };
    match request["operation"].as_str().unwrap_or("") {
        "symbols.normalize" => Ok(json!(normalise_callable_label(
            request["label"].as_str().ok_or("Expected label")?
        ))),
        "symbols.resolvable" => Ok(json!(node_is_resolvable_symbol(&request["node"]))),
        "symbols.labels" => Ok(json!(build_label_index(
            list("nodes")?,
            request["edges"].as_array().map(Vec::as_slice)
        ))),
        "symbols.pairs" => {
            let mut pairs: Vec<_> = existing_edge_pairs(list("edges")?).into_iter().collect();
            pairs.sort();
            Ok(json!(pairs))
        }
        "symbols.raw_calls" => Ok(json!(iter_raw_calls(list("per_file")?))),
        "symbols.imports" => Ok(json!(parse_python_import_aliases(Path::new(
            request["path"].as_str().ok_or("Expected path")?
        )))),
        "symbols.python_index" => Ok(json!(
            build_python_symbol_index(list("nodes")?)
                .into_iter()
                .collect::<Vec<_>>()
        )),
        "symbols.unique" => {
            let entries: Vec<((String, String), Vec<String>)> =
                serde_json::from_value(request["index"].clone()).map_err(crate::Error::from)?;
            let imported: ImportedSymbol =
                serde_json::from_value(request["imported"].clone()).map_err(crate::Error::from)?;
            Ok(json!(find_unique_python_symbol(
                &entries.into_iter().collect(),
                &imported
            )))
        }
        "symbols.cross_calls" => Ok(json!(resolve_cross_file_raw_calls(
            list("per_file")?,
            list("nodes")?,
            list("edges")?
        ))),
        "symbols.import_calls" => {
            let paths: Vec<PathBuf> =
                serde_json::from_value(request["paths"].clone()).map_err(|_| "Expected paths")?;
            if let Some(supplied) = request.get("aliases_by_file") {
                let aliases: BTreeMap<String, BTreeMap<String, ImportedSymbol>> =
                    serde_json::from_value(supplied.clone()).map_err(crate::Error::from)?;
                return Ok(json!(resolve_python_import_guided_calls_with(
                    list("per_file")?,
                    &paths,
                    list("nodes")?,
                    list("edges")?,
                    |path| aliases
                        .get(&fsutil::path_string(path))
                        .cloned()
                        .unwrap_or_default()
                )));
            }
            Ok(json!(resolve_python_import_guided_calls(
                list("per_file")?,
                &paths,
                list("nodes")?,
                list("edges")?
            )))
        }
        "symbols.bash_edges" => {
            let paths: Vec<PathBuf> =
                serde_json::from_value(request["paths"].clone()).map_err(|_| "Expected paths")?;
            Ok(json!(resolve_bash_source_edges(
                list("per_file")?,
                &paths,
                Path::new(request["root"].as_str().ok_or("Expected root")?),
                list("edges")?
            )?))
        }
        "symbols.bash_id" => {
            let parts: Vec<String> =
                serde_json::from_value(request["parts"].clone()).map_err(|_| "Expected parts")?;
            Ok(json!(bash_make_id(&parts)))
        }
        _ => Err("Unknown symbols operation".into()),
    }
}
