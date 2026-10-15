// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/elixir.py
//! Complete translation of Graphify's Elixir tree-sitter extractor.
use super::base::{file_stem, is_language_builtin, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax, regex_compat};
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
const IMPORTS: &[&str] = &["alias", "import", "require", "use"];
const SKIP: &[&str] = &[
    "def",
    "defp",
    "defmodule",
    "defmacro",
    "defmacrop",
    "defstruct",
    "defprotocol",
    "defimpl",
    "defguard",
    "defdelegate",
    "alias",
    "import",
    "require",
    "use",
    "if",
    "unless",
    "case",
    "cond",
    "with",
    "for",
];
struct Extractor<'tree, 'source> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
    labels: HashMap<String, String>,
    pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
    /// `import`/`use` targets per enclosing module (`None` is the file's top
    /// level), and each module's and function's parent (Graphify c96a743,
    /// 48de615): Elixir scopes them to the module body and its nested modules.
    call_scope: HashMap<Option<String>, Vec<String>>,
    enclosing: HashMap<String, Option<String>>,
    /// For qualified calls (`Mod.fun()`, #4206): each module's full name in
    /// first-registration order, each scope's `alias` table (scoped like
    /// `call_scope`) and each module's own defs.
    module_names: Vec<(String, String)>,
    aliases: HashMap<Option<String>, HashMap<String, String>>,
    module_defs: HashMap<(String, String), String>,
}
impl<'tree> Extractor<'tree, '_> {
    /// import/use targets visible to a call inside `caller`: its own module's,
    /// each enclosing module's, then the file's top-level ones.
    fn call_scope_for(&self, caller: &str) -> Vec<String> {
        let mut scope = Vec::new();
        // A module nested in a same-named one shares its id.
        let mut seen = HashSet::new();
        let mut container = self.enclosing.get(caller).cloned().flatten();
        while let Some(current) = container {
            if !seen.insert(current.clone()) {
                break;
            }
            if let Some(targets) = self.call_scope.get(&Some(current.clone())) {
                scope.extend(targets.iter().cloned());
            }
            container = self.enclosing.get(&current).cloned().flatten();
        }
        if let Some(targets) = self.call_scope.get(&None) {
            scope.extend(targets.iter().cloned());
        }
        scope
    }
    fn node(&mut self, id: &str, label: &str, line: usize, module: bool) {
        if self.seen.insert(id.into()) {
            let mut node = json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")});
            if module {
                node["_elixir_module"] = true.into();
            }
            self.nodes.push(node);
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
    fn alias_text(&self, node: Node<'tree>) -> Option<String> {
        children(node)
            .find(|n| n.kind() == "alias")
            .map(|n| read_text(n, self.source))
    }
    /// The module given for keyword `key` in an argument list: the `for:` of
    /// `defimpl Proto, for: Type` or the `as:` of `alias A.B, as: C`, held in a
    /// trailing `keywords` node as a `pair` of keyword `key:` and an `alias`.
    fn keyword_alias(&self, node: Node<'tree>, key: &str) -> Option<String> {
        for keywords in children(node).filter(|n| n.kind() == "keywords") {
            for pair in children(keywords).filter(|n| n.kind() == "pair") {
                let (mut keyword, mut value) = (None, None);
                for sub in children(pair) {
                    match sub.kind() {
                        "keyword" => keyword = Some(read_text(sub, self.source)),
                        "alias" => value = Some(read_text(sub, self.source)),
                        _ => (),
                    }
                }
                let is_key = keyword.as_deref().is_some_and(|k| {
                    !k.is_empty()
                        && k.trim_end_matches([':', ' '])
                            .trim_matches(crate::dedup_strings::whitespace)
                            == key
                });
                if is_key && let Some(value) = value.filter(|v| !v.is_empty()) {
                    return Some(value);
                }
            }
        }
        None
    }
    /// The body of a keyword-form definition (`def f(x), do: expr`): the value
    /// after the `do:` keyword of a `pair` in the arguments' `keywords` (#4207).
    fn do_keyword_body(&self, node: Option<Node<'tree>>) -> Option<Node<'tree>> {
        for keywords in children(node?).filter(|n| n.kind() == "keywords") {
            for pair in children(keywords).filter(|n| n.kind() == "pair") {
                let mut keyword: Option<String> = None;
                for sub in children(pair) {
                    if sub.kind() == "keyword" {
                        keyword = Some(read_text(sub, self.source));
                    } else if keyword.as_deref().is_some_and(|k| {
                        k.trim_end_matches([':', ' '])
                            .trim_matches(crate::dedup_strings::whitespace)
                            == "do"
                    }) {
                        return Some(sub);
                    }
                }
            }
        }
        None
    }
    fn module_name(&self, id: &str) -> Option<&str> {
        self.module_names
            .iter()
            .find(|(nid, _)| nid == id)
            .map(|(_, name)| name.as_str())
    }
    fn set_module_name(&mut self, id: &str, name: String) {
        match self.module_names.iter_mut().find(|(nid, _)| nid == id) {
            Some(entry) => entry.1 = name,
            None => self.module_names.push((id.to_string(), name)),
        }
    }
    /// Expand the first segment of a module reference through the `alias`
    /// tables visible from `scope`, or `__MODULE__` to the current module.
    fn expand_module(&self, name: &str, scope: Option<&str>) -> String {
        let (first, rest) = name.split_once('.').unwrap_or((name, ""));
        let target = if first == "__MODULE__" {
            // `scope` is a module, or a function directly inside one.
            let scope = match scope {
                Some(s) if self.module_name(s).is_none() => {
                    self.enclosing.get(s).cloned().flatten()
                }
                other => other.map(str::to_string),
            };
            scope
                .and_then(|s| self.module_name(&s).map(str::to_string))
                .filter(|n| !n.is_empty())
        } else {
            let mut seen = HashSet::new();
            let mut container = scope.map(str::to_string);
            let mut found = None;
            while seen.insert(container.clone()) {
                found = self
                    .aliases
                    .get(&container)
                    .and_then(|table| table.get(first))
                    .cloned();
                if found.is_some() || container.is_none() {
                    break;
                }
                container = container
                    .as_ref()
                    .and_then(|c| self.enclosing.get(c).cloned().flatten());
            }
            found
        };
        match target {
            None => name.to_string(),
            Some(target) if rest.is_empty() => target,
            Some(target) => format!("{target}.{rest}"),
        }
    }
    /// `alias A.B` (and `alias A.{B, C}`) binds `B` to `A.B` in `scope`;
    /// `alias A.B, as: C` binds `C`.
    fn record_aliases(&mut self, arguments: Node<'tree>, scope: Option<&str>) {
        let modules = self
            .alias_modules(arguments)
            .into_iter()
            .map(|m| self.expand_module(&m, scope))
            .collect::<Vec<_>>();
        let as_name = self.keyword_alias(arguments, "as");
        let table = self.aliases.entry(scope.map(str::to_string)).or_default();
        match as_name {
            Some(as_name) if modules.len() == 1 => {
                table.insert(as_name, modules[0].clone());
            }
            Some(_) => {}
            None => {
                for module in modules {
                    let short = module.rsplit('.').next().unwrap_or("").to_string();
                    table.insert(short, module);
                }
            }
        }
    }
    /// A nested `defmodule Inner` is `Outer.Inner`, and `Inner` is
    /// auto-aliased inside `Outer`.
    fn register_module(&mut self, id: &str, name: &str, parent: Option<&str>) {
        let parent_name = parent
            .and_then(|p| self.module_name(p))
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let mut name = name.to_string();
        if let (Some(parent), Some(parent_name)) = (parent, parent_name) {
            let first = name.split('.').next().unwrap_or("").to_string();
            self.aliases
                .entry(Some(parent.to_string()))
                .or_default()
                .insert(first.clone(), format!("{parent_name}.{first}"));
            name = format!("{parent_name}.{name}");
        }
        self.set_module_name(id, name);
    }
    fn alias_modules(&self, node: Node<'tree>) -> Vec<String> {
        for child in children(node) {
            if child.kind() == "alias" {
                return vec![read_text(child, self.source)];
            }
            if child.kind() == "dot" {
                let mut base = None;
                let mut tuple = None;
                for sub in children(child) {
                    if sub.kind() == "alias" && base.is_none() {
                        base = Some(read_text(sub, self.source));
                    } else if sub.kind() == "tuple" {
                        tuple = Some(sub);
                    }
                }
                if let (Some(base), Some(tuple)) = (base.filter(|s| !s.is_empty()), tuple) {
                    let members = children(tuple)
                        .filter(|n| n.kind() == "alias")
                        .map(|n| format!("{base}.{}", read_text(n, self.source)))
                        .collect::<Vec<_>>();
                    if !members.is_empty() {
                        return members;
                    }
                }
                return vec![read_text(child, self.source)];
            }
        }
        vec![]
    }
    fn walk(&mut self, node: Node<'tree>, parent: Option<&str>) {
        if node.kind() != "call" {
            for child in children(node) {
                self.walk(child, parent);
            }
            return;
        }
        let mut identifier = None;
        let mut arguments = None;
        let mut block = None;
        for child in children(node) {
            match child.kind() {
                "identifier" => identifier = Some(child),
                "arguments" => arguments = Some(child),
                "do_block" => block = Some(child),
                _ => (),
            }
        }
        let Some(identifier) = identifier else {
            for child in children(node) {
                self.walk(child, parent);
            }
            return;
        };
        let keyword = read_text(identifier, self.source);
        let line = node.start_position().row + 1;
        if keyword == "defmodule" {
            let Some(name) = arguments
                .and_then(|n| self.alias_text(n))
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let id = make_id([self.stem.as_str(), name.as_str()]);
            self.node(&id, &name, line, parent.is_none());
            self.edge(&self.file.clone(), &id, "contains", line, None);
            self.enclosing
                .insert(id.clone(), parent.map(str::to_string));
            self.register_module(&id, &name, parent);
            if let Some(block) = block {
                for child in children(block) {
                    self.walk(child, Some(&id));
                }
            }
            return;
        }
        if keyword == "defprotocol" || keyword == "defimpl" {
            // #3839: protocols and implementations are named containers; their
            // functions no longer leak onto the file.
            let Some(protocol) = arguments
                .and_then(|n| self.alias_text(n))
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let container = parent
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.file)
                .to_string();
            let id = if keyword == "defprotocol" {
                let id = make_id([self.stem.as_str(), protocol.as_str()]);
                self.node(&id, &protocol, line, parent.is_none());
                self.edge(&container, &id, "contains", line, None);
                self.enclosing
                    .insert(id.clone(), parent.map(str::to_string));
                self.register_module(&id, &protocol, parent);
                id
            } else {
                let target = arguments.and_then(|n| self.keyword_alias(n, "for"));
                let target_text = target.as_deref().unwrap_or("");
                let id = make_id([
                    self.stem.as_str(),
                    "defimpl",
                    protocol.as_str(),
                    target_text,
                ]);
                let label = match &target {
                    Some(target) => format!("{protocol} (for {target})"),
                    None => protocol.clone(),
                };
                self.node(&id, &label, line, false);
                self.edge(&container, &id, "contains", line, None);
                self.enclosing
                    .insert(id.clone(), parent.map(str::to_string));
                let name = target
                    .as_ref()
                    .map_or(String::new(), |target| format!("{protocol}.{target}"));
                self.set_module_name(&id, name);
                // A cross-file protocol is dropped by the dangling-edge guard.
                let protocol_id = make_id([self.stem.as_str(), protocol.as_str()]);
                self.edge(&id, &protocol_id, "implements", line, None);
                id
            };
            if let Some(block) = block {
                for child in children(block) {
                    self.walk(child, Some(&id));
                }
            }
            return;
        }
        // `defmacro`/`defmacrop` and `defguard`/`defguardp` define invocable
        // members with the same head shape as `def`/`defp` (0.9.75), and so
        // does `defdelegate name(args), to: Mod`, whose body is delegated
        // (#4132).
        if matches!(
            keyword.as_str(),
            "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defdelegate"
        ) {
            let mut name = None;
            if let Some(arguments) = arguments {
                for mut child in children(arguments) {
                    while child.kind() == "binary_operator" {
                        let Some(head) = children(child).find(|n| {
                            matches!(n.kind(), "call" | "identifier" | "binary_operator")
                        }) else {
                            break;
                        };
                        child = head;
                    }
                    if child.kind() == "call" {
                        if let Some(sub) = children(child).find(|n| n.kind() == "identifier") {
                            name = Some(read_text(sub, self.source));
                        }
                    } else if child.kind() == "identifier" {
                        name = Some(read_text(child, self.source));
                        break;
                    }
                }
            }
            let Some(name) = name.filter(|s| !s.is_empty()) else {
                return;
            };
            let container = parent
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.file)
                .to_string();
            let id = make_id([container.as_str(), name.as_str()]);
            self.node(&id, &format!("{name}()"), line, false);
            self.enclosing
                .insert(id.clone(), parent.map(str::to_string));
            if let Some(parent) = parent.filter(|s| !s.is_empty()) {
                self.edge(parent, &id, "method", line, None);
                self.module_defs
                    .entry((parent.to_string(), name.clone()))
                    .or_insert_with(|| id.clone());
            } else {
                self.edge(&self.file.clone(), &id, "contains", line, None);
            }
            if let Some(block) = block {
                self.bodies.push((id, block));
            } else if let Some(body) = self.do_keyword_body(arguments) {
                // `def f(x), do: expr` has no do_block: its body is the `do:`
                // pair's value, walked so its calls are not lost (#4207).
                self.bodies.push((id, body));
            }
            return;
        }
        if IMPORTS.contains(&keyword.as_str())
            && let Some(arguments) = arguments
        {
            for module in self.alias_modules(arguments) {
                let id = make_id([module.as_str()]);
                self.edge(&self.file.clone(), &id, "imports", line, Some("import"));
                // Only import/use bring functions into scope for unqualified
                // calls; alias/require do not.
                if keyword == "import" || keyword == "use" {
                    self.call_scope
                        .entry(parent.map(str::to_string))
                        .or_default()
                        .push(module);
                }
            }
            if keyword == "alias" {
                self.record_aliases(arguments, parent);
            }
            return;
        }
        for child in children(node) {
            self.walk(child, parent);
        }
    }
    fn walk_calls(&mut self, node: Node<'tree>, caller: &str) {
        if node.kind() != "call" {
            for child in children(node) {
                self.walk_calls(child, caller);
            }
            return;
        }
        if let Some(child) = children(node).find(|n| n.kind() == "identifier") {
            let keyword = read_text(child, self.source);
            if SKIP.contains(&keyword.as_str()) {
                if keyword == "alias" {
                    // A function-local alias.
                    for arguments in children(node).filter(|n| n.kind() == "arguments") {
                        self.record_aliases(arguments, Some(caller));
                    }
                }
                for child in children(node) {
                    self.walk_calls(child, caller);
                }
                return;
            }
        }
        static MODULE_REF: LazyLock<regex::Regex> =
            LazyLock::new(|| regex_compat::compile(r"^(?:__MODULE__|[A-Z]\w*)(?:\.[A-Z]\w*)*$"));
        let mut callee = None;
        let mut member = false;
        let mut receiver_module = None;
        for child in children(node) {
            if child.kind() == "dot" {
                member = true;
                callee = Some(
                    read_text(child, self.source)
                        .trim_end_matches('.')
                        .rsplit('.')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
                // `Mod.fun()`, `Alias.fun()` and `__MODULE__.fun()` keep their
                // module; a variable or atom receiver does not (#4206).
                if child.child_count() == 3
                    && let Some(receiver) = child.child(0)
                {
                    let text = read_text(receiver, self.source);
                    if MODULE_REF.is_match(&text) {
                        receiver_module = Some(self.expand_module(&text, Some(caller)));
                    }
                }
                break;
            }
            if child.kind() == "identifier" {
                callee = Some(read_text(child, self.source));
                break;
            }
        }
        let receiver_module = receiver_module.filter(|m| !m.is_empty());
        if let Some(callee) = callee
            .filter(|s| !s.is_empty() && (receiver_module.is_some() || !is_language_builtin(s)))
        {
            // A remote call only reaches the named module's own def, never the
            // bare name; any other member call stays unbound here.
            let target = if let Some(module) = &receiver_module {
                self.module_names
                    .iter()
                    .rev()
                    .find(|(_, name)| name == module)
                    .and_then(|(id, _)| self.module_defs.get(&(id.clone(), callee.clone())))
                    .cloned()
            } else if member {
                None
            } else {
                self.labels.get(&callee).cloned()
            };
            if let Some(target) = target.filter(|s| !s.is_empty() && s.as_str() != caller) {
                if self.pairs.insert((caller.into(), target.clone())) {
                    self.edge(
                        caller,
                        &target,
                        "calls",
                        node.start_position().row + 1,
                        Some("call"),
                    );
                }
            } else {
                let scope = self.call_scope_for(caller);
                let mut call = json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1),"elixir_call_scope":scope});
                if let Some(module) = receiver_module {
                    call["elixir_module"] = json!(module);
                }
                self.raw_calls.push(call);
            }
        }
        for child in children(node) {
            self.walk_calls(child, caller);
        }
    }
}
pub fn extract_elixir(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&match crate::grammar!(
        "grammar-elixir",
        "tree_sitter_elixir",
        tree_sitter_elixir::LANGUAGE
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":error.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Elixir parse cancelled"});
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
        labels: HashMap::new(),
        pairs: HashSet::new(),
        raw_calls: vec![],
        call_scope: HashMap::new(),
        enclosing: HashMap::new(),
        module_names: vec![],
        aliases: HashMap::new(),
        module_defs: HashMap::new(),
    };
    ex.node(&file, name, 1, false);
    ex.walk(tree.root_node(), None);
    for node in &ex.nodes {
        ex.labels.insert(
            node["label"]
                .as_str()
                .unwrap()
                .trim_matches(['(', ')'])
                .trim_start_matches('.')
                .into(),
            node["id"].as_str().unwrap().into(),
        );
    }
    for (caller, body) in ex.bodies.clone() {
        ex.walk_calls(body, &caller);
    }
    ex.edges.retain(|e| {
        ex.seen.contains(e["source"].as_str().unwrap_or(""))
            && (ex.seen.contains(e["target"].as_str().unwrap_or("")) || e["relation"] == "imports")
    });
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"input_tokens":0,"output_tokens":0})
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-elixir")]
    fn grammar_matches_pinned_reference() {
        let l: tree_sitter::Language = tree_sitter_elixir::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 235);
        assert_eq!(l.parse_state_count(), 7001);
        assert_eq!(l.abi_version(), 14);
    }
}
