// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/yaml_config.py
//! Structural YAML extraction (Graphify 0.9.95, #2214): `.yaml`/`.yml` files are
//! code. A file node plus `contains` edges to keys, the shape `extract_json`
//! gives JSON; every document of a multi-document stream is walked, data-shaped
//! YAML (a plain data array, an OpenAPI/Swagger spec, a JSON schema, a flat
//! all-scalar mapping) is skipped, and output is bounded (1 MiB, 500 pairs,
//! depth 6). Loading follows PyYAML's `safe_load_all` (`yaml_frontmatter`), so
//! a `!!python/...` or unknown tag is refused, never constructed.
use super::{
    base::{file_stem, make_id},
    yaml_frontmatter::{self, Loaded},
};
use crate::{fsutil, ids::normalize_id, io_compat, python_compat};
use serde_json::{Value, json};
use std::{collections::HashSet, io::Read, path::Path};

pub const YAML_MAX_BYTES: usize = 1_048_576;
const MAX_PAIRS: usize = 500;
const MAX_DEPTH: usize = 6;
/// Config/manifest file names worth structural extraction regardless of shape.
pub const CONFIG_YAML_NAMES: &[&str] = &[
    "values.yaml",
    "values.yml",
    "chart.yaml",
    "chart.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
    "compose.yaml",
    "compose.yml",
    "kustomization.yaml",
    "kustomization.yml",
    "skaffold.yaml",
    "skaffold.yml",
    "serverless.yaml",
    "serverless.yml",
    "mkdocs.yaml",
    "mkdocs.yml",
    ".gitlab-ci.yml",
    ".gitlab-ci.yaml",
    ".pre-commit-config.yaml",
    "cloudbuild.yaml",
    "cloudbuild.yml",
    "pubspec.yaml",
    "environment.yml",
    "action.yml",
    "action.yaml",
    "catalog-info.yaml",
    ".travis.yml",
    "appveyor.yml",
    "azure-pipelines.yml",
];
/// Top-level keys that mark a config or manifest (Kubernetes, compose, CI,
/// Ansible, Helm, serverless).
pub const CONFIG_YAML_KEYS: &[&str] = &[
    "apiversion",
    "kind",
    "metadata",
    "spec",
    "services",
    "volumes",
    "networks",
    "jobs",
    "steps",
    "stages",
    "on",
    "workflows",
    "pipeline",
    "hosts",
    "tasks",
    "roles",
    "name",
    "version",
    "dependencies",
    "scripts",
    "resources",
    "provider",
    "functions",
];

/// Python's `str.casefold()`.
fn casefold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        match crate::unicode15::CASEFOLD.binary_search_by_key(&c, |entry| entry.0) {
            Ok(index) => folded.push_str(crate::unicode15::CASEFOLD[index].1),
            Err(_) => folded.push(c),
        }
    }
    folded
}

/// `str(key)`: a non-finite float key reads `inf`, `-inf` or `nan`.
fn key_text(key: &Value) -> String {
    yaml_frontmatter::nonfinite_key(key)
        .map(str::to_owned)
        .unwrap_or_else(|| python_compat::string(key))
}

/// `str(key).casefold()` of a mapping's keys.
fn folded_keys(entries: &[(Value, Loaded)]) -> HashSet<String> {
    entries
        .iter()
        .map(|(key, _)| casefold(&key_text(key)))
        .collect()
}

fn structured(value: &Loaded) -> bool {
    matches!(value, Loaded::Seq(_) | Loaded::Map(_))
}

/// `_is_config_yaml`: a recognized config/manifest document worth extracting.
fn is_config_yaml(str_path: &str, name: &str, data: &Loaded) -> bool {
    if CONFIG_YAML_NAMES.contains(&casefold(name).as_str()) {
        return true;
    }
    if casefold(str_path).contains("/.github/workflows/") {
        return true;
    }
    let entries = match data {
        // A top-level sequence is config only as an Ansible-style list of
        // plays or tasks; a plain data array is not.
        Loaded::Seq(items) => {
            return items.iter().any(|item| match item {
                Loaded::Map(entries) => {
                    let keys = folded_keys(entries);
                    ["hosts", "tasks", "roles"]
                        .iter()
                        .any(|key| keys.contains(*key))
                }
                _ => false,
            });
        }
        Loaded::Map(entries) => entries,
        Loaded::Other(_) => return false,
    };
    let keys = folded_keys(entries);
    if (keys.contains("openapi") || keys.contains("swagger")) && keys.contains("paths") {
        return false;
    }
    if keys.contains("$schema")
        && ["$defs", "definitions", "$id"]
            .iter()
            .any(|key| keys.contains(*key))
    {
        return false;
    }
    if CONFIG_YAML_KEYS.iter().any(|key| keys.contains(*key)) {
        return true;
    }
    // A mapping with a nested value is config-like; a flat all-scalar mapping
    // with no config key is a data bundle.
    entries.iter().any(|(_, value)| structured(value))
}

/// `dict.get(name)` for a string key.
fn get<'a>(entries: &'a [(Value, Loaded)], name: &str) -> Option<&'a Loaded> {
    entries
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// `_doc_label`: a stable label for one document of a multi-document stream.
fn doc_label(doc: &Loaded, index: usize) -> String {
    if let Loaded::Map(entries) = doc
        && let Some(Loaded::Other(Value::String(kind))) = get(entries, "kind")
        && !kind.is_empty()
    {
        if let Some(Loaded::Map(meta)) = get(entries, "metadata")
            && let Some(Loaded::Other(Value::String(name))) = get(meta, "name")
            && !name.is_empty()
        {
            return format!("{kind} {name}");
        }
        return kind.clone();
    }
    format!("doc {index}")
}

/// PyYAML's reader refuses a stream holding any character outside YAML's
/// printable set (`Reader.NON_PRINTABLE`), before loading anything.
fn non_printable(text: &str) -> Option<char> {
    text.chars().find(|&c| {
        !matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{7e}' | '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    })
}

/// PyYAML's scanner skips only spaces between tokens, so a tab where a token
/// would start is an error ("found character '\t' that cannot start any
/// token"); yaml-rust2 accepts tabs there. This finds the common case, a tab in
/// a line's leading whitespace, outside the content of a block scalar (`|`,
/// `>`) and outside a quoted scalar continued from an earlier line, where tabs
/// are text. Tabs as separation after a token (`key:\tvalue`) are not detected.
fn leading_tab_line(text: &str) -> Option<usize> {
    let mut block: Option<(usize, Option<usize>)> = None; // (parent indent, content indent)
    let mut quote: Option<char> = None;
    for (number, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let spaces = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[spaces..];
        if quote.is_none()
            && let Some((parent, content)) = block
        {
            let blank = rest.trim_matches([' ', '\t']).is_empty();
            match content {
                _ if blank => continue,
                None if spaces > parent => {
                    block = Some((parent, Some(spaces)));
                    continue;
                }
                Some(indent) if spaces >= indent => continue,
                _ => block = None,
            }
        }
        if quote.is_none() && rest.starts_with('\t') {
            return Some(number + 1);
        }
        // Quoted scalars opened on this line and still open at its end, and a
        // block indicator ending it.
        let mut previous = None::<char>;
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match quote {
                Some('\'') if c == '\'' => {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
                Some('"') if c == '\\' => {
                    chars.next();
                }
                Some('"') if c == '"' => quote = None,
                Some(_) => {}
                None if c == '#' && previous.is_none_or(|p| p == ' ' || p == '\t') => break,
                None if (c == '\'' || c == '"')
                    && previous.is_none_or(|p| matches!(p, ' ' | '\t' | '[' | '{' | ',')) =>
                {
                    quote = Some(c)
                }
                None => {}
            }
            if !c.is_whitespace() || quote.is_some() {
                previous = Some(c);
            } else {
                previous = Some(' ');
            }
        }
        if quote.is_none() {
            let code = rest
                .split(" #")
                .next()
                .unwrap_or(rest)
                .trim_end_matches([' ', '\t']);
            let indicator = code
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == '+' || c == '-')
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
            if (indicator.ends_with(" |")
                || indicator.ends_with(" >")
                || indicator == "|"
                || indicator == ">")
                && !code.starts_with('#')
            {
                block = Some((spaces, None));
            }
        }
    }
    None
}

/// `bytes.decode("utf-8", errors="ignore")`: invalid sequences are dropped.
fn decode_ignoring_errors(raw: &[u8]) -> String {
    let mut text = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        text.push_str(chunk.valid());
    }
    text
}

struct Walk {
    stem: String,
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    pairs: usize,
}

impl Walk {
    fn node(&mut self, id: &str, label: &str) {
        if !id.is_empty() && self.seen.insert(id.to_owned()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":"L1"}));
        }
    }
    fn edge(&mut self, source: &str, target: &str) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        self.edges.push(json!({"source":source,"target":target,"relation":"contains","confidence":"EXTRACTED","source_file":self.path,"source_location":"L1","weight":1.0}));
    }
    /// `walk`: keys become nodes under their parent; a list's structured items
    /// are walked under the list's own parent, with no index nodes. Once the
    /// pair budget is spent every mapping returns at its first key.
    fn walk(&mut self, value: &Loaded, parent: &str, parent_key: Option<&str>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match value {
            Loaded::Map(entries) => {
                for (key, child) in entries {
                    if self.pairs >= MAX_PAIRS {
                        return;
                    }
                    self.pairs += 1;
                    let key = key_text(key);
                    if normalize_id(&key).is_empty() {
                        continue;
                    }
                    let nid = match parent_key.filter(|key| !key.is_empty()) {
                        Some(parent_key) => make_id([self.stem.as_str(), parent_key, key.as_str()]),
                        None => make_id([self.stem.as_str(), key.as_str()]),
                    };
                    if nid.is_empty() {
                        continue;
                    }
                    self.node(&nid, &key);
                    self.edge(parent, &nid);
                    if structured(child) {
                        self.walk(child, &nid, Some(&key), depth + 1);
                    }
                }
            }
            Loaded::Seq(items) => {
                for item in items {
                    if structured(item) {
                        self.walk(item, parent, parent_key, depth + 1);
                    }
                }
            }
            Loaded::Other(_) => {}
        }
    }
}

/// `{"nodes": [], "edges": [], key: value}`.
fn empty(key: &str, value: impl Into<Value>) -> Value {
    let mut out = json!({"nodes":[],"edges":[]});
    out[key] = value.into();
    out
}

/// `extract_yaml`: structural nodes and edges of a `.yaml`/`.yml` file.
pub fn extract_yaml(path: &Path) -> Value {
    let mut raw = Vec::new();
    let read = std::fs::File::open(path)
        .and_then(|file| file.take((YAML_MAX_BYTES + 1) as u64).read_to_end(&mut raw));
    if let Err(error) = read {
        return empty("error", io_compat::read_error(&error, path));
    }
    if raw.len() > YAML_MAX_BYTES {
        return empty("error", "yaml file too large to index");
    }
    let text = decode_ignoring_errors(&raw);
    if let Some(c) = non_printable(&text) {
        return empty(
            "error",
            format!(
                "unacceptable character #x{:04x}: special characters are not allowed",
                c as u32
            ),
        );
    }
    if let Some(line) = leading_tab_line(&text) {
        return empty(
            "error",
            format!("found character '\\t' that cannot start any token (line {line})"),
        );
    }
    // PyYAML's scanner skips a byte-order mark at the very start of the stream.
    let docs = match yaml_frontmatter::parse_all(text.strip_prefix('\u{feff}').unwrap_or(&text)) {
        Ok(docs) => docs,
        Err(error) => return empty("error", error.to_string()),
    };
    let structured_docs = docs
        .iter()
        .filter(|doc| structured(doc))
        .collect::<Vec<_>>();
    if structured_docs.is_empty() {
        return empty("skipped", "scalar yaml");
    }
    let str_path = fsutil::path_string(path);
    let parts = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&parts).to_owned();
    let config_docs = structured_docs
        .into_iter()
        .filter(|doc| is_config_yaml(&str_path, &name, doc))
        .collect::<Vec<_>>();
    if config_docs.is_empty() {
        return empty("skipped", "data yaml (not a config/manifest)");
    }
    let mut walk = Walk {
        stem: file_stem(path),
        path: str_path.clone(),
        nodes: Vec::new(),
        edges: Vec::new(),
        seen: HashSet::new(),
        pairs: 0,
    };
    let file_nid = make_id([str_path.as_str()]);
    walk.node(&file_nid, &name);
    let multi = config_docs.len() > 1;
    for (index, doc) in config_docs.into_iter().enumerate() {
        if multi {
            // Each document of a stream stays distinct: a Deployment and a
            // Service in one file must not merge their metadata.
            let label = doc_label(doc, index);
            let numbered = format!("doc{index}");
            let mut doc_nid = make_id([walk.stem.as_str(), numbered.as_str(), label.as_str()]);
            if doc_nid.is_empty() {
                doc_nid = make_id([walk.stem.as_str(), numbered.as_str()]);
            }
            if doc_nid.is_empty() {
                continue;
            }
            walk.node(&doc_nid, &label);
            walk.edge(&file_nid, &doc_nid);
            walk.walk(doc, &doc_nid, Some(&label), 1);
        } else {
            walk.walk(doc, &file_nid, None, 0);
        }
    }
    json!({"nodes":walk.nodes,"edges":walk.edges})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(name: &str, text: &str) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        extract_yaml(&path)
    }

    fn labels(result: &Value) -> Vec<String> {
        result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["label"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn keys_become_contained_nodes() {
        let result = extract("values.yaml", "replicaCount: 2\nimage:\n  tag: latest\n");
        assert_eq!(
            labels(&result),
            ["values.yaml", "replicaCount", "image", "tag"]
        );
        let edges = result["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 3);
        assert!(edges.iter().all(|edge| edge["relation"] == "contains"));
        assert!(edges[2]["source"].as_str().unwrap().ends_with("image"));
    }

    #[test]
    fn multi_document_streams_label_each_document() {
        let result = extract(
            "manifests.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web-svc\n",
        );
        let labels = labels(&result);
        assert!(labels.contains(&"Deployment web".to_owned()));
        assert!(labels.contains(&"Service web-svc".to_owned()));
    }

    #[test]
    fn data_scalar_and_unsafe_yaml_extract_nothing() {
        assert_eq!(
            extract("scalar.yaml", "just-a-string\n")["skipped"],
            "scalar yaml"
        );
        assert_eq!(extract("empty.yaml", "")["skipped"], "scalar yaml");
        for (name, text) in [
            ("strings.yaml", "greeting: hello\nfarewell: goodbye\n"),
            ("api.yaml", "openapi: 3.0.0\npaths:\n  /x: {}\n"),
            ("list.yaml", "- one\n- two\n"),
        ] {
            assert_eq!(
                extract(name, text)["skipped"],
                "data yaml (not a config/manifest)",
                "{name}"
            );
        }
        let unsafe_tag = extract(
            "evil.yaml",
            "danger: !!python/object/apply:os.system ['true']\n",
        );
        assert!(unsafe_tag.get("error").is_some());
        assert_eq!(unsafe_tag["nodes"], json!([]));
    }

    #[test]
    fn yaml_1_1_keys_read_as_pyyaml_reads_them() {
        // `on` is the boolean True in YAML 1.1 (a GitHub workflow's trigger key).
        let result = extract("ci.yml", "on:\n  push: {}\njobs:\n  build: {}\n");
        assert_eq!(labels(&result), ["ci.yml", "True", "push", "jobs", "build"]);
        // `1` and `"1"` are two keys; `1` and `true` are one.
        let result = extract("values.yaml", "1: a\n'1': b\ntrue: c\nx: {y: 1}\n");
        assert_eq!(labels(&result), ["values.yaml", "1", "x", "y"]);
        assert_eq!(result["edges"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn non_finite_floats_load_as_pyyaml_loads_them() {
        // JSON holds no infinity, yet PyYAML loads `.inf` and `.nan`; as keys
        // they read as Python's `str()` of the float, apart from the string
        // `'inf'`, whose node they share.
        let result = extract(
            "values.yaml",
            "limits:\n  - .inf\n.inf: x\n.nan: z\n'inf': w\nnested:\n  -.Inf: y\n",
        );
        assert_eq!(
            labels(&result),
            ["values.yaml", "limits", "inf", "nan", "nested", "-inf"]
        );
        assert_eq!(result["edges"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn the_pair_cap_bounds_output() {
        let body = format!(
            "root:\n{}",
            (0..2000)
                .map(|i| format!("  k{i}: {i}\n"))
                .collect::<String>()
        );
        let result = extract("values.yaml", &body);
        assert_eq!(result["nodes"].as_array().unwrap().len(), 501);
    }
}
