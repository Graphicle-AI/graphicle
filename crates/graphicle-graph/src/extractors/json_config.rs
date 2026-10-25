// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/json_config.py
//! Port of graphify/extractors/json_config.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, ids::normalize_id, io_compat};
use serde_json::{Value, json};
use std::{collections::HashSet, io::Read, path::Path};
use tree_sitter::Node;

pub const CONFIG_JSON_NAMES: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "composer.json",
    "deno.json",
    "deno.jsonc",
    "bower.json",
    "manifest.json",
    "app.json",
    "now.json",
    "vercel.json",
    "angular.json",
    "nest-cli.json",
    "biome.json",
    "biome.jsonc",
    "renovate.json",
    ".babelrc",
    ".babelrc.json",
    ".eslintrc.json",
    ".prettierrc.json",
    ".prettierrc",
    "babel.config.json",
];
pub const DEP_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
];
pub const CONFIG_JSON_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
    "extends",
    "$schema",
    "compilerOptions",
];
/// A root `$schema` plus one of these marks a JSON Schema document, never a
/// config that merely points at its tool's schema (Graphify 0.9.76, #2255).
pub const SCHEMA_DEFINITION_KEYS: &[&str] = &["$defs", "definitions", "$id"];
pub const JSON_MAX_BYTES: usize = 1_048_576;

/// Python's `str.isspace`: Unicode whitespace plus the information separators
/// U+001C..U+001F, which `char::is_whitespace` leaves out.
fn python_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |index| node.child(index))
}

fn string_text(node: Node<'_>, source: &[u8]) -> String {
    node.child_by_field_name("string_content")
        .map(|node| read_text(node, source))
        .unwrap_or_else(|| read_text(node, source).trim_matches(['\'', '"']).to_owned())
}

fn key_text(pair: Node<'_>, source: &[u8]) -> Option<String> {
    pair.child_by_field_name("key").map(|key| {
        if key.kind() == "string" {
            string_text(key, source)
        } else {
            read_text(key, source)
        }
    })
}

pub fn is_config_json(path: &Path, object: Node<'_>, source: &[u8]) -> bool {
    let parts = crate::path_syntax::posix_parts(&fsutil::path_string(path));
    let name = crate::path_syntax::posix_name(&parts);
    let mut folded = String::new();
    for c in name.chars() {
        match crate::unicode15::CASEFOLD.binary_search_by_key(&c, |entry| entry.0) {
            Ok(index) => folded.push_str(crate::unicode15::CASEFOLD[index].1),
            Err(_) => folded.push(c),
        }
    }
    if CONFIG_JSON_NAMES.contains(&folded.as_str())
        || [
            ".eslintrc.json",
            ".prettierrc.json",
            ".babelrc.json",
            "tsconfig.json",
            "jsconfig.json",
        ]
        .iter()
        .any(|suffix| folded.ends_with(suffix))
    {
        return true;
    }
    let root_keys = children(object)
        .filter(|node| node.kind() == "pair")
        .filter_map(|node| node.child_by_field_name("key"))
        .map(|key| string_text(key, source))
        .collect::<HashSet<_>>();
    if root_keys.contains("$schema")
        && SCHEMA_DEFINITION_KEYS
            .iter()
            .any(|key| root_keys.contains(*key))
    {
        return false;
    }
    CONFIG_JSON_KEYS.iter().any(|key| root_keys.contains(*key))
}

struct Extractor<'a> {
    source: &'a [u8],
    path: String,
    stem: String,
    file_nid: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    pairs: usize,
}

impl Extractor<'_> {
    fn node(&mut self, id: &str, label: &str, line: usize, kind: &str) {
        if !id.is_empty() && self.seen.insert(id.to_owned()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":kind,"source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn object(&mut self, object: Node<'_>, parent: &str, parent_key: Option<&str>, depth: usize) {
        if depth > 6 {
            return;
        }
        for pair in children(object).filter(|node| node.kind() == "pair") {
            if self.pairs >= 500 {
                return;
            }
            self.pairs += 1;
            let Some(key) = key_text(pair, self.source)
                .filter(|key| !key.is_empty() && !normalize_id(key).is_empty())
            else {
                continue;
            };
            let nid = if let Some(parent_key) = parent_key.filter(|key| !key.is_empty()) {
                make_id([self.stem.as_str(), parent_key, key.as_str()])
            } else {
                make_id([self.stem.as_str(), key.as_str()])
            };
            if nid.is_empty() {
                continue;
            }
            let line = pair.start_position().row + 1;
            self.node(&nid, &key, line, "code");
            self.edge(parent, &nid, "contains", line, None);
            let Some(value) = pair.child_by_field_name("value") else {
                continue;
            };
            if value.kind() == "object" {
                self.object(value, &nid, Some(&key), depth + 1);
            } else if value.kind() == "array" && key == "extends" {
                for item in children(value).filter(|node| node.kind() == "string") {
                    let reference = string_text(item, self.source);
                    if reference.is_empty() {
                        continue;
                    }
                    let reference_id = make_id(["ref", reference.as_str()]);
                    if !reference_id.is_empty() {
                        self.node(&reference_id, &reference, line, "concept");
                        self.edge(&nid, &reference_id, "extends", line, Some("import"));
                    }
                }
            } else if value.kind() == "string" {
                let text = string_text(value, self.source);
                if text.is_empty() {
                    continue;
                }
                if key == "extends" {
                    let reference_id = make_id(["ref", text.as_str()]);
                    if !reference_id.is_empty() {
                        self.node(&reference_id, &text, line, "concept");
                        self.edge(
                            &self.file_nid.clone(),
                            &reference_id,
                            "extends",
                            line,
                            Some("import"),
                        );
                    }
                } else if key == "$ref" {
                    let reference_id = make_id(["ref", text.as_str()]);
                    if !reference_id.is_empty() {
                        self.edge(parent, &reference_id, "references", line, None);
                    }
                } else if parent_key.is_some_and(|key| DEP_KEYS.contains(&key))
                    && !text.trim_matches(python_space).is_empty()
                {
                    let dependency_id = make_id(["ref", key.as_str()]);
                    if !dependency_id.is_empty() {
                        self.node(&dependency_id, &key, line, "concept");
                        self.edge(
                            &self.file_nid.clone(),
                            &dependency_id,
                            "imports",
                            line,
                            Some("import"),
                        );
                        // 0.9.95 (#4569): the manifest entry declares the ref node
                        // importing code resolves to, and is relabelled
                        // `name@range` (one line, at most 40 characters) so the
                        // two nodes no longer share a label. `node` kept the
                        // first node under this id; only the entry on this line
                        // is relabelled.
                        self.edge(&nid, &dependency_id, "declares", line, None);
                        let mut range = text
                            .split(python_space)
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                        if range.chars().count() > 40 {
                            range = range.chars().take(37).collect::<String>() + "...";
                        }
                        let location = format!("L{line}");
                        if let Some(entry) = self.nodes.iter_mut().find(|node| {
                            node["id"] == nid.as_str()
                                && node["source_location"] == location.as_str()
                        }) {
                            entry["label"] = json!(format!("{key}@{range}"));
                        }
                    }
                }
            }
        }
    }
}

pub fn extract_json(path: &Path) -> Value {
    let mut source = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        file.take((JSON_MAX_BYTES + 1) as u64)
            .read_to_end(&mut source)
    });
    if let Err(error) = read {
        return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
    }
    if source.len() > JSON_MAX_BYTES {
        return json!({"nodes":[],"edges":[],"error":"json file too large to index"});
    }
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&tree_sitter_json::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"JSON parse cancelled"});
    };
    let root = tree.root_node();
    let object = if root.kind() == "document" && root.child_count() > 0 {
        root.child(0).unwrap()
    } else {
        root
    };
    if object.kind() != "object" {
        return json!({"nodes":[],"edges":[],"skipped":"data json (non-object root)"});
    }
    if !is_config_json(path, object, &source) {
        return json!({"nodes":[],"edges":[],"skipped":"data json (not a config/manifest)"});
    }
    let str_path = fsutil::path_string(path);
    let file_nid = make_id([str_path.as_str()]);
    let parts = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&parts);
    let mut extractor = Extractor {
        source: &source,
        path: str_path,
        stem: file_stem(path),
        file_nid: file_nid.clone(),
        nodes: Vec::new(),
        edges: Vec::new(),
        seen: HashSet::new(),
        pairs: 0,
    };
    extractor.node(&file_nid, name, 1, "code");
    extractor.object(object, &file_nid, None, 0);
    json!({"nodes":extractor.nodes,"edges":extractor.edges})
}
