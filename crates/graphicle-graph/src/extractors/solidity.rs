// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/solidity.py
//! Port of graphify/extractors/solidity.py (Graphify 0.9.69, #3716):
//! contracts, interfaces and libraries with their state, events, errors,
//! structs, enums, functions, modifiers and imports. Inheritance and `using`
//! references bind across imported files through
//! [`resolve_solidity_type_references`]; calls, `super` calls and modifiers a
//! contract inherits bind through [`resolve_solidity_inherited_calls`]
//! (Graphify 0.9.92, #4379).
use super::base::{file_stem, make_id, read_text};
use crate::{
    extract::per_file::PerFile,
    fsutil, io_compat, path_syntax, python_compat,
    resolver_registry::{Rec, append_edges, at, edge_record},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;

const TYPE_KINDS: [&str; 3] = ["contract", "interface", "library"];
fn type_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "contract_declaration" => "contract",
        "interface_declaration" => "interface",
        "library_declaration" => "library",
        _ => return None,
    })
}
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
struct Extractor<'s> {
    source: &'s [u8],
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    raw_calls: Vec<Value>,
    seen: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
}
impl Extractor<'_> {
    fn text(&self, node: Node<'_>) -> String {
        read_text(node, self.source)
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
        arity: Option<usize>,
    ) -> String {
        if self.seen.insert(id.into()) {
            let mut item = Map::new();
            item.insert("id".into(), json!(id));
            item.insert("label".into(), json!(label));
            item.insert("file_type".into(), json!("code"));
            item.insert(
                "source_location".into(),
                json!(format!("L{}", node.start_position().row + 1)),
            );
            let mut metadata = json!({"language":"solidity","kind":kind});
            if let Some(arity) = arity {
                // The parameter count, so the cross-contract pass picks the
                // overload the in-contract lookup would (name, arity).
                metadata["arity"] = json!(arity);
            }
            item.insert("metadata".into(), metadata);
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
    fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        node: Node<'_>,
        context: Option<&str>,
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
        let mut item = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1),"weight":1.0});
        if let Some(context) = context.filter(|c| !c.is_empty()) {
            item["context"] = json!(context);
        }
        if let Some(target_file) = target_file.filter(|t| !t.is_empty()) {
            item["target_file"] = json!(target_file);
        }
        self.edges.push(item);
    }
    /// `_inherited_call`: a call or modifier the contract itself does not
    /// declare, for [`resolve_solidity_inherited_calls`]. `kind` is `inherited`
    /// (bare `f()` / `this.f()`), `super` (`super.f()`) or `modifier`.
    fn inherited_call(
        &mut self,
        caller: &str,
        callee: &str,
        arity: Option<usize>,
        kind: &str,
        node: Node<'_>,
    ) {
        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"arity":arity,"solidity_call":kind,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
    }
    fn type_stub(&mut self, name: &str, node: Node<'_>) -> String {
        let name = name.rsplit('.').next().unwrap_or("").to_string();
        let id = make_id(["solidity", "type", name.as_str()]);
        self.add_node(&id, &name, node, "external_type", false, false, None)
    }
    /// Callables carry an `arity:types` signature to tell overloads apart;
    /// other members are unique per kind by name within a contract.
    #[allow(clippy::too_many_arguments)]
    fn add_member(
        &mut self,
        type_id: &str,
        member: Node<'_>,
        name: &str,
        label: &str,
        kind: &str,
        relation: &str,
        signature: &str,
        callable: bool,
        arity: Option<usize>,
    ) -> String {
        let discriminator = if signature.is_empty() {
            name
        } else {
            signature
        };
        let id = make_id([type_id, kind, name, discriminator]);
        self.add_node(&id, label, member, kind, true, callable, arity);
        self.add_edge(type_id, &id, relation, member, None, None);
        id
    }
    fn declaration(&mut self, declaration: Node<'_>, kind: &str, stem: &str, file_id: &str) {
        let (Some(name_node), Some(body)) = (
            declaration.child_by_field_name("name"),
            declaration.child_by_field_name("body"),
        ) else {
            return;
        };
        let type_name = self.text(name_node);
        let type_id = self.add_node(
            &make_id([stem, kind, type_name.as_str()]),
            &type_name,
            declaration,
            kind,
            true,
            kind == "contract",
            None,
        );
        self.add_edge(file_id, &type_id, "contains", declaration, None, None);
        for child in named(declaration) {
            if child.kind() != "inheritance_specifier" {
                continue;
            }
            let Some(ancestor) = child.child_by_field_name("ancestor") else {
                continue;
            };
            let base = self
                .text(ancestor)
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string();
            let stub = self.type_stub(&base, child);
            let context = format!("solidity_type:{base}");
            self.add_edge(&type_id, &stub, "inherits", child, Some(&context), None);
        }
        let mut functions: HashMap<(String, usize), Vec<String>> = HashMap::new();
        let mut modifiers: HashMap<String, String> = HashMap::new();
        let mut references: HashMap<(String, usize), String> = HashMap::new();
        let mut bodies = vec![];
        let mut pending_modifiers = vec![];
        for member in named(body) {
            let kind = member.kind();
            if kind == "using_directive" {
                let alias = named(member)
                    .into_iter()
                    .find(|c| matches!(c.kind(), "type_alias" | "identifier" | "user_defined_type"))
                    .map(|c| self.text(c))
                    .unwrap_or_default();
                if !alias.is_empty() {
                    let name = alias.rsplit('.').next().unwrap_or("").to_string();
                    let stub = self.type_stub(&name, member);
                    let context = format!("solidity_type:{name}");
                    self.add_edge(&type_id, &stub, "uses", member, Some(&context), None);
                }
                continue;
            }
            if kind == "struct_declaration" || kind == "enum_declaration" {
                let Some(name_node) = member.child_by_field_name("name") else {
                    continue;
                };
                let name = self.text(name_node);
                let member_body = member.child_by_field_name("body");
                let is_struct = kind == "struct_declaration";
                let owner = self.add_member(
                    &type_id,
                    member,
                    &name,
                    &name,
                    if is_struct { "struct" } else { "enum" },
                    "contains",
                    "",
                    false,
                    None,
                );
                for item in member_body.map(named).unwrap_or_default() {
                    if is_struct {
                        if item.kind() != "struct_member" {
                            continue;
                        }
                        let Some(field) = item.child_by_field_name("name") else {
                            continue;
                        };
                        let field = self.text(field);
                        let id = make_id([owner.as_str(), "field", field.as_str()]);
                        self.add_node(&id, &field, item, "field", true, false, None);
                        self.add_edge(&owner, &id, "contains", item, None, None);
                    } else {
                        if item.kind() != "enum_value" {
                            continue;
                        }
                        let value = self.text(item);
                        let id = make_id([owner.as_str(), value.as_str()]);
                        self.add_node(&id, &value, item, "enum_value", true, false, None);
                        // A value is a discriminant case, not a declared member;
                        // constructor binding skips case_of targets (0.9.77).
                        self.add_edge(&owner, &id, "case_of", item, None, None);
                    }
                }
                continue;
            }
            let simple = match kind {
                "event_definition" => Some("event"),
                "error_declaration" => Some("error"),
                "state_variable_declaration" => Some("state_variable"),
                _ => None,
            };
            if let Some(simple) = simple {
                let Some(name_node) = member.child_by_field_name("name") else {
                    continue;
                };
                let name = self.text(name_node);
                let id = self.add_member(
                    &type_id, member, &name, &name, simple, "contains", "", false, None,
                );
                let arity = named(member)
                    .into_iter()
                    .filter(|c| matches!(c.kind(), "event_parameter" | "parameter"))
                    .count();
                references.insert((name, arity), id);
                continue;
            }
            if !matches!(
                kind,
                "function_definition"
                    | "constructor_definition"
                    | "modifier_definition"
                    | "fallback_receive_definition"
            ) {
                continue;
            }
            let (name, callable_kind) = match kind {
                "constructor_definition" => ("constructor".to_string(), "constructor"),
                "fallback_receive_definition" => {
                    if self.text(member).trim_start().starts_with("receive") {
                        ("receive".to_string(), "receive")
                    } else {
                        ("fallback".to_string(), "fallback")
                    }
                }
                _ => {
                    let Some(name_node) = member.child_by_field_name("name") else {
                        continue;
                    };
                    let callable_kind = if kind == "modifier_definition" {
                        "modifier"
                    } else {
                        "function"
                    };
                    (self.text(name_node), callable_kind)
                }
            };
            let parameters = named(member)
                .into_iter()
                .filter(|c| matches!(c.kind(), "parameter" | "fallback_receive_parameter"))
                .collect::<Vec<_>>();
            let signature = parameters
                .iter()
                .filter_map(|p| p.child_by_field_name("type"))
                .map(|t| self.text(t))
                .collect::<Vec<_>>()
                .join(",");
            let arity = parameters.len();
            let id = self.add_member(
                &type_id,
                member,
                &name,
                &format!("{name}()"),
                callable_kind,
                "method",
                &format!("{arity}:{signature}"),
                true,
                Some(arity),
            );
            if callable_kind == "modifier" {
                modifiers.insert(name.clone(), id.clone());
            } else {
                functions
                    .entry((name.clone(), arity))
                    .or_default()
                    .push(id.clone());
            }
            if let Some(member_body) = member.child_by_field_name("body") {
                bodies.push((member_body, id.clone()));
            }
            // A constructor's `ERC20("n", "s")` base call is a modifier
            // invocation too (0.9.92).
            if callable_kind != "modifier" {
                for child in named(member)
                    .into_iter()
                    .filter(|c| c.kind() == "modifier_invocation")
                {
                    let modifier = named(child)
                        .into_iter()
                        .find(|c| c.kind() == "identifier")
                        .map(|c| self.text(c))
                        .unwrap_or_default();
                    if !modifier.is_empty() {
                        pending_modifiers.push((id.clone(), modifier, child));
                    }
                }
            }
        }
        for (caller, modifier, node) in pending_modifiers {
            if let Some(target) = modifiers.get(&modifier).cloned() {
                self.add_edge(&caller, &target, "uses", node, None, None);
            } else {
                // Declared in a base contract (`onlyOwner` from Ownable), or a
                // base-constructor call the inheritance pass finds no modifier for.
                self.inherited_call(&caller, &modifier, None, "modifier", node);
            }
        }
        for (body, caller) in bodies {
            let mut stack = vec![body];
            while let Some(node) = stack.pop() {
                if node.kind() == "call_expression"
                    && let Some(callee_node) = node.child_by_field_name("function")
                {
                    let written = callee_text(callee_node, self.source);
                    let callee = written.rsplit('.').next().unwrap_or("").to_string();
                    let arity = named(node)
                        .into_iter()
                        .filter(|c| c.kind() == "call_argument")
                        .count();
                    let dotted = written.contains('.');
                    let candidates = if !dotted || written.starts_with("this.") {
                        functions
                            .get(&(callee.clone(), arity))
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        vec![]
                    };
                    if let [target] = candidates.as_slice() {
                        self.add_edge(&caller, target, "calls", node, None, None);
                    } else if let Some(referenced) = references
                        .get(&(callee.clone(), arity))
                        .filter(|_| !dotted)
                        .cloned()
                    {
                        self.add_edge(&caller, &referenced, "references", node, None, None);
                    } else if candidates.is_empty()
                        && !callee.is_empty()
                        && (!dotted || written.starts_with("this."))
                    {
                        // Not declared in this contract: an inherited function
                        // (`_msgSender()` from Context), bound through the
                        // contract's linearized bases.
                        self.inherited_call(&caller, &callee, Some(arity), "inherited", node);
                    } else if written.starts_with("super.") && written.matches('.').count() == 1 {
                        self.inherited_call(&caller, &callee, Some(arity), "super", node);
                    } else if !callee.is_empty() && dotted {
                        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                    }
                }
                let mut kids = named(node);
                kids.reverse();
                stack.extend(kids);
            }
        }
    }
    /// File-level (free) functions, legal since Solidity 0.7, and the calls
    /// they make. Only an unambiguous same-file free function is linked
    /// directly; member calls (`Lib.f()`) are left for cross-file resolution.
    fn free_functions(&mut self, root: Node<'_>, stem: &str, file_id: &str) {
        let mut functions: HashMap<(String, usize), Vec<String>> = HashMap::new();
        let mut bodies = vec![];
        for declaration in named(root) {
            if declaration.kind() != "function_definition" {
                continue;
            }
            let Some(name_node) = declaration.child_by_field_name("name") else {
                continue;
            };
            let name = self.text(name_node);
            let parameters = named(declaration)
                .into_iter()
                .filter(|c| c.kind() == "parameter")
                .collect::<Vec<_>>();
            let signature = parameters
                .iter()
                .filter_map(|p| p.child_by_field_name("type"))
                .map(|t| self.text(t))
                .collect::<Vec<_>>()
                .join(",");
            let arity = parameters.len();
            let id = self.add_node(
                &make_id([
                    stem,
                    "function",
                    name.as_str(),
                    format!("{arity}:{signature}").as_str(),
                ]),
                &format!("{name}()"),
                declaration,
                "function",
                true,
                true,
                None,
            );
            self.add_edge(file_id, &id, "contains", declaration, None, None);
            functions.entry((name, arity)).or_default().push(id.clone());
            if let Some(body) = declaration.child_by_field_name("body") {
                bodies.push((body, id));
            }
        }
        for (body, caller) in bodies {
            let mut stack = vec![body];
            while let Some(node) = stack.pop() {
                if node.kind() == "call_expression"
                    && let Some(callee_node) = node.child_by_field_name("function")
                {
                    let written = callee_text(callee_node, self.source);
                    let callee = written.rsplit('.').next().unwrap_or("").to_string();
                    let arity = named(node)
                        .into_iter()
                        .filter(|c| c.kind() == "call_argument")
                        .count();
                    if !written.contains('.') {
                        if let Some([target]) = functions.get(&(callee, arity)).map(Vec::as_slice) {
                            let target = target.clone();
                            self.add_edge(&caller, &target, "calls", node, None, None);
                        }
                    } else if !callee.is_empty() {
                        self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"language":"solidity","source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                    }
                }
                let mut kids = named(node);
                kids.reverse();
                stack.extend(kids);
            }
        }
    }
}
/// `_callee_text`: the callee as written (`super.supportsInterface`,
/// `_check`). The grammar parses `a || super.f(x)` as a call of the member
/// `(a || super).f`; the right-most operand is read as the receiver instead.
fn callee_text(callee: Node<'_>, source: &[u8]) -> String {
    fn unwrap<'t>(node: Node<'t>) -> Node<'t> {
        if node.kind() == "expression" {
            named(node).into_iter().next().unwrap_or(node)
        } else {
            node
        }
    }
    let inner = unwrap(callee);
    if inner.kind() == "member_expression"
        && let (Some(object), Some(property)) = (
            inner.child_by_field_name("object"),
            inner.child_by_field_name("property"),
        )
    {
        let mut base = object;
        loop {
            let unwrapped = unwrap(base);
            let right = if unwrapped.kind() == "binary_expression" {
                unwrapped.child_by_field_name("right")
            } else {
                None
            };
            let Some(right) = right else { break };
            base = right;
        }
        if base != object {
            return format!(
                "{}.{}",
                read_text(base, source),
                read_text(property, source)
            );
        }
    }
    read_text(callee, source)
}
/// `_c3_linearization`: Solidity's C3 linearization of `type_id`, most derived
/// first. Each type's direct bases are in declaration order and the last is
/// the most derived, so they merge right to left. A cycle or an inconsistent
/// hierarchy falls back to a right-to-left depth-first order.
fn c3_linearization(type_id: &str, bases: &HashMap<String, Vec<String>>) -> Vec<String> {
    fn linearize(
        tid: &str,
        active: &mut Vec<String>,
        bases: &HashMap<String, Vec<String>>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Option<Vec<String>> {
        if let Some(done) = memo.get(tid) {
            return Some(done.clone());
        }
        if active.iter().any(|a| a == tid) {
            return None;
        }
        let parents = bases
            .get(tid)
            .map(|b| b.iter().rev().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut sequences: Vec<Vec<String>> = vec![];
        active.push(tid.to_owned());
        for parent in &parents {
            let lin = linearize(parent, active, bases, memo);
            let Some(lin) = lin else {
                active.pop();
                return None;
            };
            sequences.push(lin);
        }
        active.pop();
        sequences.push(parents);
        let mut result = vec![tid.to_owned()];
        while sequences.iter().any(|s| !s.is_empty()) {
            let head = sequences
                .iter()
                .filter_map(|seq| seq.first())
                .find(|candidate| {
                    !sequences
                        .iter()
                        .any(|other| other.iter().skip(1).any(|o| o == *candidate))
                })
                .cloned()?;
            for seq in &mut sequences {
                if seq.first() == Some(&head) {
                    seq.remove(0);
                }
            }
            result.push(head);
        }
        memo.insert(tid.to_owned(), result.clone());
        Some(result)
    }
    let mut memo = HashMap::new();
    if let Some(linear) = linearize(type_id, &mut vec![], bases, &mut memo) {
        return linear;
    }
    let mut order: Vec<String> = vec![];
    let mut stack = vec![type_id.to_owned()];
    while let Some(tid) = stack.pop() {
        if order.contains(&tid) {
            continue;
        }
        stack.extend(bases.get(&tid).into_iter().flatten().cloned());
        order.push(tid);
    }
    order
}
/// `resolve_solidity_inherited_calls` (Graphify 0.9.92, #4379): bind calls,
/// `super` calls and modifiers a contract inherits. Runs after the type pass
/// has bound each `inherits` edge to the base's real node, and walks the
/// caller contract's C3 linearization as the compiler does: the first contract
/// that declares a matching member wins (`super.f()` starts after the caller's
/// own contract, as the plain lookup does once the extractor found nothing
/// there); several same-arity overloads in that contract are ambiguous.
pub fn resolve_solidity_inherited_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    resolve_solidity_inherited_calls_any(per, nodes, edges);
}
/// [`resolve_solidity_inherited_calls`] over the per-file results and the
/// edges in either form; new edges are appended after the loop.
pub fn resolve_solidity_inherited_calls_any<P: PerFile, E: Rec>(
    per: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) {
    let calls = per
        .iter()
        .flat_map(|r| r.raw_calls().into_iter().flatten())
        .filter(|rc| {
            rc.rec_get("solidity_call")
                .is_some_and(crate::build::truthy)
        })
        .collect::<Vec<_>>();
    if calls.is_empty() {
        return;
    }
    let mut node_by_id: HashMap<String, &Value> = HashMap::new();
    for node in nodes {
        node_by_id.insert(node["id"].to_string(), node);
    }
    let empty = Value::Null;
    let meta = |id: &Value| -> &Value {
        node_by_id
            .get(&id.to_string())
            .map(|n| &n["metadata"])
            .filter(|m| m.is_object())
            .unwrap_or(&empty)
    };
    let is_type = |metadata: &Value| {
        metadata["kind"]
            .as_str()
            .is_some_and(|kind| TYPE_KINDS.contains(&kind))
    };
    let mut owner: HashMap<String, String> = HashMap::new();
    let mut functions: HashMap<String, HashMap<String, Vec<(String, Value)>>> = HashMap::new();
    let mut modifiers: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut bases: HashMap<String, Vec<String>> = HashMap::new();
    let mut existing: HashSet<(String, String, String)> = HashSet::new();
    for edge in edges.iter() {
        let (source, target, relation) =
            (at(edge, "source"), at(edge, "target"), at(edge, "relation"));
        existing.insert((source.to_string(), target.to_string(), relation.to_string()));
        let (Some(source_id), Some(target_id)) = (source.as_str(), target.as_str()) else {
            continue;
        };
        let source_meta = meta(source);
        if source_meta["language"] != "solidity" || !is_type(source_meta) {
            continue;
        }
        if relation == "method" {
            let member = meta(target);
            let label = node_by_id
                .get(&target.to_string())
                .and_then(|n| n.get("label"))
                .map(python_compat::string)
                .unwrap_or_default();
            let name = label.strip_suffix("()").unwrap_or(&label).to_owned();
            owner.insert(target_id.to_owned(), source_id.to_owned());
            if member["kind"] == "function" {
                functions
                    .entry(source_id.to_owned())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push((target_id.to_owned(), member["arity"].clone()));
            } else if member["kind"] == "modifier" {
                modifiers
                    .entry(source_id.to_owned())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push(target_id.to_owned());
            }
        } else if relation == "inherits" && is_type(meta(target)) {
            bases
                .entry(source_id.to_owned())
                .or_default()
                .push(target_id.to_owned());
        }
    }
    let find = |linear: &[String], name: &str, arity: &Value, kind: &str| -> Option<String> {
        for type_id in linear {
            let found: Vec<String> = if kind == "modifier" {
                modifiers
                    .get(type_id)
                    .and_then(|m| m.get(name))
                    .cloned()
                    .unwrap_or_default()
            } else {
                functions
                    .get(type_id)
                    .and_then(|m| m.get(name))
                    .into_iter()
                    .flatten()
                    .filter(|(_, declared)| {
                        arity.is_null() || declared.is_null() || declared == arity
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            if !found.is_empty() {
                return (found.len() == 1).then(|| found[0].clone());
            }
        }
        None
    };
    let mut linear_cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut added: Vec<E> = vec![];
    for rc in calls {
        let caller = at(rc, "caller_nid");
        let Some(contract) = caller.as_str().and_then(|c| owner.get(c)).cloned() else {
            continue;
        };
        let name = at(rc, "callee");
        if !crate::build::truthy(name) {
            continue;
        }
        let name = python_compat::string(name);
        let linear = linear_cache
            .entry(contract.clone())
            .or_insert_with(|| c3_linearization(&contract, &bases));
        let kind = python_compat::string(at(rc, "solidity_call"));
        let Some(target) = find(&linear[1..], &name, at(rc, "arity"), &kind) else {
            continue;
        };
        let relation = if kind == "modifier" { "uses" } else { "calls" };
        if !existing.insert((
            caller.to_string(),
            json!(target).to_string(),
            json!(relation).to_string(),
        )) {
            continue;
        }
        added.push(edge_record(json!({"source":caller,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":at(rc, "source_file"),"source_location":at(rc, "source_location"),"weight":1.0})));
    }
    append_edges(edges, added);
}
pub fn extract_solidity(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":format!("Solidity grammar failed to load: {}", io_compat::read_error(&error, path))});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&graphicle_tree_sitter_solidity::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":format!("Solidity grammar failed to load: {error}")});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Solidity grammar failed to load: parse cancelled"});
    };
    let root = tree.root_node();
    let path_text = fsutil::path_string(path);
    let stem = file_stem(path);
    let file_id = make_id([path_text.as_str()]);
    let parts = path_syntax::posix_parts(&path_text);
    let file_name = path_syntax::posix_name(&parts).to_string();
    let mut x = Extractor {
        source: &source,
        path: path_text,
        nodes: vec![],
        edges: vec![],
        raw_calls: vec![],
        seen: HashSet::new(),
        seen_edges: HashSet::new(),
    };
    x.add_node(&file_id, &file_name, root, "file", true, false, None);
    for declaration in named(root) {
        if declaration.kind() != "import_directive" {
            continue;
        }
        let Some(source_node) = declaration.child_by_field_name("source") else {
            continue;
        };
        let relative = unquote(&x.text(source_node));
        if relative.is_empty() || relative.starts_with("http:") || relative.starts_with("https:") {
            continue;
        }
        let target = fsutil::path_string(&path.parent().unwrap_or(Path::new("")).join(&relative));
        x.add_edge(
            &file_id,
            &make_id([target.as_str()]),
            "imports_from",
            declaration,
            None,
            Some(&target),
        );
    }
    for declaration in named(root) {
        if let Some(kind) = type_kind(declaration.kind()) {
            x.declaration(declaration, kind, &stem, &file_id);
        }
    }
    x.free_functions(root, &stem, &file_id);
    let Extractor {
        nodes,
        edges,
        raw_calls,
        seen,
        ..
    } = x;
    let edges = edges
        .into_iter()
        .filter(|e| {
            seen.contains(e["source"].as_str().unwrap_or(""))
                && (seen.contains(e["target"].as_str().unwrap_or(""))
                    || e["relation"] == "imports_from")
        })
        .collect::<Vec<_>>();
    json!({"nodes":nodes,"edges":edges,"raw_calls":raw_calls})
}

/// Point `solidity_type:<name>` inheritance and `using` edges at the one type
/// of that name declared in the same file or a file it imports.
pub fn resolve_solidity_type_references(per: &[Value], nodes: &[Value], edges: &mut [Value]) {
    resolve_solidity_type_references_any(per, nodes, edges);
}
/// [`resolve_solidity_type_references`] over the edges in either form (the
/// per-file results are not read).
pub fn resolve_solidity_type_references_any<P: PerFile, E: Rec>(
    _per: &[P],
    nodes: &[Value],
    edges: &mut [E],
) {
    let text = |v: &Value| python_compat::string(v);
    let node_by_id = nodes
        .iter()
        .map(|n| (n["id"].to_string(), n))
        .collect::<HashMap<_, _>>();
    // `{str(source_file): id}` then its inverse, with Python dict semantics.
    let mut file_id_by_source: Vec<(String, Value)> = vec![];
    let mut position: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        if !crate::build::truthy(&n["source_file"]) {
            continue;
        }
        let source = text(&n["source_file"]);
        let parts = path_syntax::posix_parts(&source);
        if n.get("label") != Some(&json!(path_syntax::posix_name(&parts))) {
            continue;
        }
        match position.get(&source) {
            Some(i) => file_id_by_source[*i].1 = n["id"].clone(),
            None => {
                position.insert(source.clone(), file_id_by_source.len());
                file_id_by_source.push((source, n["id"].clone()));
            }
        }
    }
    let mut source_by_file_id: HashMap<String, String> = HashMap::new();
    for (source, id) in &file_id_by_source {
        source_by_file_id.insert(id.to_string(), source.clone());
    }
    let mut imported: HashMap<String, BTreeSet<String>> = HashMap::new();
    for edge in edges
        .iter()
        .filter(|e| at(*e, "relation") == "imports_from")
    {
        if let (Some(from), Some(to)) = (
            source_by_file_id.get(&at(edge, "source").to_string()),
            source_by_file_id.get(&at(edge, "target").to_string()),
        ) {
            imported.entry(from.clone()).or_default().insert(to.clone());
        }
    }
    let mut types: HashMap<(String, String), Vec<Value>> = HashMap::new();
    for n in nodes {
        if !n["metadata"].is_object()
            || !TYPE_KINDS.contains(&n["metadata"]["kind"].as_str().unwrap_or("\u{0}"))
            || !crate::build::truthy(&n["source_file"])
        {
            continue;
        }
        let label = n.get("label").map(text).unwrap_or_default();
        types
            .entry((text(&n["source_file"]), label))
            .or_default()
            .push(n["id"].clone());
    }
    for edge in edges.iter_mut() {
        let Some(name) = at(edge, "context")
            .as_str()
            .and_then(|c| c.strip_prefix("solidity_type:"))
            .map(str::to_string)
        else {
            continue;
        };
        let Some(source_node) = node_by_id
            .get(&at(edge, "source").to_string())
            .filter(|n| crate::build::truthy(&n["source_file"]))
        else {
            continue;
        };
        let source_file = text(&source_node["source_file"]);
        let mut candidates = types
            .get(&(source_file.clone(), name.clone()))
            .cloned()
            .unwrap_or_default();
        for other in imported.get(&source_file).into_iter().flatten() {
            candidates.extend(
                types
                    .get(&(other.clone(), name.clone()))
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        if let [target] = candidates.as_slice() {
            edge.rec_set("target", target.clone());
        }
    }
}
