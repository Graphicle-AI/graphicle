// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/vbnet.py
//! Port of graphify/extractors/vbnet.py (Graphify 0.9.69, #3717): namespaces,
//! classes/modules/interfaces/structures/enums, members, `Handles` clauses and
//! arity-checked calls. VB identifiers are case-insensitive, so keys casefold.
//! Calls on another file's part of a partial type resolve through
//! [`resolve_vbnet_partial_calls`].
use super::base::{file_stem, make_id, read_text};
use crate::{dedup_strings::casefold, fsutil, io_compat, path_syntax, python_compat};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;

fn type_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "class_block" => "class",
        "module_block" => "module",
        "interface_block" => "interface",
        "structure_block" => "structure",
        "enum_block" => "enum",
        _ => return None,
    })
}
fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
struct Extractor<'t, 's> {
    source: &'s [u8],
    path: String,
    stem: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    seen: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
    types_by_name: HashMap<String, String>,
    /// Unique short names are a fallback only after namespace lookup.
    owner_by_short: HashMap<String, Option<String>>,
    owners: HashSet<String>,
    methods: HashMap<(String, String, usize), Vec<String>>,
    events: HashMap<(String, String), String>,
    bodies: Vec<(Node<'t>, String, String, String)>,
    pending_handles: Vec<(String, String, String, Node<'t>)>,
}
impl<'t> Extractor<'t, '_> {
    fn text(&self, node: Node<'_>) -> String {
        read_text(node, self.source)
    }
    fn qualified_owner(&self, receiver: &str, caller: &str) -> Option<String> {
        if let Some(global) = receiver.strip_prefix("global.") {
            return self.owners.get(global).cloned();
        }
        // A relative type name first searches the caller's namespace and its
        // parents; an explicit qualifier must never collapse to its last token.
        let mut scope = caller;
        while let Some((parent, _)) = scope.rsplit_once('.') {
            let candidate = format!("{parent}.{receiver}");
            if self.owners.contains(&candidate) {
                return Some(candidate);
            }
            scope = parent;
        }
        if let Some(owner) = self.owners.get(receiver) {
            return Some(owner.clone());
        }
        if receiver.contains('.') {
            return None;
        }
        self.owner_by_short.get(receiver).cloned().flatten()
    }
    #[allow(clippy::too_many_arguments)]
    fn add_node(
        &mut self,
        id: &str,
        label: &str,
        node: Node<'_>,
        kind: &str,
        source_backed: bool,
        callable: bool,
        metadata: &[(&str, Value)],
    ) -> String {
        if self.seen.insert(id.into()) {
            let mut details = Map::new();
            details.insert("language".into(), json!("vbnet"));
            details.insert("kind".into(), json!(kind));
            for (key, value) in metadata {
                details.insert((*key).into(), value.clone());
            }
            let mut item = Map::new();
            item.insert("id".into(), json!(id));
            item.insert("label".into(), json!(label));
            item.insert("file_type".into(), json!("code"));
            item.insert(
                "source_location".into(),
                json!(format!("L{}", node.start_position().row + 1)),
            );
            item.insert("metadata".into(), Value::Object(details));
            if source_backed {
                item.insert("source_file".into(), json!(self.path));
            }
            if callable {
                item.insert("_callable".into(), json!(true));
            }
            self.nodes.push(Value::Object(item));
        }
        id.into()
    }
    fn add_edge(&mut self, source: &str, target: &str, relation: &str, node: Node<'_>) {
        if source.is_empty()
            || target.is_empty()
            || source == target
            || !self
                .seen_edges
                .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1),"weight":1.0}));
    }
    fn type_reference(&mut self, name: &str, node: Node<'_>) -> String {
        let short = name.rsplit('.').next().unwrap_or("").to_string();
        if let Some(target) = self.types_by_name.get(&casefold(&short)) {
            return target.clone();
        }
        self.add_node(
            &make_id(["vbnet", "type", casefold(name).as_str()]),
            &short,
            node,
            "external_type",
            false,
            false,
            &[],
        )
    }
    fn add_data_member(
        &mut self,
        type_id: &str,
        member: Node<'_>,
        name: &str,
        kind: &str,
        relation: &str,
    ) -> String {
        let id = self.add_node(
            &make_id([
                type_id,
                kind,
                casefold(name).as_str(),
                member.start_position().row.to_string().as_str(),
            ]),
            name,
            member,
            kind,
            true,
            false,
            &[],
        );
        self.add_edge(type_id, &id, relation, member);
        id
    }
    fn process_type(&mut self, block: Node<'t>, parent: &str, namespace: &str) {
        let kind = type_kind(block.kind()).unwrap_or_default();
        let Some(name_node) = block.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let full_name = if namespace.is_empty() {
            name.clone()
        } else {
            format!("{namespace}.{name}")
        };
        let owner = casefold(&full_name);
        let stem = self.stem.clone();
        let type_id = self.add_node(
            &make_id([stem.as_str(), "type", owner.as_str()]),
            &name,
            block,
            kind,
            true,
            matches!(kind, "class" | "structure"),
            &[("name", json!(name)), ("full_name", json!(full_name))],
        );
        self.add_edge(parent, &type_id, "contains", block);
        self.types_by_name.insert(casefold(&name), type_id.clone());
        self.owner_by_short
            .entry(casefold(&name))
            .and_modify(|known| {
                if known.as_deref() != Some(owner.as_str()) {
                    *known = None;
                }
            })
            .or_insert_with(|| Some(owner.clone()));
        self.owners.insert(owner.clone());
        for clause in named(block) {
            let relation = match clause.kind() {
                "inherits_clause" => "inherits",
                "implements_clause" => "implements",
                _ => continue,
            };
            for type_node in named(clause).into_iter().filter(|c| c.kind() == "type") {
                let referenced = self.text(type_node);
                let target = self.type_reference(&referenced, type_node);
                self.add_edge(&type_id, &target, relation, clause);
            }
        }
        for member in named(block) {
            match member.kind() {
                "enum_member" => {
                    if let Some(member_name) = member.child_by_field_name("name") {
                        let member_name = self.text(member_name);
                        // An enum member is a discriminant case, not a contained
                        // declaration (Graphify 0.9.76, 67a96aa); `case_of` targets
                        // are also never what a `New X()` constructs.
                        self.add_data_member(
                            &type_id,
                            member,
                            &member_name,
                            "enum_member",
                            "case_of",
                        );
                    }
                    continue;
                }
                "field_declaration" => {
                    for declarator in named(member)
                        .into_iter()
                        .filter(|c| c.kind() == "variable_declarator")
                    {
                        if let Some(member_name) = declarator.child_by_field_name("name") {
                            let member_name = self.text(member_name);
                            self.add_data_member(
                                &type_id,
                                declarator,
                                &member_name,
                                "field",
                                "contains",
                            );
                        }
                    }
                    continue;
                }
                "property_declaration" | "event_declaration" => {
                    if let Some(member_name) = member.child_by_field_name("name") {
                        let data_name = self.text(member_name);
                        let event = member.kind() == "event_declaration";
                        let id = self.add_data_member(
                            &type_id,
                            member,
                            &data_name,
                            if event { "event" } else { "property" },
                            "contains",
                        );
                        if event {
                            self.events
                                .insert((owner.clone(), casefold(&data_name)), id);
                        }
                    }
                    continue;
                }
                "method_declaration" | "constructor_declaration" => {}
                _ => continue,
            }
            let (method_name, method_kind) = if member.kind() == "constructor_declaration" {
                ("New".to_string(), "constructor")
            } else {
                let Some(name_node) = member.child_by_field_name("name") else {
                    continue;
                };
                (self.text(name_node), "method")
            };
            let parameters = member
                .child_by_field_name("parameters")
                .map(|p| {
                    named(p)
                        .into_iter()
                        .filter(|c| c.kind() == "parameter")
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let arity = parameters.len();
            let required = parameters
                .iter()
                .filter(|p| !casefold(self.text(**p).trim_start()).starts_with("optional "))
                .count();
            let accepted = (required..=arity).collect::<Vec<_>>();
            let method_id = self.add_node(
                &make_id([
                    type_id.as_str(),
                    method_kind,
                    casefold(&method_name).as_str(),
                    arity.to_string().as_str(),
                    member.start_position().row.to_string().as_str(),
                ]),
                &format!("{method_name}()"),
                member,
                method_kind,
                true,
                true,
                &[
                    ("owner", json!(owner)),
                    ("name", json!(method_name)),
                    ("arity", json!(arity)),
                    ("accepted_arities", json!(accepted)),
                ],
            );
            self.add_edge(&type_id, &method_id, "method", member);
            for accepted in &accepted {
                self.methods
                    .entry((owner.clone(), casefold(&method_name), *accepted))
                    .or_default()
                    .push(method_id.clone());
            }
            for clause in named(member)
                .into_iter()
                .filter(|c| c.kind() == "handles_clause")
            {
                for handled in named(clause)
                    .into_iter()
                    .filter(|c| c.kind() == "namespace_name")
                {
                    let written = self.text(handled);
                    self.pending_handles
                        .push((method_id.clone(), owner.clone(), written, handled));
                }
            }
            self.bodies
                .push((member, method_id, owner.clone(), name.clone()));
        }
    }
    fn scan(&mut self, node: Node<'t>, parent: &str, namespace: &str) {
        if node.kind() == "imports_statement" {
            return;
        }
        if node.kind() == "namespace_block" {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let local = self.text(name_node);
            let full = if namespace.is_empty() {
                local.clone()
            } else {
                format!("{namespace}.{local}")
            };
            let stem = self.stem.clone();
            let id = self.add_node(
                &make_id([stem.as_str(), "namespace", casefold(&full).as_str()]),
                &local,
                node,
                "namespace",
                true,
                false,
                &[("full_name", json!(full))],
            );
            self.add_edge(parent, &id, "contains", node);
            for child in named(node) {
                if child.id() != name_node.id() {
                    self.scan(child, &id, &full);
                }
            }
            return;
        }
        if type_kind(node.kind()).is_some() {
            self.process_type(node, parent, namespace);
            // Nested type declarations are the only child containers that still
            // need recursion after this type's own members.
            for child in named(node) {
                if child.kind() == "type_declaration" {
                    self.scan(child, parent, namespace);
                }
            }
            return;
        }
        for child in named(node) {
            self.scan(child, parent, namespace);
        }
    }
}
pub fn extract_vbnet(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":format!("VB.NET grammar failed to load: {}", io_compat::read_error(&error, path))});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&graphicle_tree_sitter_vb_dotnet::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":format!("VB.NET grammar failed to load: {error}")});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"VB.NET grammar failed to load: parse cancelled"});
    };
    let root = tree.root_node();
    let path_text = fsutil::path_string(path);
    let file_id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let file_name = path_syntax::posix_name(&parts).to_string();
    let mut x = Extractor {
        source: &source,
        path: path_text,
        stem: file_stem(path),
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        seen: HashSet::new(),
        seen_edges: HashSet::new(),
        types_by_name: HashMap::new(),
        owner_by_short: HashMap::new(),
        owners: HashSet::new(),
        methods: HashMap::new(),
        events: HashMap::new(),
        bodies: vec![],
        pending_handles: vec![],
    };
    x.add_node(&file_id, &file_name, root, "file", true, false, &[]);
    for statement in named(root)
        .into_iter()
        .filter(|n| n.kind() == "imports_statement")
    {
        let Some(namespace_node) = named(statement)
            .into_iter()
            .find(|c| c.kind() == "namespace_name")
        else {
            continue;
        };
        let namespace = x.text(namespace_node);
        let target = x.add_node(
            &make_id(["vbnet", "namespace", casefold(&namespace).as_str()]),
            &namespace,
            namespace_node,
            "external_namespace",
            false,
            false,
            &[],
        );
        x.add_edge(&file_id, &target, "imports", statement);
    }
    x.scan(root, &file_id, "");
    for (method, owner, written, handled) in std::mem::take(&mut x.pending_handles) {
        let event = written.rsplit('.').next().unwrap_or("");
        let target = match x.events.get(&(owner, casefold(event))) {
            Some(target) => target.clone(),
            None => x.add_node(
                &make_id(["vbnet", "event", casefold(&written).as_str()]),
                &written,
                handled,
                "external_event",
                false,
                false,
                &[],
            ),
        };
        x.add_edge(&method, &target, "handles", handled);
    }
    for (body, caller, owner, type_name) in std::mem::take(&mut x.bodies) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.id() != body.id()
                && matches!(
                    node.kind(),
                    "method_declaration" | "constructor_declaration" | "property_declaration"
                )
            {
                continue;
            }
            if node.kind() == "invocation"
                && let Some(target) = node.child_by_field_name("target")
            {
                let written = x.text(target);
                let parts = written.split('.').collect::<Vec<_>>();
                let receiver = casefold(&parts[..parts.len() - 1].join("."));
                let callee = parts[parts.len() - 1].to_string();
                let self_receiver = receiver.is_empty()
                    || receiver == "me"
                    || receiver == "myclass"
                    || receiver == casefold(&type_name);
                // Self/unqualified calls target the caller's own type; a qualified
                // call (`Helpers.Log()`) targets the named type/module. A value
                // receiver (a local variable) has no known owner and stays unresolved.
                let target_owner = if self_receiver {
                    Some(owner.clone())
                } else {
                    x.qualified_owner(&receiver, &owner)
                };
                if let Some(target_owner) = target_owner {
                    let arity = node
                        .child_by_field_name("arguments")
                        .map_or(0, |a| a.named_child_count());
                    match x
                        .methods
                        .get(&(target_owner.clone(), casefold(&callee), arity))
                        .map(Vec::as_slice)
                    {
                        Some([target]) => {
                            let target = target.clone();
                            x.add_edge(&caller, &target, "calls", node);
                        }
                        _ => {
                            x.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"arity":arity,"owner":target_owner,"is_member_call":true,"language":"vbnet","source_file":x.path,"source_location":format!("L{}",node.start_position().row+1)}));
                        }
                    }
                }
            }
            let mut kids = named(node);
            kids.reverse();
            stack.extend(kids);
        }
    }
    let edges = std::mem::take(&mut x.edges)
        .into_iter()
        .filter(|e| {
            x.seen.contains(e["source"].as_str().unwrap_or(""))
                && x.seen.contains(e["target"].as_str().unwrap_or(""))
        })
        .collect::<Vec<_>>();
    json!({"nodes":x.nodes,"edges":edges,"raw_calls":x.raw_calls})
}

/// Bind calls to the one same-named, arity-compatible method of the same
/// (possibly partial) owner type anywhere in the corpus.
pub fn resolve_vbnet_partial_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    let mut methods: HashMap<(String, String, i64), Vec<Value>> = HashMap::new();
    for node in nodes {
        let metadata = &node["metadata"];
        if metadata["language"] != "vbnet"
            || !matches!(metadata["kind"].as_str(), Some("method" | "constructor"))
        {
            continue;
        }
        let (Some(owner), Some(name), Some(accepted)) = (
            metadata["owner"].as_str(),
            metadata["name"].as_str(),
            metadata["accepted_arities"].as_array(),
        ) else {
            continue;
        };
        for arity in accepted.iter().filter_map(Value::as_i64) {
            methods
                .entry((casefold(owner), casefold(name), arity))
                .or_default()
                .push(node["id"].clone());
        }
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
        if call["language"] != "vbnet" || !crate::build::truthy(&call["owner"]) {
            continue;
        }
        let key = (
            casefold(&python_compat::string(&call["owner"])),
            casefold(
                &call
                    .get("callee")
                    .map(python_compat::string)
                    .unwrap_or_default(),
            ),
            call.get("arity").and_then(Value::as_i64).unwrap_or(-1),
        );
        let caller = call["caller_nid"].clone();
        let Some([target]) = methods.get(&key).map(Vec::as_slice) else {
            continue;
        };
        if *target == caller || !existing.insert((caller.clone(), target.clone())) {
            continue;
        }
        edges.push(json!({"source":caller,"target":target,"relation":"calls","context":"partial_type_call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":call.get("source_file").cloned().unwrap_or(json!("")),"source_location":call["source_location"],"weight":1.0}));
    }
}
