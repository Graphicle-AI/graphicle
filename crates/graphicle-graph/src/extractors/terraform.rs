// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/terraform.py
//! Terraform/HCL extraction, portable directory IDs and incremental module topology.
use super::base::{make_id, read_text, ts_index};
use crate::{Result, fsutil, io_compat, path_syntax, python_compat, security};
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::LazyLock,
};
use tree_sitter::Node;

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn named(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count()).filter_map(move |i| node.named_child(ts_index(i)?))
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| *c > '\x1f' && *c != '\x7f')
        .take(security::METADATA_MAX_VALUE_LEN)
        .collect()
}
fn unpaired_surrogate(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] != b'\\' {
            offset += 1;
            continue;
        }
        if bytes.get(offset + 1) != Some(&b'u') {
            offset += 2;
            continue;
        }
        let value = raw
            .get(offset + 2..offset + 6)
            .and_then(|s| u16::from_str_radix(s, 16).ok());
        offset += 6;
        match value {
            Some(0xdc00..=0xdfff) => return true,
            Some(0xd800..=0xdbff) => {
                let low = raw
                    .get(offset..offset + 2)
                    .filter(|s| *s == "\\u")
                    .and_then(|_| raw.get(offset + 2..offset + 6))
                    .and_then(|s| u16::from_str_radix(s, 16).ok());
                if !matches!(low, Some(0xdc00..=0xdfff)) {
                    return true;
                }
                offset += 6;
            }
            _ => (),
        }
    }
    false
}
static SENSITIVE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
    "password|passwd|secret|token|api[-_]?key|access[-_]?key|private[-_]?key|credential|client[-_]?secret|connection[-_]?string|sas[-_]?token|auth|passphrase"
).unwrap()
});

fn sensitive_name(key: &str) -> bool {
    // Python's Unicode IGNORECASE includes these non-ASCII ASCII-letter folds.
    let key: String = key
        .chars()
        .map(|c| match c {
            'İ' | 'ı' => 'i',
            'ſ' => 's',
            'K' => 'k',
            c => c.to_ascii_lowercase(),
        })
        .collect();
    SENSITIVE.is_match(&key)
}

pub fn redact_value(key: &str, value: &Value) -> Value {
    if sensitive_name(key) {
        return json!("[redacted]");
    }
    match value {
        Value::Object(values) => {
            // ECS/Batch and ordinary configuration use {name, value} pairs.
            // The value's own generic key does not carry the secret signal.
            let named_secret = values.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("name") && v.as_str().is_some_and(sensitive_name)
            });
            Value::Object(
                values
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            if named_secret
                                && (k.eq_ignore_ascii_case("value")
                                    || k.eq_ignore_ascii_case("valueFrom")
                                    || k.eq_ignore_ascii_case("value_from"))
                            {
                                json!("[redacted]")
                            } else {
                                redact_value(k, v)
                            },
                        )
                    })
                    .collect(),
            )
        }
        // Lists inherit the parent key so `configs = [{ password = "x" }]` is redacted (PR #3762).
        Value::Array(items) => Value::Array(items.iter().map(|v| redact_value(key, v)).collect()),
        _ => value.clone(),
    }
}
pub fn scope_id(directory: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(directory.as_bytes()));
    make_id(["terraform", directory, &digest[..16]])
}

pub fn prepare_terraform(result: &mut Value, path: &Path, root: &Path) -> Result<()> {
    let directory = fsutil::resolve_missing(path.parent().unwrap_or(Path::new(".")))
        .map_err(crate::Error::from)?;
    let Ok(relative) = directory.strip_prefix(root) else {
        return Ok(());
    };
    let relative = fsutil::path_string(relative);
    let old = scope_id(&fsutil::path_string(&directory));
    let new = scope_id(&relative);
    let prefix = format!("{old}_");
    if let Some(nodes) = result.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes.iter_mut() {
            if let Some(id) = node["id"].as_str().filter(|id| id.starts_with(&prefix)) {
                node["id"] = json!(format!("{}{}", new, &id[old.len()..]));
            }
            node["_terraform_directory"] = json!(relative);
        }
        for node in nodes {
            let source = node
                .get("module_source")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !(source.starts_with("./") || source.starts_with("../")) {
                continue;
            }
            if let Ok(target) = fsutil::resolve_missing(&directory.join(source))
                && let Ok(target) = target.strip_prefix(root)
            {
                node["_terraform_source_directory"] = json!(fsutil::path_string(target));
            }
        }
    }
    if let Some(edges) = result.get_mut("edges").and_then(Value::as_array_mut) {
        for edge in edges {
            for key in ["source", "target"] {
                if let Some(id) = edge[key].as_str().filter(|id| id.starts_with(&prefix)) {
                    edge[key] = json!(format!("{}{}", new, &id[old.len()..]));
                }
            }
        }
    }
    Ok(())
}

pub fn resolve_terraform_modules(
    per_file: &[Value],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    let mut files: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut anchors: HashMap<String, String> = HashMap::new();
    for node in nodes.iter() {
        let Some(directory) = node.get("_terraform_directory").and_then(Value::as_str) else {
            continue;
        };
        let source = node["source_file"].as_str().unwrap_or("");
        if node["type"] == "terraform_file"
            && path_syntax::suffix(path_syntax::posix_name(&path_syntax::posix_parts(source)))
                == ".tf"
        {
            files
                .entry(directory.into())
                .or_default()
                .push(node.clone());
        } else if node["type"] == "module" && node["language"] == "terraform" {
            anchors.insert(directory.into(), node["id"].as_str().unwrap_or("").into());
        }
    }
    let fresh_files: HashSet<_> = per_file
        .iter()
        .flat_map(|r| r["nodes"].as_array().into_iter().flatten())
        .filter(|n| n["type"] == "terraform_file")
        .filter_map(|n| n["id"].as_str())
        .collect();
    let mut known: HashSet<String> = nodes
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_owned))
        .collect();
    for (directory, members) in files {
        let anchor = anchors
            .entry(directory.clone())
            .or_insert_with(|| make_id([scope_id(&directory).as_str(), "directory"]));
        if known.insert(anchor.clone()) {
            let owner = members
                .iter()
                .min_by_key(|n| n["source_file"].as_str().unwrap_or(""))
                .unwrap();
            nodes.push(json!({"id":anchor,"label":format!("Terraform module: {directory}"),"type":"module","language":"terraform","file_type":"code","source_file":owner["source_file"],"source_location":null,"_terraform_directory":directory}));
        }
        for member in members {
            if fresh_files.contains(member["id"].as_str().unwrap_or("")) {
                edges.push(json!({"source":anchor,"target":member["id"],"relation":"contains","confidence":"EXTRACTED","weight":1.0,"source_file":member["source_file"],"source_location":null}));
            }
        }
    }
    let fresh_calls: HashSet<_> = per_file
        .iter()
        .flat_map(|r| r["nodes"].as_array().into_iter().flatten())
        .filter(|n| n.get("module_source").is_some())
        .filter_map(|n| n["id"].as_str())
        .collect();
    for node in nodes.iter() {
        if !fresh_calls.contains(node["id"].as_str().unwrap_or("")) {
            continue;
        }
        if let Some(target) = node
            .get("_terraform_source_directory")
            .and_then(Value::as_str)
            .and_then(|d| anchors.get(d))
        {
            edges.push(json!({"source":node["id"],"target":target,"relation":"module_source","confidence":"EXTRACTED","weight":1.0,"source_file":node["source_file"],"source_location":node["module_source_location"]}));
        }
    }
}

pub fn refresh_terraform_paths(
    changed: &[PathBuf],
    corpus: &[PathBuf],
    invalidated: &[PathBuf],
) -> Vec<PathBuf> {
    let is_tf = |p: &Path| {
        path_syntax::suffix(path_syntax::posix_name(&path_syntax::posix_parts(
            &fsutil::path_string(p),
        ))) == ".tf"
    };
    if !changed.iter().chain(invalidated).any(|p| {
        is_tf(p)
            || matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some(".gitignore" | ".graphicleignore")
            )
    }) {
        return changed.to_vec();
    }
    let mut seen = HashSet::new();
    changed
        .iter()
        .chain(corpus.iter().filter(|p| is_tf(p)))
        .filter(|p| seen.insert(fsutil::path_string(p)))
        .cloned()
        .collect()
}

struct Extractor<'source> {
    source: &'source [u8],
    path: String,
    scope: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    by_id: HashMap<String, usize>,
    seen_edges: HashSet<(String, String, String)>,
}
impl Extractor<'_> {
    fn node(&mut self, address: &str, line: usize) -> String {
        let id = make_id([self.scope.as_str(), address]);
        if !self.by_id.contains_key(&id) {
            self.by_id.insert(id.clone(), self.nodes.len());
            self.nodes.push(json!({"id":id,"label":address,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
            self.edges.push(json!({"source":self.file_id,"target":id,"relation":"contains","confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
        }
        id
    }
    fn edge(&mut self, source: &str, address: &str, relation: &str, line: usize) {
        let target = make_id([self.scope.as_str(), address]);
        if source == target
            || !self
                .seen_edges
                .insert((source.into(), target.clone(), relation.into()))
        {
            return;
        }
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn block_parts(&self, block: Node<'_>) -> (Option<String>, Vec<String>) {
        let mut typ = None;
        let mut labels = Vec::new();
        for child in children(block) {
            match child.kind() {
                "block_start" | "body" | "block_end" => break,
                "identifier" if typ.is_none() => typ = Some(read_text(child, self.source)),
                "string_lit" | "identifier" => labels.push(
                    trim(&read_text(child, self.source))
                        .trim_matches('"')
                        .into(),
                ),
                _ => (),
            }
        }
        (typ, labels)
    }
    fn ref_address(&self, expr: Node<'_>) -> Option<String> {
        let head = read_text(expr, self.source);
        let mut attrs = Vec::new();
        if let Some(parent) = expr.parent() {
            let mut seen = false;
            for child in children(parent) {
                if child.id() == expr.id() {
                    seen = true;
                    continue;
                }
                if seen && child.kind() == "get_attr" {
                    let Some(name) = children(child).find(|n| n.kind() == "identifier") else {
                        break;
                    };
                    attrs.push(read_text(name, self.source));
                } else if seen {
                    break;
                }
            }
        }
        if head.is_empty()
            || matches!(
                head.as_str(),
                "count" | "each" | "self" | "path" | "terraform"
            )
        {
            return None;
        }
        if head == "data" {
            return (attrs.len() >= 2).then(|| format!("data.{}.{}", attrs[0], attrs[1]));
        }
        attrs.first().map(|a| format!("{head}.{a}"))
    }
    fn collect_refs(&mut self, node: Node<'_>, owner: &str, relation: &str) {
        let relation = if node.kind() == "attribute"
            && node
                .child_by_field_name("key")
                .or_else(|| node.child(0))
                .is_some_and(|n| read_text(n, self.source) == "depends_on")
        {
            "depends_on"
        } else {
            relation
        };
        if node.kind() == "variable_expr"
            && let Some(address) = self.ref_address(node)
        {
            self.edge(owner, &address, relation, node.start_position().row + 1);
        }
        for child in named(node) {
            self.collect_refs(child, owner, relation);
        }
    }
    fn parse_value(&self, mut node: Node<'_>, depth: usize) -> Result<Value> {
        if depth > 5 {
            return Ok(json!(clean(trim(&read_text(node, self.source)))));
        }
        while matches!(
            node.kind(),
            "expression" | "literal_value" | "collection_value"
        ) && node.named_child_count() == 1
        {
            node = node.named_child(0).unwrap();
        }
        let raw = read_text(node, self.source);
        let raw = trim(&raw);
        Ok(match node.kind() {
            "bool_lit" => json!(raw == "true"),
            "numeric_lit" => {
                let unsigned = raw
                    .strip_prefix('-')
                    .or_else(|| raw.strip_prefix('+'))
                    .unwrap_or(raw);
                let digits = unsigned.replace('_', "");
                if !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && digits.len() <= 4300
                {
                    let digits = digits.trim_start_matches('0');
                    let decimal = if digits.is_empty() {
                        "0".into()
                    } else if raw.starts_with('-') {
                        format!("-{digits}")
                    } else {
                        digits.into()
                    };
                    Value::Number(Number::from_str(&decimal).map_err(crate::Error::from)?)
                } else if let Ok(value) = raw.replace('_', "").parse::<f64>() {
                    Value::Number(Number::from_f64(value).ok_or("Non-finite Terraform literal cannot be represented in the JSON graph contract")?)
                } else {
                    json!(clean(raw))
                }
            }
            "null_lit" => Value::Null,
            "string_lit" if raw.starts_with('"') && raw.ends_with('"') => match serde_json::from_str::<
                String,
            >(raw)
            {
                Ok(value) => json!(clean(&value)),
                Err(_) if unpaired_surrogate(raw) => return Err(
                    "Unpaired Unicode surrogate cannot be represented in the UTF-8 graph contract"
                        .into(),
                ),
                Err(_) => json!(clean(raw.get(1..raw.len() - 1).unwrap_or(""))),
            },
            "tuple" => {
                let values = named(node)
                    .filter(|n| !matches!(n.kind(), "tuple_start" | "tuple_end"))
                    .take(security::METADATA_MAX_LIST_ITEMS)
                    .map(|n| self.parse_value(n, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                Value::Array(values)
            }
            "object" => {
                let mut object = Map::new();
                for child in children(node).filter(|n| n.kind() == "object_elem") {
                    let key = child
                        .child_by_field_name("key")
                        .or_else(|| child.named_child(0));
                    let value = child.child_by_field_name("val").or_else(|| {
                        (child.named_child_count() > 1)
                            .then(|| {
                                ts_index(child.named_child_count() - 1)
                                    .and_then(|i| child.named_child(i))
                            })
                            .flatten()
                    });
                    if let (Some(key), Some(value)) = (key, value) {
                        let parsed = self.parse_value(key, depth + 1)?;
                        let key = if parsed.is_null() {
                            trim(&read_text(key, self.source)).trim_matches('"').into()
                        } else {
                            python_compat::string(&parsed)
                        };
                        object.insert(clean(&key), self.parse_value(value, depth + 1)?);
                        if object.len() >= security::METADATA_MAX_LIST_ITEMS {
                            break;
                        }
                    }
                }
                Value::Object(object)
            }
            _ => json!(clean(raw)),
        })
    }
    fn direct_attributes(&self, body: Node<'_>) -> Result<Map<String, Value>> {
        let mut attrs = Map::new();
        for child in children(body).filter(|n| n.kind() == "attribute") {
            let Some(key) = child.child_by_field_name("key").or_else(|| child.child(0)) else {
                continue;
            };
            let key = clean(trim(&read_text(key, self.source)));
            if key.is_empty() {
                continue;
            }
            if let Some(value) = named(child).last() {
                attrs.insert(
                    key.clone(),
                    redact_value(&key, &self.parse_value(value, 0)?),
                );
                if attrs.len() >= security::METADATA_MAX_ATTRIBUTES {
                    break;
                }
            }
        }
        Ok(attrs)
    }
    fn extract(&mut self, root: Node<'_>, tf: bool) -> Result<()> {
        let body = children(root).find(|n| n.kind() == "body").unwrap_or(root);
        for block in children(body).filter(|n| n.kind() == "block") {
            let (kind, labels) = self.block_parts(block);
            let body = children(block).find(|n| n.kind() == "body");
            let line = block.start_position().row + 1;
            let address = match kind.as_deref() {
                Some("resource") if labels.len() >= 2 => format!("{}.{}", labels[0], labels[1]),
                Some("data") if labels.len() >= 2 => format!("data.{}.{}", labels[0], labels[1]),
                Some("module") if !labels.is_empty() => format!("module.{}", labels[0]),
                Some("variable") if !labels.is_empty() => format!("var.{}", labels[0]),
                Some("output") if !labels.is_empty() => format!("output.{}", labels[0]),
                Some("provider") if !labels.is_empty() => format!("provider.{}", labels[0]),
                Some("locals") if body.is_some() => {
                    for attr in children(body.unwrap()).filter(|n| n.kind() == "attribute") {
                        if let Some(key) = attr.child(0) {
                            let address = format!("local.{}", read_text(key, self.source));
                            let id = self.node(&address, attr.start_position().row + 1);
                            self.collect_refs(attr, &id, "references");
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            let owner = self.node(&address, line);
            if let Some(body) = body {
                if kind.as_deref() == Some("module") && tf {
                    for attr in named(body).filter(|n| n.kind() == "attribute") {
                        if attr
                            .named_child(0)
                            .is_none_or(|n| read_text(n, self.source) != "source")
                        {
                            continue;
                        }
                        let Some(mut value) = named(attr).last() else {
                            continue;
                        };
                        while matches!(value.kind(), "expression" | "literal_value")
                            && value.named_child_count() == 1
                        {
                            value = value.named_child(0).unwrap();
                        }
                        if value.kind() != "string_lit" || value.has_error() {
                            continue;
                        }
                        if let Ok(source) =
                            serde_json::from_str::<String>(&read_text(value, self.source))
                        {
                            let node = &mut self.nodes[self.by_id[&owner]];
                            node["module_source"] =
                                json!(source.replace("$${", "${").replace("%%{", "%{"));
                            node["module_source_location"] =
                                json!(format!("L{}", attr.start_position().row + 1));
                        }
                    }
                }
                let mut attrs = self.direct_attributes(body)?;
                // `attrs.get("sensitive") in (True, "true")`: Python equality,
                // so a numeric 1 is `True` as well.
                let sensitive = attrs.get("sensitive").is_some_and(|v| {
                    matches!(v, Value::Bool(true)) || v == "true" || v.as_f64() == Some(1.0)
                });
                if matches!(kind.as_deref(), Some("variable" | "output"))
                    && (labels.first().is_some_and(|name| sensitive_name(name)) || sensitive)
                {
                    for key in ["default", "value"] {
                        if let Some(value) = attrs.get_mut(key) {
                            *value = json!("[redacted]");
                        }
                    }
                }
                if !attrs.is_empty() {
                    self.nodes[self.by_id[&owner]]["attributes"] = Value::Object(attrs);
                }
                self.collect_refs(body, &owner, "references");
            }
        }
        Ok(())
    }
}

pub fn extract_terraform(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(e) => return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)})),
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&graphicle_tree_sitter_hcl::LANGUAGE.into())
        .map_err(crate::Error::from)?;
    let tree = parser.parse(&source, None).ok_or("HCL parse cancelled")?;
    let directory = fsutil::resolve_missing(path.parent().unwrap_or(Path::new(".")))
        .map_err(crate::Error::from)?;
    let path_text = fsutil::path_string(path);
    let parts = path_syntax::posix_parts(&path_text);
    let name = path_syntax::posix_name(&parts);
    let source_file = fsutil::path_string(&directory.join(name));
    let file_id = make_id([source_file.as_str()]);
    let mut extractor = Extractor {
        source: &source,
        path: source_file.clone(),
        scope: scope_id(&fsutil::path_string(&directory)),
        file_id: file_id.clone(),
        nodes: vec![
            json!({"id":file_id,"label":name,"file_type":"code","source_file":source_file,"source_location":null,"type":"terraform_file"}),
        ],
        edges: vec![],
        by_id: HashMap::from([(file_id, 0)]),
        seen_edges: HashSet::new(),
    };
    extractor.extract(tree.root_node(), path_syntax::suffix(name) == ".tf")?;
    Ok(json!({"nodes":extractor.nodes,"edges":extractor.edges}))
}

pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "terraform.scope" => Ok(json!(scope_id(
            request["directory"].as_str().ok_or("Expected directory")?
        ))),
        "terraform.redact" => Ok(redact_value(
            request["key"].as_str().ok_or("Expected key")?,
            &request["value"],
        )),
        "terraform.prepare" => {
            let mut result = request["data"].clone();
            prepare_terraform(
                &mut result,
                Path::new(request["path"].as_str().ok_or("Expected path")?),
                Path::new(request["root"].as_str().ok_or("Expected root")?),
            )?;
            Ok(result)
        }
        "terraform.resolve" => {
            let per_file = request["per_file"].as_array().ok_or("Expected per_file")?;
            let mut nodes = request["nodes"].as_array().ok_or("Expected nodes")?.clone();
            let mut edges = request["edges"].as_array().ok_or("Expected edges")?.clone();
            resolve_terraform_modules(per_file, &mut nodes, &mut edges);
            Ok(json!({"nodes":nodes,"edges":edges}))
        }
        "terraform.refresh" => {
            let paths = |key| -> Result<Vec<PathBuf>> {
                request[key]
                    .as_array()
                    .ok_or("Expected paths")?
                    .iter()
                    .map(|p| p.as_str().map(PathBuf::from).ok_or("Expected path".into()))
                    .collect()
            };
            Ok(json!(refresh_terraform_paths(
                &paths("changed")?,
                &paths("corpus")?,
                &paths("invalidated")?
            )))
        }
        _ => Err("Unknown Terraform operation".into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn secret_names_protect_paired_values_and_block_defaults() {
        assert_eq!(
            super::redact_value(
                "environment",
                &json!([
                    {"name":"DB_PASSWORD","value":"fixture-secret","description":"keep"},
                    {"Name":"api_key","valueFrom":"fixture-reference"},
                    {"name":"APP_MODE","value":"production"},
                ])
            ),
            json!([
                {"name":"DB_PASSWORD","value":"[redacted]","description":"keep"},
                {"Name":"api_key","valueFrom":"[redacted]"},
                {"name":"APP_MODE","value":"production"},
            ])
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("variables.tf");
        std::fs::write(
            &path,
            r#"
variable "db_password" { default = "fixture-password" }
variable "material" {
  sensitive = true
  default = "fixture-material"
}
output "admin_token" { value = "fixture-token" }
variable "region" { default = "us-east-1" }
"#,
        )
        .unwrap();
        let graph = super::extract_terraform(&path).unwrap();
        let serialized = graph.to_string();
        for value in ["fixture-password", "fixture-material", "fixture-token"] {
            assert!(!serialized.contains(value));
        }
        assert!(serialized.contains("us-east-1"));
        assert_eq!(serialized.matches("[redacted]").count(), 3);
    }

    #[test]
    fn redaction_recurses_into_lists_of_maps() {
        let value = json!({"configs": [{"password": "hunter2", "name": "a"}], "tags": ["a", "b"]});
        assert_eq!(
            super::redact_value("default", &value),
            json!({"configs": [{"password": "[redacted]", "name": "a"}], "tags": ["a", "b"]})
        );
        assert_eq!(
            super::redact_value("list", &json!(["a", 1, null])),
            json!(["a", 1, null])
        );
        assert_eq!(
            super::redact_value("api_key", &json!(["x"])),
            json!("[redacted]")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.tf");
        std::fs::write(
            &path,
            "variable \"x\" { default = { configs = [{ password = \"hunter2\", name = \"a\" }] } }\n",
        )
        .unwrap();
        let result = super::extract_terraform(&path).unwrap();
        let attributes = &result["nodes"][1]["attributes"]["default"];
        assert_eq!(
            attributes,
            &json!({"configs": [{"password": "[redacted]", "name": "a"}]})
        );
    }

    #[test]
    fn grammar_matches_the_pinned_python_package() {
        let language: tree_sitter::Language = graphicle_tree_sitter_hcl::LANGUAGE.into();
        assert_eq!(language.node_kind_count(), 121);
        assert_eq!(language.parse_state_count(), 678);
        assert_eq!(language.abi_version(), 14);
    }
}
