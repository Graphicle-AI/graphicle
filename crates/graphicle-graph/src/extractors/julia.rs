// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/julia.py
//! Complete translation of Graphify's Julia tree-sitter extractor.
use super::{
    base::{file_stem, make_id, read_text},
    engine_semantics::semantic_reference_edge,
};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};
use tree_sitter::Node;
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn last_child(node: Node<'_>) -> Option<Node<'_>> {
    node.child(node.child_count().checked_sub(1)?)
}
/// `_unwrap_call_expression`: the `call_expression` at or under `node`
/// through any `where_expression` (`f(x::T) where {T}`) and `typed_expression`
/// (`f(x)::Float64`) wrappers around the call head (#4126, #4310).
fn unwrap_call(mut node: Option<Node<'_>>) -> Option<Node<'_>> {
    while let Some(n) = node.filter(|n| matches!(n.kind(), "where_expression" | "typed_expression"))
    {
        node = n.child(0);
    }
    node.filter(|n| n.kind() == "call_expression")
}
struct Extractor<'tree, 'source> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
}
impl<'tree> Extractor<'tree, '_> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
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
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context {
            edge["context"] = c.into();
        }
        self.edges.push(edge);
    }
    fn ensure_named(&mut self, name: &str) -> String {
        let id = make_id([self.stem.as_str(), name]);
        if self.seen.contains(&id) {
            return id;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    /// `_type_name_of`: a type name, unwrapping a parametric head (`Box{T}`).
    fn type_name(&self, node: Option<Node<'tree>>) -> Option<String> {
        let node = node?;
        match node.kind() {
            "identifier" => Some(read_text(node, self.source)),
            "parametrized_type_expression" => children(node)
                .find(|n| n.kind() == "identifier")
                .map(|n| read_text(n, self.source)),
            _ => None,
        }
    }
    /// `(name, supertype)` of a `type_head`: `Foo`, `Foo <: Bar`, and their
    /// parametric forms (`Box{T}`, `Sq{T} <: Shape{T}`, #4126).
    fn type_head(&self, node: Node<'tree>) -> (Option<String>, Option<String>) {
        let operand =
            |n: &Node<'_>| matches!(n.kind(), "identifier" | "parametrized_type_expression");
        if let Some(binary) = children(node).find(|n| n.kind() == "binary_expression") {
            let operands = children(binary).filter(operand).collect::<Vec<_>>();
            let Some(first) = operands.first() else {
                return (None, None);
            };
            let parent = if operands.len() >= 2 {
                self.type_name(operands.last().copied())
            } else {
                None
            };
            return (self.type_name(Some(*first)), parent);
        }
        (self.type_name(children(node).find(operand)), None)
    }
    /// `_type_head_params`: the type parameters a parametric head declares
    /// (`{T}`, `{T,U}`, `{T<:Number}`), which are not types to reference.
    fn type_head_params(&self, node: Node<'tree>) -> HashSet<String> {
        let mut head = None;
        for c in children(node) {
            if c.kind() == "parametrized_type_expression" {
                head = Some(c);
                break;
            }
            if c.kind() == "binary_expression" {
                head = children(c).find(|n| n.kind() == "parametrized_type_expression");
                break;
            }
        }
        let mut params = HashSet::new();
        let Some(curly) = head.and_then(|h| children(h).find(|n| n.kind() == "curly_expression"))
        else {
            return params;
        };
        for c in children(curly) {
            if c.kind() == "identifier" {
                params.insert(read_text(c, self.source));
            } else if c.kind() == "binary_expression"
                && let Some(first) = children(c).find(|n| n.kind() == "identifier")
            {
                // A constrained parameter `T <: Number`.
                params.insert(read_text(first, self.source));
            }
        }
        params
    }
    /// `_functor_type_name`: the bare type of a functor's receiver annotation
    /// (`Box`, `Box{T}`, `Mod.Box`, `Mod.Box{T}`, #4351).
    fn functor_type_name(&self, mut node: Option<Node<'tree>>) -> Option<String> {
        while let Some(n) = node {
            match n.kind() {
                "identifier" => return Some(read_text(n, self.source)),
                "parametrized_type_expression" => node = n.child(0),
                "field_expression" => node = last_child(n),
                _ => return None,
            }
        }
        None
    }
    /// `_def_name`: `(bare name, label)` for the callee of a method
    /// definition: `foo`, `+`, `Base.show`, `Base.:+` and functors
    /// `(c::Circle)(x)` (#4310). The id uses the bare name, so the methods of
    /// one generic function share a node; the label keeps the qualifier.
    fn def_name(&self, call: Option<Node<'tree>>) -> Option<(String, String)> {
        let callee = call?.child(0)?;
        match callee.kind() {
            "identifier" | "operator" => {
                let name = read_text(callee, self.source);
                Some((name.clone(), name))
            }
            "field_expression" => {
                let mut last = last_child(callee)?;
                if last.kind() == "quote_expression" {
                    // `Base.:+`
                    last =
                        children(last).find(|n| matches!(n.kind(), "operator" | "identifier"))?;
                }
                matches!(last.kind(), "identifier" | "operator")
                    .then(|| (read_text(last, self.source), read_text(callee, self.source)))
            }
            "parenthesized_expression" => {
                // A functor `(c::Circle)(x)` / `(::Circle)(x)`: the type is the
                // annotation's last child, never another identifier (in
                // `(b::Mod.Box)` that would be the variable `b`).
                let typed = children(callee)
                    .find(|n| matches!(n.kind(), "typed_expression" | "unary_typed_expression"))?;
                let name = self.functor_type_name(last_child(typed))?;
                Some((format!("{name}#call"), format!("({name})")))
            }
            _ => None,
        }
    }
    /// `_func_name_from_signature`: the first child that unwraps to a call
    /// head decides.
    fn function_name(&self, node: Node<'tree>) -> Option<(String, String)> {
        children(node)
            .find_map(|child| unwrap_call(Some(child)))
            .and_then(|call| self.def_name(Some(call)))
    }
    fn walk_calls(&mut self, node: Node<'tree>, caller: &str) {
        if matches!(
            node.kind(),
            "function_definition" | "short_function_definition"
        ) {
            return;
        }
        if node.kind() == "call_expression"
            && let Some(callee) = node.child(0)
        {
            let name = if callee.kind() == "identifier" {
                Some(read_text(callee, self.source))
            } else if callee.kind() == "field_expression" && callee.child_count() >= 3 {
                callee
                    .child(callee.child_count() - 1)
                    .map(|n| read_text(n, self.source))
            } else {
                None
            };
            if let Some(name) = name {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.edge(
                    caller,
                    &id,
                    "calls",
                    node.start_position().row + 1,
                    Some("call"),
                );
            }
        } else if node.kind() == "broadcast_call_expression" {
            // `foo.(xs)` (#4310).
            if let Some(callee) = node.child(0).filter(|n| n.kind() == "identifier") {
                let id = make_id([self.stem.as_str(), read_text(callee, self.source).as_str()]);
                self.edge(
                    caller,
                    &id,
                    "calls",
                    node.start_position().row + 1,
                    Some("call"),
                );
            }
        } else if node.kind() == "binary_expression" && node.child_count() == 3 {
            // `x |> foo` and `xs .|> foo` (#4310).
            if let (Some(op), Some(rhs)) = (node.child(1), node.child(2))
                && matches!(read_text(op, self.source).as_str(), "|>" | ".|>")
                && rhs.kind() == "identifier"
            {
                let id = make_id([self.stem.as_str(), read_text(rhs, self.source).as_str()]);
                self.edge(
                    caller,
                    &id,
                    "calls",
                    node.start_position().row + 1,
                    Some("call"),
                );
            }
        }
        for child in children(node) {
            self.walk_calls(child, caller);
        }
    }
    fn module_name(&self, node: Node<'tree>) -> Option<String> {
        match node.kind() {
            "import_path" => children(node)
                .filter(|n| n.kind() == "identifier")
                .last()
                .map(|n| read_text(n, self.source)),
            "identifier" | "scoped_identifier" => Some(read_text(node, self.source)),
            _ => None,
        }
    }
    fn walk(&mut self, node: Node<'tree>, scope: &str) {
        let line = node.start_position().row + 1;
        if node.kind() == "module_definition" {
            if let Some(name) = children(node).find(|n| n.kind() == "identifier") {
                let name = read_text(name, self.source);
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.node(&id, &name, line);
                self.edge(&self.file.clone(), &id, "defines", line, None);
                for child in children(node) {
                    self.walk(child, &id);
                }
            }
            return;
        }
        if matches!(node.kind(), "struct_definition" | "abstract_definition") {
            let Some(head) = children(node).find(|n| n.kind() == "type_head") else {
                return;
            };
            let (name, super_name) = self.type_head(head);
            let Some(name) = name.filter(|s| !s.is_empty()) else {
                return;
            };
            let id = make_id([self.stem.as_str(), name.as_str()]);
            self.node(&id, &name, line);
            self.edge(scope, &id, "defines", line, None);
            if let Some(parent) = super_name.filter(|s| !s.is_empty()) {
                let target = self.ensure_named(&parent);
                self.edge(&id, &target, "inherits", line, None);
            }
            if node.kind() == "struct_definition" {
                let params = self.type_head_params(head);
                for child in children(node).filter(|n| n.kind() == "typed_expression") {
                    let ids = children(child)
                        .filter(|n| n.kind() == "identifier")
                        .collect::<Vec<_>>();
                    if ids.len() >= 2 {
                        let name = read_text(*ids.last().unwrap(), self.source);
                        if params.contains(&name) {
                            // A declared type parameter, not a real type.
                            continue;
                        }
                        let target = self.ensure_named(&name);
                        self.edges.push(
                            semantic_reference_edge(
                                &id,
                                &target,
                                "field",
                                &self.path,
                                &json!(child.start_position().row + 1),
                            )
                            .expect("field is a supported context"),
                        );
                    }
                }
            }
            return;
        }
        if node.kind() == "function_definition" {
            if let Some(signature) = children(node).find(|n| n.kind() == "signature")
                && let Some((name, label)) =
                    self.function_name(signature).filter(|(s, _)| !s.is_empty())
            {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.node(&id, &format!("{label}()"), line);
                self.edge(scope, &id, "defines", line, None);
                self.bodies.push((id, node));
            }
            return;
        }
        // #3841: macro definitions share the function shape (a `signature`
        // wrapping a call_expression).
        if node.kind() == "macro_definition" {
            if let Some(signature) = children(node).find(|n| n.kind() == "signature")
                && let Some((name, _)) =
                    self.function_name(signature).filter(|(s, _)| !s.is_empty())
            {
                let label = format!("@{name}");
                let id = make_id([self.stem.as_str(), label.as_str()]);
                self.node(&id, &label, line);
                self.edge(scope, &id, "defines", line, None);
                self.bodies.push((id, node));
            }
            return;
        }
        // #3841: `@enum Name a b c` / `@enum Name begin a; b end`; the first
        // argument is the (possibly typed) enum, the rest are `case_of` members.
        // Other macro calls are walked so nested definitions are still seen.
        if node.kind() == "macrocall_expression" {
            let macro_name = children(node)
                .find(|n| n.kind() == "macro_identifier")
                .and_then(|m| children(m).find(|n| n.kind() == "identifier"))
                .map(|n| read_text(n, self.source));
            if macro_name.as_deref() == Some("enum") {
                let mut arguments = vec![];
                if let Some(list) = children(node).find(|n| n.kind() == "macro_argument_list") {
                    for child in children(list) {
                        match child.kind() {
                            "identifier" | "typed_expression" | "assignment" => {
                                arguments.push(child)
                            }
                            "compound_statement" => arguments.extend(
                                children(child)
                                    .filter(|n| matches!(n.kind(), "identifier" | "assignment")),
                            ),
                            _ => (),
                        }
                    }
                }
                let ident = |n: Node<'tree>| {
                    if n.kind() == "identifier" {
                        Some(n)
                    } else {
                        children(n).find(|c| c.kind() == "identifier")
                    }
                };
                if let Some(type_node) = arguments.first().and_then(|n| ident(*n)) {
                    let name = read_text(type_node, self.source);
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(scope, &id, "defines", line, None);
                    for member in arguments.iter().skip(1).filter_map(|n| ident(*n)) {
                        let member_name = read_text(member, self.source);
                        let member_line = member.start_position().row + 1;
                        let member_id =
                            make_id([self.stem.as_str(), name.as_str(), member_name.as_str()]);
                        self.node(&member_id, &member_name, member_line);
                        self.edge(&id, &member_id, "case_of", member_line, None);
                    }
                }
                return;
            }
            for child in children(node) {
                self.walk(child, scope);
            }
            return;
        }
        // Short function `foo(x) = expr`, also with `where`, a return type, an
        // operator, a qualified name or a functor head.
        if node.kind() == "assignment" {
            if let Some((name, label)) = self.def_name(unwrap_call(node.child(0))) {
                let id = make_id([self.stem.as_str(), name.as_str()]);
                self.node(&id, &format!("{label}()"), line);
                self.edge(scope, &id, "defines", line, None);
                if node.child_count() >= 3 {
                    self.bodies
                        .push((id, node.child(node.child_count() - 1).unwrap()));
                }
            }
            return;
        }
        if matches!(node.kind(), "using_statement" | "import_statement") {
            for child in children(node) {
                let name = if child.kind() == "selected_import" {
                    children(child)
                        .find(|n| {
                            matches!(n.kind(), "identifier" | "scoped_identifier" | "import_path")
                        })
                        .and_then(|n| self.module_name(n))
                } else {
                    self.module_name(child)
                };
                if let Some(name) = name.filter(|s| !s.is_empty()) {
                    let id = make_id([name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(scope, &id, "imports", line, Some("import"));
                }
            }
            return;
        }
        for child in children(node) {
            self.walk(child, scope);
        }
    }
}
pub fn extract_julia(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&match crate::grammar!(
        "grammar-julia",
        "tree_sitter_julia",
        tree_sitter_julia::LANGUAGE
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Julia parse cancelled"});
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
        bodies: vec![],
    };
    ex.node(&file, name, 1);
    ex.walk(tree.root_node(), &file);
    for (caller, body) in ex.bodies.clone() {
        if matches!(body.kind(), "function_definition" | "macro_definition") {
            for child in children(body).filter(|n| n.kind() != "signature") {
                ex.walk_calls(child, &caller);
            }
        } else {
            ex.walk_calls(body, &caller);
        }
    }
    json!({"nodes":ex.nodes,"edges":ex.edges})
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-julia")]
    fn grammar_matches_pinned_reference() {
        let l: tree_sitter::Language = tree_sitter_julia::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 245);
        assert_eq!(l.parse_state_count(), 19618);
        assert_eq!(l.abi_version(), 14);
    }
}
