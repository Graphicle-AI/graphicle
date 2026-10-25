// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/zig.py
//! Complete translation of the pinned Graphify Zig tree-sitter extractor.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};
use tree_sitter::Node;

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
struct Extractor<'tree, 'source> {
    source: &'source [u8],
    path: String,
    stem: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
    call_pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
}
impl<'tree> Extractor<'tree, '_> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn extract_import(&mut self, node: Node<'tree>) {
        for child in children(node) {
            if child.kind() == "builtin_function" {
                let mut builtin = None;
                let mut args = None;
                for part in children(child) {
                    match part.kind() {
                        "builtin_identifier" => builtin = Some(read_text(part, self.source)),
                        "arguments" => args = Some(part),
                        _ => (),
                    }
                }
                if matches!(builtin.as_deref(), Some("@import" | "@cImport"))
                    && let Some(args) = args
                {
                    for arg in children(args) {
                        if matches!(arg.kind(), "string_literal" | "string") {
                            let raw = read_text(arg, self.source);
                            let raw = raw.trim_matches('"');
                            let module = raw
                                .rsplit('/')
                                .next()
                                .unwrap_or("")
                                .split('.')
                                .next()
                                .unwrap_or("");
                            if !module.is_empty() {
                                let id = make_id([module]);
                                self.edge(
                                    &self.file_id.clone(),
                                    &id,
                                    "imports_from",
                                    node.start_position().row + 1,
                                );
                            }
                            return;
                        }
                    }
                }
            } else if child.kind() == "field_expression" {
                self.extract_import(child);
                return;
            }
        }
    }
    fn walk(&mut self, node: Node<'tree>, owner: Option<&str>) {
        // Enum members (`red`, `north = 0`) are `container_field`s directly under the
        // enum_declaration. A tagged union (`union(enum) { circle: f64, point }`)
        // has the same shape and its fields are the discriminant cases (0.9.75);
        // a bare struct/union's typed data fields share it and stay untouched.
        if node.kind() == "container_field"
            && let Some(owner) = owner.filter(|s| !s.is_empty())
            && node
                .parent()
                .is_some_and(|p| p.kind() == "enum_declaration" || is_tagged_union(p))
        {
            let name = node
                .child_by_field_name("name")
                .or_else(|| children(node).find(|c| c.kind() == "identifier"));
            if let Some(name) = name {
                let name = read_text(name, self.source);
                if !name.is_empty() {
                    let line = node.start_position().row + 1;
                    let id = make_id([owner, name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(owner, &id, "case_of", line);
                }
            }
            return;
        }
        if node.kind() == "function_declaration" {
            if let Some(name) = node.child_by_field_name("name") {
                let name = read_text(name, self.source);
                let line = node.start_position().row + 1;
                let id = if let Some(owner) = owner {
                    let id = make_id([owner, name.as_str()]);
                    self.node(&id, &format!(".{name}()"), line);
                    self.edge(owner, &id, "method", line);
                    id
                } else {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file_id.clone(), &id, "contains", line);
                    id
                };
                if let Some(body) = node.child_by_field_name("body") {
                    self.bodies.push((id, body));
                }
            }
            return;
        }
        if node.kind() == "variable_declaration" {
            let mut name = None;
            let mut value = None;
            for child in children(node) {
                match child.kind() {
                    "identifier" => name = Some(child),
                    "struct_declaration"
                    | "enum_declaration"
                    | "union_declaration"
                    | "error_set_declaration"
                    | "builtin_function"
                    | "field_expression" => value = Some(child),
                    _ => (),
                }
            }
            if let Some(value) = value {
                if matches!(
                    value.kind(),
                    "struct_declaration" | "enum_declaration" | "union_declaration"
                ) {
                    if let Some(name) = name {
                        let name = read_text(name, self.source);
                        let line = node.start_position().row + 1;
                        let id = make_id([self.stem.as_str(), name.as_str()]);
                        self.node(&id, &name, line);
                        self.edge(&self.file_id.clone(), &id, "contains", line);
                        for child in children(value) {
                            self.walk(child, Some(&id));
                        }
                    }
                    return;
                }
                if value.kind() == "error_set_declaration" {
                    // `const E = error{ A, B };` names an enumeration of error
                    // values: a node for the set plus a `case_of` edge per
                    // `identifier` member (0.9.75).
                    if let Some(name) = name {
                        let name = read_text(name, self.source);
                        let line = node.start_position().row + 1;
                        let id = make_id([self.stem.as_str(), name.as_str()]);
                        self.node(&id, &name, line);
                        self.edge(&self.file_id.clone(), &id, "contains", line);
                        for child in children(value).filter(|c| c.kind() == "identifier") {
                            let member = read_text(child, self.source);
                            if member.is_empty() {
                                continue;
                            }
                            let member_line = child.start_position().row + 1;
                            let member_id = make_id([id.as_str(), member.as_str()]);
                            self.node(&member_id, &member, member_line);
                            self.edge(&id, &member_id, "case_of", member_line);
                        }
                    }
                    return;
                }
                if matches!(value.kind(), "builtin_function" | "field_expression") {
                    self.extract_import(node);
                }
            }
            return;
        }
        for child in children(node) {
            self.walk(child, owner);
        }
    }
    fn walk_calls(&mut self, node: Node<'tree>, caller: &str) {
        if node.kind() == "function_declaration" {
            return;
        }
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
        {
            let name = read_text(function, self.source);
            let callee = name.rsplit('.').next().unwrap_or("");
            let member = name.contains('.');
            let labels = [format!("{callee}()"), format!(".{callee}()")];
            let target = self
                .nodes
                .iter()
                .find(|n| {
                    n["label"]
                        .as_str()
                        .is_some_and(|s| labels.iter().any(|label| s == label))
                })
                .and_then(|n| n["id"].as_str())
                .map(str::to_string);
            if let Some(target) = target.filter(|id| !id.is_empty() && id != caller) {
                if self.call_pairs.insert((caller.into(), target.clone())) {
                    self.edge(caller, &target, "calls", node.start_position().row + 1);
                }
            } else if !callee.is_empty() {
                self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
            }
        }
        for child in children(node) {
            self.walk_calls(child, caller);
        }
    }
}
pub fn extract_zig(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&match crate::grammar!(
        "grammar-zig",
        "tree_sitter_zig",
        tree_sitter_zig::LANGUAGE
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Zig parse cancelled"});
    };
    let path_text = fsutil::path_string(path);
    let id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let name = path_syntax::posix_name(&parts);
    let mut extractor = Extractor {
        source: &source,
        path: path_text,
        stem: file_stem(path),
        file_id: id.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        call_pairs: HashSet::new(),
        raw_calls: vec![],
    };
    extractor.node(&id, name, 1);
    extractor.walk(tree.root_node(), None);
    for (caller, body) in extractor.bodies.clone() {
        extractor.walk_calls(body, &caller);
    }
    extractor.edges.retain(|e| {
        extractor.seen.contains(e["source"].as_str().unwrap_or(""))
            && (extractor.seen.contains(e["target"].as_str().unwrap_or(""))
                || e["relation"] == "imports_from")
    });
    json!({"nodes":extractor.nodes,"edges":extractor.edges,"raw_calls":extractor.raw_calls})
}

/// A tagged union (`union(enum)` / `union(Tag)`) carries its tag in
/// parentheses, so it has a `(` child; a bare `union { ... }` has none.
fn is_tagged_union(node: Node<'_>) -> bool {
    node.kind() == "union_declaration" && children(node).any(|c| c.kind() == "(")
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-zig")]
    fn grammar_matches_the_pinned_python_package() {
        let language: tree_sitter::Language = tree_sitter_zig::LANGUAGE.into();
        assert_eq!(language.node_kind_count(), 274);
        assert_eq!(language.parse_state_count(), 1852);
        assert_eq!(language.abi_version(), 14);
    }
}
