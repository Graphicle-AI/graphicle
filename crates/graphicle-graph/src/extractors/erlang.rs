// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/erlang.py
//! Port of graphify/extractors/erlang.py (Graphify 0.9.69, #3714): modules,
//! exports, behaviours, includes, records, types, macros and functions keyed
//! by name/arity. Local calls bind by arity; `mod:fun(...)` calls resolve
//! across files through [`resolve_erlang_remote_calls`].
use super::base::{file_stem, make_id, read_text};
use crate::{
    Result,
    extract::per_file::PerFile,
    fsutil, io_compat, path_syntax, python_compat,
    resolver_registry::{Rec, append_edges, at, edge_record},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
/// Named descendants in pre-order (`_descendants`), excluding `node`.
fn descendants(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = vec![];
    let mut stack = named(node);
    stack.reverse();
    while let Some(n) = stack.pop() {
        out.push(n);
        let mut kids = named(n);
        kids.reverse();
        stack.extend(kids);
    }
    out
}
/// `_atom`: an atom's text without its single quotes.
fn atom(value: &str) -> String {
    let value = value.trim();
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() >= 2 && chars[0] == '\'' && chars[chars.len() - 1] == '\'' {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        value.into()
    }
}
/// Python's `int(text)` for a base-10 literal. `Ok(None)` is a valid literal
/// beyond `i64` (it cannot equal any arity); `Err` is the `ValueError` text.
fn python_int(text: &str) -> std::result::Result<Option<i64>, String> {
    let body = text.trim();
    let digits = body.strip_prefix(['+', '-']).unwrap_or(body);
    let valid = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(format!(
            "ValueError: invalid literal for int() with base 10: {}",
            python_compat::repr_string(text)
        ));
    }
    Ok(body.replace('_', "").parse::<i64>().ok())
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
        metadata: &[(&str, Value)],
    ) -> String {
        if self.seen.insert(id.into()) {
            let mut details = Map::new();
            details.insert("language".into(), json!("erlang"));
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
}
pub fn extract_erlang(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(source) => source,
        Err(error) => {
            return Ok(
                json!({"nodes":[],"edges":[],"error":format!("Erlang grammar failed to load: {}", io_compat::read_error(&error, path))}),
            );
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&graphicle_tree_sitter_erlang::LANGUAGE.into()) {
        return Ok(
            json!({"nodes":[],"edges":[],"error":format!("Erlang grammar failed to load: {error}")}),
        );
    }
    let Some(tree) = parser.parse(&source, None) else {
        return Ok(
            json!({"nodes":[],"edges":[],"error":"Erlang grammar failed to load: parse cancelled"}),
        );
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
    x.add_node(&file_id, &file_name, root, "file", true, false, &[]);

    let mut module_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut module_id = file_id.clone();
    if let Some(declaration) = named(root)
        .into_iter()
        .find(|n| n.kind() == "module_attribute")
        && let Some(name) = declaration.child_by_field_name("name")
    {
        module_name = atom(&x.text(name));
        module_id = x.add_node(
            &make_id([stem.as_str(), "module", module_name.as_str()]),
            &module_name.clone(),
            declaration,
            "module",
            true,
            false,
            &[("module", json!(module_name))],
        );
        x.add_edge(&file_id, &module_id, "contains", declaration);
    }

    let mut functions: HashMap<(String, i64), String> = HashMap::new();
    let mut bodies = vec![];
    let mut export_specs = vec![];
    for node in named(root) {
        match node.kind() {
            "export_attribute" => {
                for fa in descendants(node).into_iter().filter(|n| n.kind() == "fa") {
                    let inner = descendants(fa);
                    let name = inner.iter().find(|n| n.kind() == "atom");
                    let integer = inner.iter().find(|n| n.kind() == "integer");
                    if let (Some(name), Some(integer)) = (name, integer) {
                        let arity = python_int(&x.text(*integer)).map_err(crate::Error::from)?;
                        export_specs.push((atom(&x.text(*name)), arity, fa));
                    }
                }
                continue;
            }
            "behaviour_attribute" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = atom(&x.text(name_node));
                    let target = x.add_node(
                        &make_id(["erlang", "behaviour", name.as_str()]),
                        &name,
                        name_node,
                        "behaviour",
                        false,
                        false,
                        &[],
                    );
                    x.add_edge(&module_id, &target, "implements", node);
                }
                continue;
            }
            "pp_include" | "pp_include_lib" => {
                let Some(string) = named(node).into_iter().find(|n| n.kind() == "string") else {
                    continue;
                };
                let include = x.text(string).trim_matches('"').to_string();
                if node.kind() == "pp_include" {
                    let target = path.parent().unwrap_or(Path::new("")).join(&include);
                    let target_text = fsutil::path_string(&target);
                    let target_id = make_id([target_text.as_str()]);
                    if x.seen_edges.insert((
                        module_id.clone(),
                        target_id.clone(),
                        "imports_from".into(),
                    )) {
                        x.edges.push(json!({"source":module_id,"target":target_id,"relation":"imports_from","confidence":"EXTRACTED","source_file":x.path,"source_location":format!("L{}",node.start_position().row+1),"weight":1.0,"target_file":target_text}));
                    }
                } else {
                    let target = x.add_node(
                        &make_id(["erlang", "include_lib", include.as_str()]),
                        &include,
                        string,
                        "include_lib",
                        false,
                        false,
                        &[],
                    );
                    x.add_edge(&module_id, &target, "imports", node);
                }
                continue;
            }
            _ => {}
        }
        let declaration = match node.kind() {
            "record_decl" => Some(("record", "name")),
            "type_alias" | "opaque" => Some(("type", "name")),
            "pp_define" => Some(("macro", "lhs")),
            _ => None,
        };
        if let Some((kind, field)) = declaration {
            let Some(container) = node.child_by_field_name(field) else {
                continue;
            };
            let (name, label) = match kind {
                "type" => {
                    let name =
                        atom(&x.text(container.child_by_field_name("name").unwrap_or(container)));
                    let arity = container
                        .child_by_field_name("args")
                        .map_or(0, |a| a.named_child_count());
                    let label = format!("{name}/{arity}");
                    (name, label)
                }
                "macro" => {
                    let name = x.text(container.child_by_field_name("name").unwrap_or(container));
                    (name.clone(), name)
                }
                _ => {
                    let name = atom(&x.text(container));
                    (name.clone(), name)
                }
            };
            let id = x.add_node(
                &make_id([module_id.as_str(), kind, name.as_str()]),
                &label,
                node,
                kind,
                true,
                false,
                &[("module", json!(module_name)), ("name", json!(name))],
            );
            x.add_edge(&module_id, &id, "contains", node);
            continue;
        }
        if node.kind() != "fun_decl" {
            continue;
        }
        for clause in named(node)
            .into_iter()
            .filter(|n| n.kind() == "function_clause")
        {
            let (Some(name_node), Some(args)) = (
                clause.child_by_field_name("name"),
                clause.child_by_field_name("args"),
            ) else {
                continue;
            };
            let name = atom(&x.text(name_node));
            let arity = args.named_child_count() as i64;
            let key = (name.clone(), arity);
            let function_id = match functions.get(&key) {
                Some(id) => id.clone(),
                None => {
                    let id = x.add_node(
                        &make_id([
                            module_id.as_str(),
                            "function",
                            name.as_str(),
                            arity.to_string().as_str(),
                        ]),
                        &format!("{name}/{arity}"),
                        clause,
                        "function",
                        true,
                        true,
                        &[
                            ("module", json!(module_name)),
                            ("name", json!(name)),
                            ("arity", json!(arity)),
                        ],
                    );
                    functions.insert(key, id.clone());
                    x.add_edge(&module_id, &id, "contains", clause);
                    id
                }
            };
            if let Some(body) = clause.child_by_field_name("body") {
                bodies.push((body, function_id));
            }
        }
    }
    for (name, arity, spec) in export_specs {
        if let Some(target) = arity.and_then(|a| functions.get(&(name, a))).cloned() {
            x.add_edge(&module_id, &target, "exports", spec);
        }
    }
    for (body, caller) in bodies {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.kind() == "call"
                && let (Some(callee), Some(args)) = (
                    node.child_by_field_name("expr"),
                    node.child_by_field_name("args"),
                )
            {
                let arity = args.named_child_count() as i64;
                if callee.kind() == "atom" {
                    let name = atom(&x.text(callee));
                    if let Some(target) = functions.get(&(name, arity)).cloned() {
                        x.add_edge(&caller, &target, "calls", node);
                    }
                } else if callee.kind() == "remote" {
                    let written = x.text(callee);
                    if let Some((module, name)) = written.split_once(':') {
                        x.raw_calls.push(json!({"caller_nid":caller,"callee":atom(name),"arity":arity,"remote_module":atom(module),"is_member_call":true,"language":"erlang","source_file":x.path,"source_location":format!("L{}",node.start_position().row+1)}));
                    }
                }
            } else if node.kind() == "internal_fun"
                && let (Some(name_node), Some(arity_node)) = (
                    node.child_by_field_name("fun"),
                    node.child_by_field_name("arity"),
                )
            {
                // `fun helper/1` hands a local function off to be invoked
                // (0.9.74 #3993); remote `fun mod:f/2` stays unresolved.
                let digits = x
                    .text(arity_node)
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>();
                if let Ok(arity) = digits.parse::<i64>()
                    && let Some(target) = functions.get(&(atom(&x.text(name_node)), arity)).cloned()
                {
                    x.add_edge(&caller, &target, "indirect_call", node);
                }
            }
            let mut kids = named(node);
            kids.reverse();
            stack.extend(kids);
        }
    }
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
    Ok(json!({"nodes":nodes,"edges":edges,"raw_calls":raw_calls}))
}

/// Bind `mod:fun(...)` raw calls to the one function with that exact module,
/// name and arity anywhere in the corpus.
pub fn resolve_erlang_remote_calls(per: &[Value], nodes: &[Value], edges: &mut Vec<Value>) {
    resolve_erlang_remote_calls_any(per, nodes, edges);
}
/// [`resolve_erlang_remote_calls`] over the per-file results and the edges
/// in either form. New edges are appended after the loop, which reads none
/// of them.
pub fn resolve_erlang_remote_calls_any<P: PerFile, E: Rec>(
    per: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) {
    let mut functions: HashMap<(String, String, i64), Vec<String>> = HashMap::new();
    for node in nodes {
        let metadata = &node["metadata"];
        if metadata["language"] != "erlang" || metadata["kind"] != "function" {
            continue;
        }
        if let (Some(module), Some(name), Some(arity)) = (
            metadata["module"].as_str(),
            metadata["name"].as_str(),
            metadata["arity"].as_i64(),
        ) {
            functions
                .entry((module.into(), name.into(), arity))
                .or_default()
                .push(python_compat::string(&node["id"]));
        }
    }
    let mut existing = edges
        .iter()
        .filter(|e| at(*e, "relation") == "calls")
        .map(|e| (at(e, "source").clone(), at(e, "target").clone()))
        .collect::<HashSet<_>>();
    let mut added: Vec<E> = vec![];
    for call in per.iter().flat_map(|r| r.raw_calls().into_iter().flatten()) {
        if at(call, "language") != "erlang" || !crate::build::truthy(at(call, "remote_module")) {
            continue;
        }
        let key = (
            python_compat::string(at(call, "remote_module")),
            call.rec_get("callee")
                .map(python_compat::string)
                .unwrap_or_default(),
            call.rec_get("arity").and_then(Value::as_i64).unwrap_or(-1),
        );
        let caller = at(call, "caller_nid").clone();
        let Some([target]) = functions.get(&key).map(Vec::as_slice) else {
            continue;
        };
        if caller.as_str() == Some(target.as_str()) {
            continue;
        }
        let pair = (caller.clone(), json!(target));
        if !existing.insert(pair) {
            continue;
        }
        added.push(edge_record(json!({"source":caller,"target":target,"relation":"calls","context":"remote_call","confidence":"EXTRACTED","confidence_score":1.0,"source_file":call.rec_get("source_file").cloned().unwrap_or(json!("")),"source_location":at(call, "source_location"),"weight":1.0})));
    }
    append_edges(edges, added);
}
