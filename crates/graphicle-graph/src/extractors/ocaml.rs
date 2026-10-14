// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/ocaml.py
//! Complete translation of the OCaml implementation/interface extractor.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
/// The `object ... end` body of a class binding: an `object_expression` in an
/// implementation, a `class_body_type` in an interface. A parametric class
/// nests it deeper, so descendants are searched in upstream's stack order.
fn object_body(binding: Node<'_>) -> Option<Node<'_>> {
    let mut stack: Vec<Node<'_>> = children(binding).collect();
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "object_expression" | "class_body_type") {
            return Some(node);
        }
        stack.extend(children(node));
    }
    None
}
struct Call {
    caller: String,
    callee: String,
    qualifier: Option<String>,
    full: String,
    line: usize,
}
struct Extractor<'source> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    definitions: HashMap<String, String>,
    ambiguous: HashSet<String>,
    modules: HashSet<String>,
    calls: Vec<Call>,
}
impl Extractor<'_> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize, inferred: bool) {
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":if inferred{"INFERRED"}else{"EXTRACTED"},"source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn ref_stub(&mut self, name: &str) -> String {
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn named_text(&self, node: Node<'_>, kind: &str) -> Option<String> {
        children(node)
            .find(|n| n.kind() == kind)
            .map(|n| read_text(n, self.source))
    }
    fn last_name(&self, node: Node<'_>) -> Option<String> {
        children(node)
            .filter(|n| matches!(n.kind(), "value_name" | "module_name" | "constructor_name"))
            .last()
            .map(|n| read_text(n, self.source))
    }
    fn path_root_module(&self, node: Node<'_>) -> Option<String> {
        let mut module = children(node).find(|n| n.kind() == "module_path")?;
        while let Some(inner) = children(module).find(|n| n.kind() == "module_path") {
            module = inner;
        }
        self.named_text(module, "module_name")
    }
    fn register(&mut self, name: &str, id: &str) {
        if self.ambiguous.contains(name) {
            return;
        }
        if self.definitions.get(name).is_some_and(|v| v != id) {
            self.ambiguous.insert(name.into());
            self.definitions.remove(name);
            return;
        }
        self.definitions.insert(name.into(), id.into());
    }
    fn define(&mut self, container: &str, id: &str, name: &str, line: usize) {
        self.node(id, name, line);
        let relation = if container == self.file {
            "defines"
        } else {
            "contains"
        };
        self.edge(container, id, relation, line, false);
        self.register(name, id);
    }
    fn emit_type(&mut self, binding: Node<'_>, container: &str) {
        let Some(name) = self
            .named_text(binding, "type_constructor")
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let id = make_id([self.stem.as_str(), name.as_str()]);
        self.define(container, &id, &name, binding.start_position().row + 1);
        for variant in children(binding).filter(|n| n.kind() == "variant_declaration") {
            for constructor in children(variant).filter(|n| n.kind() == "constructor_declaration") {
                if let Some(cname) = self
                    .named_text(constructor, "constructor_name")
                    .filter(|s| !s.is_empty())
                {
                    let cid = make_id([self.stem.as_str(), name.as_str(), cname.as_str()]);
                    let line = constructor.start_position().row + 1;
                    self.node(&cid, &cname, line);
                    self.edge(&id, &cid, "contains", line, false);
                }
            }
        }
    }
    fn walk(&mut self, node: Node<'_>, container: &str, enclosing: &str) {
        let line = node.start_position().row + 1;
        if node.kind() == "open_module" {
            if let Some(name) = children(node)
                .find(|n| n.kind() == "module_path")
                .and_then(|n| self.last_name(n))
                .filter(|s| !s.is_empty())
            {
                let target = self.ref_stub(&name);
                self.edge(container, &target, "imports_from", line, true);
            }
            return;
        }
        if node.kind() == "module_definition"
            && let Some(binding) = children(node).find(|n| n.kind() == "module_binding")
            && let Some(name) = self
                .named_text(binding, "module_name")
                .filter(|s| !s.is_empty())
        {
            let id = make_id([self.stem.as_str(), name.as_str()]);
            self.define(container, &id, &name, line);
            self.modules.insert(name);
            for child in children(binding) {
                self.walk(child, &id, enclosing);
            }
            return;
        }
        if node.kind() == "module_type_definition"
            && let Some(name) = self
                .named_text(node, "module_type_name")
                .filter(|s| !s.is_empty())
        {
            let id = make_id([self.stem.as_str(), name.as_str()]);
            self.define(container, &id, &name, line);
            self.modules.insert(name);
            for child in children(node) {
                self.walk(child, &id, enclosing);
            }
            return;
        }
        if node.kind() == "value_definition" {
            let top = node
                .parent()
                .is_some_and(|n| matches!(n.kind(), "compilation_unit" | "structure"));
            for binding in children(node).filter(|n| n.kind() == "let_binding") {
                let mut scope = enclosing.to_string();
                if let Some(name) = self
                    .named_text(binding, "value_name")
                    .filter(|s| !s.is_empty() && top)
                {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.define(container, &id, &name, binding.start_position().row + 1);
                    scope = id;
                }
                for child in children(binding) {
                    self.walk(child, container, &scope);
                }
            }
            return;
        }
        if node.kind() == "value_specification" {
            if let Some(name) = self
                .named_text(node, "value_name")
                .filter(|s| !s.is_empty())
            {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.define(container, &id, &name, line);
            }
            return;
        }
        if node.kind() == "type_definition" {
            for binding in children(node).filter(|n| n.kind() == "type_binding") {
                self.emit_type(binding, container);
            }
            return;
        }
        // #3838: `class c = object ... end` (.ml) and `class c : object ... end`
        // (.mli) define the class, its `method` members and instance variables.
        if node.kind() == "class_definition"
            && let Some(binding) = children(node).find(|n| n.kind() == "class_binding")
            && let Some(class) = self
                .named_text(binding, "class_name")
                .filter(|s| !s.is_empty())
        {
            let id = make_id([self.stem.as_str(), class.as_str()]);
            self.define(container, &id, &class, line);
            if let Some(body) = object_body(binding) {
                for member in children(body) {
                    match member.kind() {
                        "method_definition" | "method_specification" => {
                            let Some(method) = self
                                .named_text(member, "method_name")
                                .filter(|s| !s.is_empty())
                            else {
                                continue;
                            };
                            let member_line = member.start_position().row + 1;
                            let method_id =
                                make_id([self.stem.as_str(), class.as_str(), method.as_str()]);
                            self.node(&method_id, &method, member_line);
                            self.edge(&id, &method_id, "method", member_line, false);
                            // Methods are only reachable as `obj#m`, so they are
                            // not registered as bare local definitions.
                            for child in children(member) {
                                self.walk(child, &id, &method_id);
                            }
                        }
                        "instance_variable_definition" | "instance_variable_specification" => {
                            let Some(variable) = self
                                .named_text(member, "instance_variable_name")
                                .filter(|s| !s.is_empty())
                            else {
                                continue;
                            };
                            let member_line = member.start_position().row + 1;
                            let variable_id =
                                make_id([self.stem.as_str(), class.as_str(), variable.as_str()]);
                            self.node(&variable_id, &variable, member_line);
                            self.edge(&id, &variable_id, "contains", member_line, false);
                        }
                        _ => (),
                    }
                }
            }
            return;
        }
        if node.kind() == "application_expression"
            && let Some(function) = node.named_child(0).filter(|n| n.kind() == "value_path")
            && let Some(callee) = self.last_name(function).filter(|s| !s.is_empty())
        {
            self.calls.push(Call {
                caller: if enclosing.is_empty() {
                    self.file.clone()
                } else {
                    enclosing.into()
                },
                callee,
                qualifier: self.path_root_module(function),
                full: read_text(function, self.source),
                line,
            });
        }
        for child in children(node) {
            self.walk(child, container, enclosing);
        }
    }
    fn resolve_calls(&mut self) {
        for call in std::mem::take(&mut self.calls) {
            let external = call
                .qualifier
                .as_ref()
                .is_some_and(|q| !self.modules.contains(q));
            let (target, inferred) = if external {
                let name = if self.definitions.contains_key(&call.callee) {
                    &call.full
                } else {
                    &call.callee
                };
                (self.ref_stub(name), true)
            } else if let Some(target) = self.definitions.get(&call.callee) {
                (target.clone(), false)
            } else {
                (self.ref_stub(&call.callee), true)
            };
            self.edge(&call.caller, &target, "calls", call.line, inferred);
        }
    }
}
pub fn extract_ocaml(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    // The suffix after `.graphifyrc` language overrides (#2961).
    let language = if crate::extract_languages::effective_suffix(path) == ".mli" {
        crate::grammar!(
            "grammar-ocaml",
            "tree_sitter_ocaml_interface",
            tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE
        )
    } else {
        crate::grammar!(
            "grammar-ocaml",
            "tree_sitter_ocaml",
            tree_sitter_ocaml::LANGUAGE_OCAML
        )
    };
    let language = match language {
        Ok(language) => language,
        Err(error) => return json!({"nodes":[],"edges":[],"error":error}),
    };
    if let Err(error) = parser.set_language(&language) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"OCaml parse cancelled"});
    };
    let text = fsutil::path_string(path);
    let parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts);
    let file = make_id([text.as_str()]);
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem: file_stem(path),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        definitions: HashMap::new(),
        ambiguous: HashSet::new(),
        modules: HashSet::new(),
        calls: vec![],
    };
    ex.node(&file, name, 1);
    ex.walk(tree.root_node(), &file, "");
    ex.resolve_calls();
    json!({"nodes":ex.nodes,"edges":ex.edges})
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-ocaml")]
    fn grammar_matches_pinned_reference() {
        for (language, nodes, states) in [
            (tree_sitter_ocaml::LANGUAGE_OCAML, 470, 8356),
            (tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE, 469, 7337),
        ] {
            let l: tree_sitter::Language = language.into();
            assert_eq!(l.node_kind_count(), nodes);
            assert_eq!(l.parse_state_count(), states);
            assert_eq!(l.abi_version(), 15);
        }
    }
}
