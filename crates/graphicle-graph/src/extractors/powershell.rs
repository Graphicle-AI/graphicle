// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/powershell.py
//! PowerShell scripts, classes, import commands and data-only manifests.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    children(node).find(|n| n.kind() == kind)
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
static EXT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\.[^.]+$").unwrap());
const SKIP: &[&str] = &[
    "using",
    "return",
    "if",
    "else",
    "elseif",
    "foreach",
    "for",
    "while",
    "do",
    "switch",
    "try",
    "catch",
    "finally",
    "throw",
    "break",
    "continue",
    "exit",
    "param",
    "begin",
    "process",
    "end",
    "import-module",
];
/// File-scope advanced-function blocks that parse as pseudo-commands, not
/// `named_block`s; the walk descends into them (#4270).
const NAMED_BLOCK_COMMANDS: &[&str] = &["begin", "process", "end", "dynamicparam", "clean"];
fn body(node: Node<'_>) -> Option<Node<'_>> {
    let block = child(node, "script_block")?;
    Some(child(block, "script_block_body").unwrap_or(block))
}
fn type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    for spec in children(node?).filter(|n| n.kind() == "type_spec") {
        for name in children(spec).filter(|n| n.kind() == "type_name") {
            if let Some(id) = child(name, "type_identifier") {
                return Some(read_text(id, source));
            }
        }
    }
    None
}
struct Extractor<'source, 'tree> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    /// (caller, body, owning class) for each function and method body.
    bodies: Vec<(String, Node<'tree>, Option<String>)>,
    labels: HashMap<String, String>,
    /// Lowercased class name -> every declaration's id, duplicates included.
    class_ids_by_name: HashMap<String, Vec<String>>,
    class_names_by_id: HashMap<String, String>,
    /// Class id -> lowercased method name -> method id.
    methods_by_class: HashMap<String, HashMap<String, String>>,
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
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn named(&mut self, name: &str) -> String {
        let local = make_id([self.stem.as_str(), name]);
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn reference(&mut self, source: &str, typ: Option<Node<'_>>, line: usize, context: &str) {
        if let Some(name) = type_name(typ, self.source).filter(|s| !s.is_empty()) {
            let target = self.named(&name);
            if target != source {
                self.edge(source, &target, "references", line, Some(context));
            }
        }
    }
    fn walk(&mut self, node: Node<'tree>, class: Option<&str>) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "function_statement" => {
                if let Some(name) = child(node, "function_name") {
                    let name = read_text(name, self.source);
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    if let Some(body) = body(node) {
                        self.bodies.push((id, body, None));
                        self.walk(body, class);
                    }
                }
                return;
            }
            "enum_statement" => {
                if let Some(name) = child(node, "simple_name") {
                    let name = read_text(name, self.source);
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    for member in children(node).filter(|n| n.kind() == "enum_member") {
                        if let Some(name) = child(member, "simple_name") {
                            let name = read_text(name, self.source);
                            let mid = make_id([id.as_str(), name.as_str()]);
                            let line = member.start_position().row + 1;
                            self.node(&mid, &name, line);
                            // An enum member is a discriminant case, so `case_of`
                            // like every other enum language (0.9.77).
                            self.edge(&id, &mid, "case_of", line, None);
                        }
                    }
                }
                return;
            }
            "class_statement" => {
                if let Some(name) = child(node, "simple_name") {
                    let name = read_text(name, self.source);
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    // Every declaration counts, duplicates sharing an id too.
                    self.class_ids_by_name
                        .entry(name.to_lowercase())
                        .or_default()
                        .push(id.clone());
                    self.class_names_by_id
                        .insert(id.clone(), name.to_lowercase());
                    self.node(&id, &name, line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    let mut colon = false;
                    let mut index = 0;
                    for n in children(node) {
                        if n.kind() == ":" {
                            colon = true;
                        } else if colon && n.kind() == "simple_name" {
                            let target = self.named(&read_text(n, self.source));
                            if target != id {
                                self.edge(
                                    &id,
                                    &target,
                                    if index == 0 { "inherits" } else { "implements" },
                                    line,
                                    None,
                                );
                            }
                            index += 1;
                        }
                    }
                    for n in children(node) {
                        self.walk(n, Some(&id));
                    }
                }
                return;
            }
            "class_property_definition" if class.is_some_and(|s| !s.is_empty()) => {
                self.reference(class.unwrap(), child(node, "type_literal"), line, "field");
                return;
            }
            "class_method_definition" => {
                if let Some(name) = child(node, "simple_name") {
                    let name = read_text(name, self.source);
                    let owner = class.filter(|s| !s.is_empty());
                    let id = if let Some(class) = owner {
                        let id = make_id([class, name.as_str()]);
                        self.node(&id, &format!(".{name}()"), line);
                        self.edge(class, &id, "method", line, None);
                        self.methods_by_class
                            .entry(class.to_string())
                            .or_default()
                            .insert(name.to_lowercase(), id.clone());
                        id
                    } else {
                        let id = make_id([self.stem.as_str(), name.as_str()]);
                        self.node(&id, &format!("{name}()"), line);
                        self.edge(&self.file.clone(), &id, "contains", line, None);
                        id
                    };
                    self.reference(&id, child(node, "type_literal"), line, "return_type");
                    if let Some(params) = child(node, "class_method_parameter_list") {
                        for p in children(params).filter(|n| n.kind() == "class_method_parameter") {
                            self.reference(
                                &id,
                                child(p, "type_literal"),
                                p.start_position().row + 1,
                                "parameter_type",
                            );
                        }
                    }
                    if let Some(body) = body(node) {
                        self.bodies.push((id, body, owner.map(str::to_string)));
                    }
                }
                return;
            }
            "command" => {
                if child(node, "command_invokation_operator")
                    .is_some_and(|n| trim(&read_text(n, self.source)) == ".")
                {
                    // A bare path is a command_name, `$PSScriptRoot\x.ps1` a
                    // path_command_name and a quoted path a string_literal (#4356).
                    if let Some(name) = child(node, "command_name_expr").and_then(|n| {
                        children(n).find(|c| {
                            matches!(
                                c.kind(),
                                "command_name" | "path_command_name" | "string_literal"
                            )
                        })
                    }) {
                        let raw = read_text(name, self.source);
                        let raw = trim(&raw).trim_matches(['\'', '"']);
                        let stem = raw.trim_start_matches(['.', '/', '\\']);
                        let stem = EXT.replace(stem, "").replace('\\', "/");
                        let name = stem.rsplit('/').next().unwrap_or("");
                        if !name.is_empty() {
                            self.edge(
                                &self.file.clone(),
                                &make_id([name]),
                                "imports_from",
                                line,
                                None,
                            );
                        }
                    }
                    return;
                }
                if let Some(name) = child(node, "command_name") {
                    let text = read_text(name, self.source).to_lowercase();
                    if text == "using" {
                        let tokens: Vec<String> = children(node)
                            .filter(|n| n.kind() == "command_elements")
                            .flat_map(children)
                            .filter(|n| n.kind() == "generic_token")
                            .map(|n| read_text(n, self.source))
                            .filter(|s| {
                                !["namespace", "module", "assembly"]
                                    .contains(&s.to_lowercase().as_str())
                            })
                            .collect();
                        if let Some(last) = tokens.last() {
                            let name = last.rsplit('.').next().unwrap_or("");
                            self.edge(
                                &self.file.clone(),
                                &make_id([name]),
                                "imports_from",
                                line,
                                None,
                            );
                        }
                    } else if text == "import-module" {
                        let mut name = None;
                        let mut expect = false;
                        for el in children(node)
                            .filter(|n| n.kind() == "command_elements")
                            .flat_map(children)
                        {
                            if el.kind() == "command_parameter" {
                                let parameter = read_text(el, self.source);
                                expect = ["name", "n"].contains(
                                    &parameter.trim_start_matches('-').to_lowercase().as_str(),
                                );
                            } else if el.kind() == "generic_token" && (name.is_none() || expect) {
                                name = Some(read_text(el, self.source));
                                expect = false;
                            }
                        }
                        if let Some(name) = name.filter(|s| !s.is_empty()) {
                            let bare = EXT.replace(&name, "");
                            let bare = bare
                                .rsplit('/')
                                .next()
                                .unwrap_or("")
                                .rsplit('\\')
                                .next()
                                .unwrap_or("");
                            if !bare.is_empty() {
                                self.edge(
                                    &self.file.clone(),
                                    &make_id([bare]),
                                    "imports_from",
                                    line,
                                    None,
                                );
                            }
                        }
                    } else if NAMED_BLOCK_COMMANDS.contains(&text.as_str()) {
                        // A file-scope `begin { }` (and process/end/...) body is a
                        // script_block_expression under command_elements: walk it
                        // so nested functions, classes and enums are found (#4270).
                        for n in children(node) {
                            self.walk(n, class);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n, class);
        }
    }
    /// The class that provably owns a member call's method: `$this.M()` in a
    /// uniquely named class, or `[T]::M()` naming one unique class literally.
    /// Value, expression and dynamic receivers prove nothing (#4163).
    fn receiver_class(&self, node: Node<'_>, class: Option<&str>) -> Option<String> {
        let receiver = node.child(0)?;
        let operator = node.child(1).map(|n| n.kind());
        if receiver.kind() == "variable" && operator == Some(".") {
            let class = class.filter(|s| !s.is_empty())?;
            if read_text(receiver, self.source).to_lowercase() != "$this" {
                return None;
            }
            let name = self.class_names_by_id.get(class)?;
            return (self.class_ids_by_name.get(name).map_or(0, Vec::len) == 1)
                .then(|| class.to_string());
        }
        if receiver.kind() == "type_literal" && operator == Some("::") {
            // The whole literal: a partial parse can hide type qualifiers.
            if receiver.has_error() {
                return None;
            }
            let text = read_text(receiver, self.source);
            let mut inner = text.chars();
            inner.next();
            inner.next_back();
            let name = trim(inner.as_str());
            if name.is_empty() {
                return None;
            }
            return match self.class_ids_by_name.get(&name.to_lowercase()) {
                Some(ids) if ids.len() == 1 => Some(ids[0].clone()),
                _ => None,
            };
        }
        None
    }
    fn calls(&mut self, node: Node<'_>, caller: &str, class: Option<&str>) {
        if matches!(node.kind(), "function_statement" | "class_statement") {
            return;
        }
        if node.kind() == "command"
            && let Some(name) = child(node, "command_name")
        {
            let text = read_text(name, self.source);
            let lower = text.to_lowercase();
            if !SKIP.contains(&lower.as_str()) {
                if let Some(target) = self
                    .labels
                    .get(&lower)
                    .filter(|s| !s.is_empty() && *s != caller)
                    .cloned()
                {
                    if self.call_pairs.insert((caller.into(), target.clone())) {
                        self.edge(
                            caller,
                            &target,
                            "calls",
                            node.start_position().row + 1,
                            None,
                        );
                    }
                } else if !text.is_empty() {
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":text,"is_member_call":false,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                }
            }
        } else if node.kind() == "invokation_expression"
            && let Some(member) = child(node, "member_name")
        {
            // `$this.M()` and `[C]::M()` bind only to a method of the class
            // the receiver proves, never by name alone (#3992, #4163).
            let name = read_text(member, self.source);
            let line = node.start_position().row + 1;
            let mut named = children(member).filter(|n| n.is_named());
            let literal =
                named.next().is_some_and(|n| n.kind() == "simple_name") && named.next().is_none();
            let target = self
                .receiver_class(node, class)
                .filter(|_| literal)
                .and_then(|owner| self.methods_by_class.get(&owner)?.get(&name.to_lowercase()))
                .filter(|s| !s.is_empty())
                .cloned();
            match target {
                Some(target)
                    if target != caller
                        && self.call_pairs.insert((caller.into(), target.clone())) =>
                {
                    self.edge(caller, &target, "calls", line, None);
                }
                None if !name.is_empty() => {
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":name,"is_member_call":true,"source_file":self.path,"source_location":format!("L{line}")}));
                }
                _ => {}
            }
        }
        for n in children(node) {
            self.calls(n, caller, class);
        }
    }
    fn manifest_import(&mut self, raw: &str, line: usize) {
        let name = psd1_module_name(raw);
        if !name.is_empty() {
            self.edge(
                &self.file.clone(),
                &make_id([name.as_str()]),
                "imports_from",
                line,
                Some("import"),
            );
        }
    }
    fn manifest(&mut self, node: Node<'_>) {
        if node.kind() != "hash_entry" {
            for n in children(node) {
                self.manifest(n);
            }
            return;
        }
        let Some(key) = child(node, "key_expression") else {
            return;
        };
        let key = String::from_utf8_lossy(&self.source[key.byte_range()]);
        let key = trim(&key);
        if !["RootModule", "NestedModules", "RequiredModules"].contains(&key) {
            return;
        }
        let Some(value) = child(node, "pipeline") else {
            return;
        };
        let line = node.start_position().row + 1;
        if key != "RequiredModules" {
            for text in psd1_collect_string_literals(value, self.source) {
                self.manifest_import(&text, line);
            }
            return;
        }
        let mut names = Vec::new();
        let mut inside = HashSet::new();
        modulename_entries(value, self.source, &mut names, &mut inside);
        for n in string_nodes(value)
            .into_iter()
            .filter(|n| !inside.contains(&n.start_byte()))
        {
            let text = literal(n, self.source);
            self.manifest_import(&text, line);
        }
        for text in names {
            self.manifest_import(&text, line);
        }
    }
}
fn literal(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.byte_range()])
        .trim_matches(['\'', '"'])
        .into()
}
fn string_nodes(node: Node<'_>) -> Vec<Node<'_>> {
    if node.kind() == "string_literal" {
        vec![node]
    } else {
        children(node).flat_map(string_nodes).collect()
    }
}
pub fn psd1_collect_string_literals(node: Node<'_>, source: &[u8]) -> Vec<String> {
    string_nodes(node)
        .into_iter()
        .map(|n| literal(n, source))
        .collect()
}
pub fn psd1_module_name(raw: &str) -> String {
    let name = raw.replace('\\', "/");
    let name = name.rsplit('/').next().unwrap_or("");
    trim(&EXT.replace(name, "")).into()
}
fn modulename_entries(
    node: Node<'_>,
    source: &[u8],
    names: &mut Vec<String>,
    inside: &mut HashSet<usize>,
) {
    if node.kind() == "hash_entry" {
        if let Some(key) = child(node, "key_expression") {
            let key = String::from_utf8_lossy(&source[key.byte_range()]);
            for pipeline in children(node).filter(|n| n.kind() == "pipeline") {
                for n in string_nodes(pipeline) {
                    inside.insert(n.start_byte());
                }
                if trim(&key) == "ModuleName" {
                    names.extend(psd1_collect_string_literals(pipeline, source));
                }
            }
        }
        return;
    }
    for n in children(node) {
        modulename_entries(n, source, names, inside);
    }
}
pub fn extract_powershell(path: &Path) -> Value {
    extract(path, false)
}
pub fn extract_powershell_manifest(path: &Path) -> Value {
    extract(path, true)
}
fn extract(path: &Path, manifest: bool) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&match crate::grammar!(
        "grammar-powershell",
        "tree_sitter_powershell",
        tree_sitter_powershell::LANGUAGE
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":e.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"PowerShell parse cancelled"});
    };
    let path_text = fsutil::path_string(path);
    let file = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let name = path_syntax::posix_name(&parts).to_string();
    let mut ex = Extractor {
        source: &source,
        path: path_text,
        stem: file_stem(path),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        labels: HashMap::new(),
        class_ids_by_name: HashMap::new(),
        class_names_by_id: HashMap::new(),
        methods_by_class: HashMap::new(),
        call_pairs: HashSet::new(),
        raw_calls: vec![],
    };
    ex.node(&file, &name, 1);
    if manifest {
        ex.manifest(tree.root_node());
    } else {
        ex.walk(tree.root_node(), None);
        ex.labels = ex
            .nodes
            .iter()
            .filter_map(|n| {
                Some((
                    n["label"]
                        .as_str()?
                        .trim_matches(['(', ')'])
                        .trim_start_matches('.')
                        .to_lowercase(),
                    n["id"].as_str()?.into(),
                ))
            })
            .collect();
        for (caller, body, class) in std::mem::take(&mut ex.bodies) {
            ex.calls(body, &caller, class.as_deref());
        }
        // Script-scope commands are called by the file itself; the walk stops
        // at function and class statements, so no body is walked twice (#4356).
        ex.calls(tree.root_node(), &file, None);
        ex.edges.retain(|e| {
            e["source"].as_str().is_some_and(|s| ex.seen.contains(s))
                && (e["target"].as_str().is_some_and(|s| ex.seen.contains(s))
                    || matches!(e["relation"].as_str(), Some("imports_from" | "imports")))
        });
    }
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls})
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-powershell")]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_powershell::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 419);
        assert_eq!(l.parse_state_count(), 2142);
        assert_eq!(l.abi_version(), 15);
    }
}
