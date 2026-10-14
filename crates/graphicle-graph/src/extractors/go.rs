// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/go.py
//! Go package-scoped types, case-stable symbols, signatures and call candidates.
use super::base::{file_stem, is_language_builtin, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;
pub const PREDECLARED_TYPES: &[&str] = &[
    "bool",
    "byte",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "any",
    "comparable",
];
pub const PREDECLARED_FUNCS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
];
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
/// `_go_type_param_names`: the names a declaration's generic type parameters
/// bind: its own `[T any, R comparable]` list and, for a method, the type
/// arguments of its receiver (`(b Box[T])`; a method declares none of its
/// own). They are local bindings, not corpus symbols (#4383).
pub fn type_param_names(decl: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    for declaration in children(decl)
        .filter(|n| n.kind() == "type_parameter_list")
        .flat_map(children)
        .filter(|n| n.kind() == "type_parameter_declaration")
    {
        for c in children(declaration).filter(|n| n.kind() == "identifier") {
            let text = read_text(c, source);
            if !text.is_empty() {
                names.insert(text);
            }
        }
    }
    if let Some(receiver) = decl.child_by_field_name("receiver") {
        let mut stack = vec![receiver];
        while let Some(n) = stack.pop() {
            if n.kind() == "type_arguments" {
                let mut leaves = vec![n];
                while let Some(leaf) = leaves.pop() {
                    if leaf.kind() == "type_identifier" {
                        let text = read_text(leaf, source);
                        if !text.is_empty() {
                            names.insert(text);
                        }
                    }
                    leaves.extend(children(leaf));
                }
                continue;
            }
            stack.extend(children(n));
        }
    }
    names
}
/// Walk a Go type expression into `(name, role)` references; names in `skip`
/// (the enclosing declaration's type parameters) are bindings and dropped.
pub fn collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut Vec<(String, String)>,
    skip: &HashSet<String>,
) {
    let Some(node) = node else {
        return;
    };
    match node.kind() {
        "type_identifier" | "qualified_type" => {
            let text = read_text(node, source);
            if !text.is_empty()
                && (node.kind() == "qualified_type"
                    || (!PREDECLARED_TYPES.contains(&text.as_str()) && !skip.contains(&text)))
            {
                out.push((text, if generic { "generic_arg" } else { "type" }.into()));
            }
            return;
        }
        "generic_type" => {
            collect_type_refs(node.child_by_field_name("type"), source, generic, out, skip);
            for args in children(node).filter(|n| n.kind() == "type_arguments") {
                for arg in children(args).filter(|n| n.is_named()) {
                    collect_type_refs(Some(arg), source, true, out, skip);
                }
            }
            return;
        }
        _ => {}
    }
    if node.is_named() {
        for child in children(node).filter(|n| n.is_named()) {
            collect_type_refs(Some(child), source, generic, out, skip);
        }
    }
}
struct Extractor<'source, 'tree> {
    source: &'source [u8],
    path: String,
    stem: String,
    scope: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
    imports: Map<String, Value>,
    groups: HashMap<String, HashSet<String>>,
    labels: HashMap<String, String>,
    call_pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
}
impl<'tree> Extractor<'_, 'tree> {
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
        let mut e = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context.filter(|s| !s.is_empty()) {
            e["context"] = json!(c);
        }
        self.edges.push(e);
    }
    fn named(&mut self, name: &str) -> String {
        let local = make_id([self.scope.as_str(), name]);
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn type_refs(&self, node: Option<Node<'_>>, skip: &HashSet<String>) -> Vec<(String, String)> {
        let mut refs = vec![];
        collect_type_refs(node, self.source, false, &mut refs, skip);
        refs
    }
    fn refs(
        &mut self,
        node: Option<Node<'_>>,
        from: &str,
        line: usize,
        context: &str,
        skip: &HashSet<String>,
    ) {
        for (name, role) in self.type_refs(node, skip) {
            let target = self.named(&name);
            if target != from {
                self.edge(
                    from,
                    &target,
                    "references",
                    line,
                    Some(if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        context
                    }),
                );
            }
        }
    }
    /// Parameter and return type references of a signature. Its own generic
    /// type parameters (and those of the receiver, or of an enclosing generic
    /// interface given in `type_params`) are bindings: a `[T any]` signature
    /// no longer mints a sourceless `T` shared by every generic function.
    fn signatures(
        &mut self,
        node: Node<'_>,
        function: &str,
        line: usize,
        type_params: &HashSet<String>,
    ) {
        let mut skip = type_param_names(node, self.source);
        skip.extend(type_params.iter().cloned());
        if let Some(params) = node.child_by_field_name("parameters") {
            for p in children(params).filter(|n| n.kind() == "parameter_declaration") {
                self.refs(
                    p.child_by_field_name("type"),
                    function,
                    line,
                    "parameter_type",
                    &skip,
                );
            }
        }
        if let Some(result) = node.child_by_field_name("result") {
            if result.kind() == "parameter_list" {
                for p in children(result).filter(|n| n.kind() == "parameter_declaration") {
                    self.refs(
                        p.child_by_field_name("type")
                            .or_else(|| children(p).find(|n| n.is_named())),
                        function,
                        line,
                        "return_type",
                        &skip,
                    );
                }
            } else {
                self.refs(Some(result), function, line, "return_type", &skip);
            }
        }
    }
    fn receiver(&self, node: Node<'_>) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let parameter = children(receiver).find(|n| n.kind() == "parameter_declaration")?;
        parameter
            .child_by_field_name("type")
            .map(|n| trim(read_text(n, self.source).trim_start_matches('*')).to_string())
    }
    fn scan(&mut self, node: Node<'_>) {
        // #3737: interface method names join the case-collision groups
        // under their owner so case-only siblings get distinct, stable ids.
        if node.kind() == "type_spec"
            && let Some(name) = node.child_by_field_name("name")
        {
            let owner = make_id([self.scope.as_str(), read_text(name, self.source).as_str()]);
            for body in children(node).filter(|n| n.kind() == "interface_type") {
                for elem in children(body).filter(|n| n.kind() == "method_elem") {
                    if let Some(method) = elem.child_by_field_name("name") {
                        let method = read_text(method, self.source);
                        self.groups
                            .entry(make_id([owner.as_str(), method.as_str()]))
                            .or_default()
                            .insert(method);
                    }
                }
            }
        }
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            if let Some(name) = node.child_by_field_name("name") {
                let name = read_text(name, self.source);
                let base = if node.kind() == "method_declaration" {
                    self.receiver(node)
                        .filter(|s| !s.is_empty())
                        .map(|r| make_id([self.scope.as_str(), r.as_str()]))
                        .unwrap_or_else(|| self.stem.clone())
                } else {
                    self.stem.clone()
                };
                self.groups
                    .entry(make_id([base.as_str(), name.as_str()]))
                    .or_default()
                    .insert(name);
            }
            return;
        }
        for n in children(node) {
            self.scan(n);
        }
    }
    fn symbol(&self, plain: String, name: &str) -> String {
        let Some(names) = self.groups.get(&plain).filter(|n| n.len() >= 2) else {
            return plain;
        };
        let exported: Vec<_> = names
            .iter()
            .filter(|n| n.chars().next().is_some_and(char::is_uppercase))
            .collect();
        if exported.len() == 1 && exported[0] == name {
            return plain;
        }
        let digest = format!("{:x}", Sha1::digest(name.as_bytes()));
        make_id([plain.as_str(), &digest[..6]])
    }
    fn walk(&mut self, node: Node<'tree>) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "function_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    let id = self.symbol(make_id([self.stem.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    self.signatures(node, &id, line, &HashSet::new());
                    if let Some(body) = node.child_by_field_name("body") {
                        self.bodies.push((id, body));
                    }
                }
                return;
            }
            "method_declaration" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let name = read_text(name, self.source);
                let id = if let Some(receiver) = self.receiver(node).filter(|s| !s.is_empty()) {
                    let parent = make_id([self.scope.as_str(), receiver.as_str()]);
                    self.node(&parent, &receiver, line);
                    let id = self.symbol(make_id([parent.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!(".{name}()"), line);
                    self.edge(&parent, &id, "method", line, None);
                    id
                } else {
                    let id = self.symbol(make_id([self.stem.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    id
                };
                self.signatures(node, &id, line, &HashSet::new());
                if let Some(body) = node.child_by_field_name("body") {
                    self.bodies.push((id, body));
                }
                return;
            }
            "type_declaration" => {
                for spec in children(node).filter(|n| n.kind() == "type_spec") {
                    let Some(name) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let name = read_text(name, self.source);
                    let line = spec.start_position().row + 1;
                    let id = make_id([self.scope.as_str(), name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    // A generic type's own parameters (`type Box[T any]`) are
                    // bindings for its fields and methods (#4383).
                    let type_params = type_param_names(spec, self.source);
                    let Some(body) = children(spec)
                        .find(|n| matches!(n.kind(), "struct_type" | "interface_type"))
                    else {
                        continue;
                    };
                    if body.kind() == "struct_type" {
                        for field in children(body)
                            .filter(|n| n.kind() == "field_declaration_list")
                            .flat_map(children)
                            .filter(|n| n.kind() == "field_declaration")
                        {
                            let has_name = children(field).any(|n| n.kind() == "field_identifier");
                            let typ = field.child_by_field_name("type").or_else(|| {
                                children(field)
                                    .find(|n| n.is_named() && n.kind() != "field_identifier")
                            });
                            let line = field.start_position().row + 1;
                            for (name, role) in self.type_refs(typ, &type_params) {
                                let target = self.named(&name);
                                if target == id {
                                    continue;
                                }
                                if !has_name && role == "type" {
                                    self.edge(&id, &target, "embeds", line, None);
                                } else {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some(if role == "generic_arg" {
                                            "generic_arg"
                                        } else {
                                            "field"
                                        }),
                                    );
                                }
                            }
                        }
                    } else {
                        for elem in children(body) {
                            let line = elem.start_position().row + 1;
                            if elem.kind() == "method_elem" {
                                let Some(name) = elem.child_by_field_name("name").or_else(|| {
                                    children(elem).find(|n| n.kind() == "field_identifier")
                                }) else {
                                    continue;
                                };
                                let name = read_text(name, self.source);
                                let method =
                                    self.symbol(make_id([id.as_str(), name.as_str()]), &name);
                                self.node(&method, &format!(".{name}()"), line);
                                self.edge(&id, &method, "method", line, None);
                                // #3672: interface method signatures keep their
                                // parameter/return type references.
                                self.signatures(elem, &method, line, &type_params);
                                continue;
                            }
                            if elem.kind() != "type_elem" {
                                continue;
                            }
                            let type_set =
                                children(elem).any(|n| matches!(n.kind(), "|" | "negated_type"));
                            let refs: Vec<_> = children(elem)
                                .filter(|n| n.is_named())
                                .flat_map(|n| self.type_refs(Some(n), &type_params))
                                .collect();
                            for (name, role) in refs {
                                let target = self.named(&name);
                                if target == id {
                                    continue;
                                }
                                if type_set {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some("type_constraint"),
                                    );
                                } else if role == "type" {
                                    self.edge(&id, &target, "embeds", line, None);
                                } else {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some("generic_arg"),
                                    );
                                }
                            }
                        }
                    }
                }
                return;
            }
            "import_declaration" => {
                for n in children(node) {
                    if n.kind() == "import_spec_list" {
                        for spec in children(n).filter(|n| n.kind() == "import_spec") {
                            self.import(spec);
                        }
                    } else if n.kind() == "import_spec" {
                        self.import(n);
                    }
                }
                return;
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n);
        }
    }
    fn import(&mut self, spec: Node<'_>) {
        if let Some(path) = spec.child_by_field_name("path") {
            let raw = read_text(path, self.source).trim_matches('"').to_string();
            let target = make_id(["go", "pkg", raw.as_str()]);
            self.edge(
                &self.file.clone(),
                &target,
                "imports_from",
                spec.start_position().row + 1,
                Some("import"),
            );
            let local = spec
                .child_by_field_name("name")
                .map(|n| read_text(n, self.source))
                .unwrap_or_else(|| raw.rsplit('/').next().unwrap_or("").into());
            if !local.is_empty() && local != "_" && local != "." {
                self.imports.insert(local, json!(raw));
            }
        }
    }
    fn calls(&mut self, node: Node<'_>, caller: &str) {
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return;
        }
        if node.kind() == "call_expression" {
            let mut callee = None;
            let mut member = false;
            let mut bare = false;
            let mut receiver = None;
            let mut import_path = None;
            if let Some(function) = node.child_by_field_name("function") {
                if function.kind() == "identifier" {
                    bare = true;
                    callee = Some(read_text(function, self.source));
                } else if function.kind() == "selector_expression" {
                    let operand = function
                        .child_by_field_name("operand")
                        .map(|n| read_text(n, self.source))
                        .unwrap_or_default();
                    member = !self.imports.contains_key(&operand);
                    if !member {
                        receiver = Some(operand.clone());
                        import_path = self
                            .imports
                            .get(&operand)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    callee = function
                        .child_by_field_name("field")
                        .map(|n| read_text(n, self.source));
                }
            }
            if bare
                && callee
                    .as_ref()
                    .is_some_and(|n| PREDECLARED_FUNCS.contains(&n.as_str()))
            {
                callee = None;
            }
            if let Some(callee) = callee.filter(|s| !s.is_empty() && !is_language_builtin(s)) {
                let target = if import_path.as_ref().is_some_and(|s| !s.is_empty()) {
                    None
                } else {
                    self.labels.get(&callee).filter(|s| !s.is_empty()).cloned()
                };
                if let Some(target) = target.filter(|s| s != caller) {
                    if self.call_pairs.insert((caller.into(), target.clone())) {
                        self.edge(
                            caller,
                            &target,
                            "calls",
                            node.start_position().row + 1,
                            Some("call"),
                        );
                    }
                } else {
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"language":"go","receiver":receiver,"import_path":import_path,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                }
            }
        }
        for n in children(node) {
            self.calls(n, caller);
        }
    }
}
pub fn extract_go(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&tree_sitter_go::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":e.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Go parse cancelled"});
    };
    let text = fsutil::path_string(path);
    let file = make_id([text.as_str()]);
    let mut parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts).to_string();
    if !name.is_empty() {
        parts.pop();
    }
    let stem = file_stem(path);
    let scope = path_syntax::posix_name(&parts);
    let scope = if scope.is_empty() {
        stem.clone()
    } else {
        scope.to_string()
    };
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem,
        scope,
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        imports: Map::new(),
        groups: HashMap::new(),
        labels: HashMap::new(),
        call_pairs: HashSet::new(),
        raw_calls: vec![],
    };
    ex.node(&file, &name, 1);
    ex.scan(tree.root_node());
    ex.walk(tree.root_node());
    ex.labels = ex
        .nodes
        .iter()
        .filter_map(|n| {
            Some((
                n["label"]
                    .as_str()?
                    .trim_matches(['(', ')'])
                    .trim_start_matches('.')
                    .to_string(),
                n["id"].as_str()?.into(),
            ))
        })
        .collect();
    for (caller, body) in std::mem::take(&mut ex.bodies) {
        ex.calls(body, &caller);
    }
    ex.edges.retain(|e| {
        e["source"].as_str().is_some_and(|s| ex.seen.contains(s))
            && (e["target"].as_str().is_some_and(|s| ex.seen.contains(s))
                || matches!(e["relation"].as_str(), Some("imports" | "imports_from")))
    });
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"go_imports":ex.imports})
}
#[cfg(test)]
mod tests {
    #[test]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 219);
        assert_eq!(l.parse_state_count(), 1442);
        assert_eq!(l.abi_version(), 15);
    }
}
