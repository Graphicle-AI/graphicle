// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/r.py
//! Port of graphify/extractors/r.py (Graphify 0.9.69, #3715): functions,
//! R6/Reference/ggproto/S4 classes and methods, `library`/`require` packages
//! and `source()` imports. Calls bind lexically; calls into `source()`d
//! files resolve across the corpus through [`resolve_r_sourced_calls`].
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax, python_compat};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;

const CLASS_CONSTRUCTORS: [&str; 3] = ["R6Class", "setRefClass", "ggproto"];
const NON_PROJECT_CALLS: [&str; 13] = [
    "R6Class",
    "c",
    "get",
    "ggproto",
    "library",
    "list",
    "print",
    "require",
    "setClass",
    "setGeneric",
    "setMethod",
    "setRefClass",
    "source",
];

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
/// `_unquote`: strip whitespace, then one pair of matching quotes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && matches!(chars[0], '\'' | '"') {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        value.into()
    }
}
struct Extractor<'t, 's> {
    source: &'s [u8],
    path: &'s Path,
    path_text: String,
    stem: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    seen: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
    definitions: HashMap<String, HashMap<String, String>>,
    scope_parent: HashMap<String, String>,
    bodies: Vec<(Node<'t>, String)>,
    classes: HashMap<String, String>,
}
impl<'t> Extractor<'t, '_> {
    fn text(&self, node: Node<'_>) -> String {
        read_text(node, self.source)
    }
    fn add_node(
        &mut self,
        id: &str,
        label: &str,
        line: usize,
        kind: &str,
        source_backed: bool,
        callable: bool,
    ) {
        if id.is_empty() || !self.seen.insert(id.into()) {
            return;
        }
        let mut item = Map::new();
        item.insert("id".into(), json!(id));
        item.insert("label".into(), json!(label));
        item.insert("file_type".into(), json!("code"));
        item.insert("source_location".into(), json!(format!("L{line}")));
        item.insert("metadata".into(), json!({"language":"r","kind":kind}));
        if source_backed {
            item.insert("source_file".into(), json!(self.path_text));
        }
        if callable {
            item.insert("_callable".into(), json!(true));
        }
        self.nodes.push(Value::Object(item));
    }
    fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        target_file: Option<&str>,
    ) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self
                .seen_edges
                .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path_text,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(target_file) = target_file {
            edge["target_file"] = json!(target_file);
        }
        self.edges.push(edge);
    }
    /// The last named child of an argument other than its `name`.
    fn argument_value(argument: Node<'t>) -> Option<Node<'t>> {
        let name = argument.child_by_field_name("name").map(|n| n.id());
        named(argument).into_iter().rfind(|c| Some(c.id()) != name)
    }
    fn call_parts(&self, node: Node<'t>) -> (String, Option<Node<'t>>) {
        let arguments = node.child_by_field_name("arguments");
        match node.child_by_field_name("function") {
            Some(function) => (self.text(function), arguments),
            None => (String::new(), arguments),
        }
    }
    fn string_or_identifier(&self, node: Option<Node<'_>>) -> String {
        node.map(|n| unquote(&self.text(n))).unwrap_or_default()
    }
    fn arguments(arguments: Node<'t>) -> Vec<Node<'t>> {
        named(arguments)
            .into_iter()
            .filter(|c| c.kind() == "argument")
            .collect()
    }
    /// The id of a binding `name` in scope `owner`. `make_id` drops an R
    /// name's punctuation, so a symbol-only operator (`%||%`) collapsed to its
    /// owner's id and a backtick name (`%foo%`, `[.cls`) met the plain one
    /// (`foo`); both get a reserved namespace keyed on the name's exact bytes
    /// (#4171, #4253). Repeated underscores never come out of `make_id`, so the
    /// namespaces cannot meet, and the owner is kept verbatim so a nested
    /// binding stays inside an operator's namespace.
    fn binding_id(owner: &str, name: &str) -> String {
        let suffix = make_id([name]);
        if suffix.is_empty() || name.starts_with('`') {
            let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
            return format!("{owner}__operator__{hex}");
        }
        format!("{owner}_{suffix}")
    }
    fn declare_function(
        &mut self,
        name: &str,
        function: Node<'t>,
        body: Option<Node<'t>>,
        owner: &str,
        relation: &str,
    ) -> String {
        let id = Self::binding_id(owner, name);
        let line = function.start_position().row + 1;
        self.add_node(&id, &format!("{name}()"), line, "function", true, true);
        self.add_edge(owner, &id, relation, line, None);
        self.definitions
            .entry(owner.into())
            .or_default()
            .insert(name.into(), id.clone());
        self.definitions.entry(id.clone()).or_default();
        self.scope_parent.insert(id.clone(), owner.into());
        if let Some(body) = body {
            self.bodies.push((body, id.clone()));
            self.scan_declarations(body, &id, false);
        }
        id
    }
    fn add_class(&mut self, name: &str, declaration: Node<'_>, owner: &str) -> String {
        let id = make_id([self.stem.as_str(), "class", name]);
        let line = declaration.start_position().row + 1;
        self.add_node(&id, name, line, "class", true, true);
        self.add_edge(owner, &id, "contains", line, None);
        self.definitions.entry(id.clone()).or_default();
        self.scope_parent.insert(id.clone(), owner.into());
        self.classes.insert(name.into(), id.clone());
        id
    }
    fn add_class_members(&mut self, class_id: &str, arguments: Node<'t>) {
        for argument in named(arguments) {
            if argument.kind() != "argument" {
                continue;
            }
            let value = Self::argument_value(argument);
            let key = argument
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .unwrap_or_default();
            if key == "inherit"
                && let Some(value) = value
            {
                let written = self.string_or_identifier(Some(value));
                let base = written.rsplit('$').next().unwrap_or("").to_string();
                if !base.is_empty() {
                    let base_id = self
                        .classes
                        .get(&base)
                        .cloned()
                        .unwrap_or_else(|| make_id(["r", "class", base.as_str()]));
                    let line = value.start_position().row + 1;
                    self.add_node(&base_id, &base, line, "external_class", false, false);
                    self.add_edge(class_id, &base_id, "inherits", line, None);
                }
            }
            let Some(value) = value.filter(|v| v.kind() == "call") else {
                continue;
            };
            if !matches!(key.as_str(), "public" | "private" | "active" | "methods") {
                continue;
            }
            let (list, list_arguments) = self.call_parts(value);
            let Some(list_arguments) = list_arguments.filter(|_| list == "list") else {
                continue;
            };
            for member in named(list_arguments) {
                if member.kind() != "argument" {
                    continue;
                }
                let (Some(member_name), Some(member_value)) = (
                    member.child_by_field_name("name"),
                    Self::argument_value(member).filter(|v| v.kind() == "function_definition"),
                ) else {
                    continue;
                };
                let member_name = self.text(member_name);
                self.declare_function(
                    &member_name,
                    member_value,
                    member_value.child_by_field_name("body"),
                    class_id,
                    "method",
                );
            }
        }
    }
    fn process_call_declaration(&mut self, node: Node<'t>, owner: &str) -> bool {
        let (name, arguments) = self.call_parts(node);
        // A namespace-qualified constructor (`methods::setClass`,
        // `R6::R6Class`) is the same declaration as its bare form (0.9.71).
        let name = name
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .trim_matches(crate::dedup_strings::whitespace)
            .to_string();
        let Some(arguments) = arguments else {
            return false;
        };
        let args = Self::arguments(arguments);
        let line = node.start_position().row + 1;
        let first = || args.first().and_then(|a| Self::argument_value(*a));
        match name.as_str() {
            "library" | "require" if !args.is_empty() => {
                let package = self.string_or_identifier(first());
                if !package.is_empty() {
                    let id = make_id(["r", "package", package.as_str()]);
                    self.add_node(&id, &package, line, "package", false, false);
                    let file_id = self.file_id.clone();
                    self.add_edge(&file_id, &id, "imports", line, None);
                }
                true
            }
            "source" if !args.is_empty() => {
                let relative = self.string_or_identifier(first());
                if !relative.is_empty() && !relative.contains('$') && !relative.contains('(') {
                    let target = self.path.parent().unwrap_or(Path::new("")).join(&relative);
                    let target = fsutil::path_string(&target);
                    let file_id = self.file_id.clone();
                    self.add_edge(
                        &file_id,
                        &make_id([target.as_str()]),
                        "imports_from",
                        line,
                        Some(&target),
                    );
                }
                true
            }
            "setClass" | "setRefClass" if !args.is_empty() => {
                let class = self.string_or_identifier(first());
                if !class.is_empty() {
                    let id = match self.classes.get(&class) {
                        Some(id) => id.clone(),
                        None => self.add_class(&class, node, owner),
                    };
                    self.add_class_members(&id, arguments);
                }
                true
            }
            "setMethod" if args.len() >= 3 => {
                let method = self.string_or_identifier(Self::argument_value(args[0]));
                let class = self.string_or_identifier(Self::argument_value(args[1]));
                let implementation = Self::argument_value(args[args.len() - 1]);
                if !method.is_empty()
                    && !class.is_empty()
                    && let Some(implementation) = implementation
                {
                    let id = match self.classes.get(&class) {
                        Some(id) => id.clone(),
                        None => self.add_class(&class, node, owner),
                    };
                    if implementation.kind() == "function_definition" {
                        self.declare_function(
                            &method,
                            implementation,
                            implementation.child_by_field_name("body"),
                            &id,
                            "method",
                        );
                    }
                }
                true
            }
            "setGeneric" if !args.is_empty() => {
                let generic = self.string_or_identifier(first());
                if !generic.is_empty() {
                    self.declare_function(&generic, node, None, owner, "contains");
                }
                true
            }
            _ => false,
        }
    }
    /// `(name, value, name node)` of an assignment, reading `->` right to left.
    fn binding(&self, node: Option<Node<'t>>) -> Option<(String, Node<'t>, Node<'t>)> {
        let node = node.filter(|n| n.kind() == "binary_operator")?;
        let (mut lhs, mut rhs, operator) = (
            node.child_by_field_name("lhs")?,
            node.child_by_field_name("rhs")?,
            node.child_by_field_name("operator")?,
        );
        match self.text(operator).as_str() {
            "->" | "->>" => std::mem::swap(&mut lhs, &mut rhs),
            "<-" | "<<-" | "=" => {}
            _ => return None,
        }
        (lhs.kind() == "identifier").then(|| (self.text(lhs), rhs, lhs))
    }
    fn scan_declarations(&mut self, node: Node<'t>, owner: &str, top_level: bool) {
        for child in named(node) {
            if let Some((name, value, name_node)) = self.binding(Some(child)) {
                if value.kind() == "function_definition" {
                    self.declare_function(
                        &name,
                        value,
                        value.child_by_field_name("body"),
                        owner,
                        "contains",
                    );
                    continue;
                }
                if value.kind() == "call" {
                    let (constructor, arguments) = self.call_parts(value);
                    // `R6::R6Class(...)`: match the bare constructor (0.9.71).
                    let constructor = constructor
                        .rsplit("::")
                        .next()
                        .unwrap_or_default()
                        .trim_matches(crate::dedup_strings::whitespace)
                        .to_string();
                    if CLASS_CONSTRUCTORS.contains(&constructor.as_str()) {
                        let mut class = name.clone();
                        if let Some(first) =
                            arguments.and_then(|a| Self::arguments(a).into_iter().next())
                        {
                            let written = self.string_or_identifier(Self::argument_value(first));
                            if !written.is_empty() {
                                class = written;
                            }
                        }
                        let id = self.add_class(&class, child, owner);
                        if let Some(arguments) = arguments {
                            self.add_class_members(&id, arguments);
                        }
                        continue;
                    }
                }
                if top_level {
                    let id = Self::binding_id(owner, &name);
                    let line = name_node.start_position().row + 1;
                    self.add_node(&id, &name, line, "variable", true, false);
                    self.add_edge(owner, &id, "contains", line, None);
                }
                continue;
            }
            if child.kind() == "call" && self.process_call_declaration(child, owner) {
                continue;
            }
            // tree-sitter-r attaches a right-assignment after a one-expression
            // function body to that body; recover the actual outer binding.
            if child.kind() == "function_definition" {
                let body = child.child_by_field_name("body");
                let operator = body.and_then(|b| b.child_by_field_name("operator"));
                if let (Some((name, value, _)), Some(operator)) = (self.binding(body), operator)
                    && matches!(self.text(operator).as_str(), "->" | "->>")
                {
                    self.declare_function(&name, child, Some(value), owner, "contains");
                }
                continue;
            }
            self.scan_declarations(child, owner, top_level);
        }
    }
    fn resolve_local(&self, caller: &str, name: &str) -> Option<String> {
        let mut scope = Some(caller);
        // A scope seen twice ends the walk, so a cycle of colliding scope ids
        // cannot hang extraction; the call keeps its raw record (#4171).
        let mut visited = HashSet::new();
        while let Some(s) = scope.filter(|s| visited.insert(*s)) {
            if let Some(target) = self.definitions.get(s).and_then(|d| d.get(name)) {
                return Some(target.clone());
            }
            scope = self.scope_parent.get(s).map(String::as_str);
        }
        None
    }
    fn walk_calls(&mut self, node: Node<'t>, caller: &str) {
        if node.kind() == "function_definition" {
            return;
        }
        if node.kind() == "call" {
            let callee = node.child_by_field_name("function");
            let line = node.start_position().row + 1;
            match callee.map(|c| c.kind()) {
                Some("identifier") => {
                    let callee = self.text(callee.unwrap());
                    if let Some(target) = self.resolve_local(caller, &callee) {
                        self.add_edge(caller, &target, "calls", line, None);
                    } else if !NON_PROJECT_CALLS.contains(&callee.as_str()) {
                        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"language":"r","source_file":self.path_text,"source_location":format!("L{line}")}));
                    }
                }
                // `self$method()` / `private$method()`: an R6 method reaches its
                // siblings only through self/private; resolve it in the
                // enclosing class scope. `super$` is left unresolved (0.9.71).
                Some("extract_operator") => {
                    let callee = callee.unwrap();
                    let mut cursor = callee.walk();
                    let parts = callee
                        .children(&mut cursor)
                        .filter(|c| c.kind() == "identifier")
                        .collect::<Vec<_>>();
                    if parts.len() == 2
                        && matches!(self.text(parts[0]).as_str(), "self" | "private")
                    {
                        let method = self.text(parts[1]);
                        if let Some(target) = self.resolve_local(caller, &method) {
                            self.add_edge(caller, &target, "calls", line, None);
                        }
                    }
                }
                Some("namespace_operator") => {
                    let callee = callee.unwrap();
                    if let (Some(package), Some(function)) = (
                        callee.child_by_field_name("lhs"),
                        callee.child_by_field_name("rhs"),
                    ) {
                        let (package, function) = (self.text(package), self.text(function));
                        let package_id = make_id(["r", "package", package.as_str()]);
                        self.add_node(&package_id, &package, line, "package", false, false);
                        let external =
                            make_id([package_id.as_str(), "function", function.as_str()]);
                        self.add_node(
                            &external,
                            &format!("{package}::{function}()"),
                            line,
                            "external_function",
                            false,
                            true,
                        );
                        self.add_edge(caller, &external, "calls", line, None);
                    }
                }
                _ => {}
            }
        }
        for child in named(node) {
            self.walk_calls(child, caller);
        }
    }
}
pub fn extract_r(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":format!("R grammar failed to load: {}", io_compat::read_error(&error, path))});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&graphicle_tree_sitter_r::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":format!("R grammar failed to load: {error}")});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"R grammar failed to load: parse cancelled"});
    };
    let path_text = fsutil::path_string(path);
    let file_id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let name = path_syntax::posix_name(&parts).to_string();
    let mut x = Extractor {
        source: &source,
        path,
        path_text,
        stem: file_stem(path),
        file_id: file_id.clone(),
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        seen: HashSet::new(),
        seen_edges: HashSet::new(),
        definitions: HashMap::from([(file_id.clone(), HashMap::new())]),
        scope_parent: HashMap::new(),
        bodies: vec![],
        classes: HashMap::new(),
    };
    x.add_node(&file_id, &name, 1, "file", true, false);
    x.scan_declarations(tree.root_node(), &file_id, true);
    for (body, caller) in std::mem::take(&mut x.bodies) {
        x.walk_calls(body, &caller);
    }
    let edges = std::mem::take(&mut x.edges)
        .into_iter()
        .filter(|e| {
            x.seen.contains(e["source"].as_str().unwrap_or(""))
                && (x.seen.contains(e["target"].as_str().unwrap_or(""))
                    || e["relation"] == "imports_from")
        })
        .collect::<Vec<_>>();
    json!({"nodes":x.nodes,"edges":edges,"raw_calls":x.raw_calls})
}

/// Bind R calls only through an explicit `source()` of the defining file.
pub fn resolve_r_sourced_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let text = |v: &Value| python_compat::string(v);
    let is_file = |n: &Value| {
        crate::build::truthy(&n["source_file"])
            && n.get("label").is_some_and(|label| {
                let source = text(&n["source_file"]);
                let parts = path_syntax::posix_parts(&source);
                *label == json!(path_syntax::posix_name(&parts))
            })
    };
    // Python dict semantics: the last file node for a source wins both ways.
    let mut file_source_by_id: Vec<(Value, String)> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    for n in nodes.iter().filter(|n| is_file(n)) {
        let key = n["id"].to_string();
        let entry = (n["id"].clone(), text(&n["source_file"]));
        match index.get(&key) {
            Some(i) => file_source_by_id[*i] = entry,
            None => {
                index.insert(key, file_source_by_id.len());
                file_source_by_id.push(entry);
            }
        }
    }
    let source_of = |id: &Value| {
        index
            .get(&id.to_string())
            .map(|i| file_source_by_id[*i].1.clone())
    };
    let mut sourced: HashMap<String, BTreeSet<String>> = HashMap::new();
    for edge in edges.iter().filter(|e| e["relation"] == "imports_from") {
        if !edge["target"].is_string() {
            continue;
        }
        if let Some(target) = source_of(&edge["target"]) {
            sourced
                .entry(edge["source"].to_string())
                .or_default()
                .insert(target);
        }
    }
    let mut file_id_by_source: HashMap<String, Value> = HashMap::new();
    for (id, source) in &file_source_by_id {
        file_id_by_source.insert(source.clone(), id.clone());
    }
    let mut callables: HashMap<(String, String), Vec<Value>> = HashMap::new();
    for n in nodes {
        if !crate::build::truthy(&n["_callable"]) || !crate::build::truthy(&n["source_file"]) {
            continue;
        }
        let label = n.get("label").map(text).unwrap_or_default();
        let name = label.trim_matches(['(', ')']).to_string();
        callables
            .entry((text(&n["source_file"]), name))
            .or_default()
            .push(n["id"].clone());
    }
    let mut existing = edges
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| (e["source"].clone(), e["target"].clone()))
        .collect::<HashSet<_>>();
    for call in per
        .iter()
        .flat_map(|r| r["raw_calls"].as_array().into_iter().flatten())
    {
        if call["language"] != "r" {
            continue;
        }
        let caller = call["caller_nid"].clone();
        let source_file = call.get("source_file").map(text).unwrap_or_default();
        let Some(caller_file) = file_id_by_source.get(&source_file) else {
            continue;
        };
        let callee = call.get("callee").map(text).unwrap_or_default();
        let mut candidates = vec![];
        for source in sourced.get(&caller_file.to_string()).into_iter().flatten() {
            candidates.extend(
                callables
                    .get(&(source.clone(), callee.clone()))
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        if candidates.len() != 1 || candidates[0] == caller {
            continue;
        }
        let target = candidates.remove(0);
        if !existing.insert((caller.clone(), target.clone())) {
            continue;
        }
        edges.push(json!({"source":caller,"target":target,"relation":"calls","context":"sourced_call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":call.get("source_file").cloned().unwrap_or(json!("")),"source_location":call["source_location"],"weight":1.0}));
    }
}
