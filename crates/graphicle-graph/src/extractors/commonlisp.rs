// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/commonlisp.py
//! Complete Common Lisp form, package, type and call extraction from the pinned source.
use super::base::{file_stem, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;

const DATA: &[&str] = &[
    "defstruct",
    "deftype",
    "define-condition",
    "defvar",
    "defparameter",
    "defconstant",
    "define-symbol-macro",
];
const VALUES: &[&str] = &["defvar", "defparameter", "defconstant"];
const MACROS: &[&str] = &[
    "define-modify-macro",
    "define-compiler-macro",
    "define-setf-expander",
    "defsetf",
];
const NOT_DEFINERS: &[&str] = &[
    "default",
    "default-value",
    "defaults",
    "define",
    "defer",
    "deferred",
    "deflate",
    "deftest",
];
const SPECIAL: &[&str] = &[
    "let",
    "let*",
    "flet",
    "labels",
    "macrolet",
    "if",
    "when",
    "unless",
    "cond",
    "case",
    "ecase",
    "typecase",
    "etypecase",
    "progn",
    "prog1",
    "prog2",
    "block",
    "return-from",
    "tagbody",
    "go",
    "lambda",
    "function",
    "funcall",
    "apply",
    "setf",
    "setq",
    "psetf",
    "psetq",
    "incf",
    "decf",
    "push",
    "pop",
    "loop",
    "do",
    "do*",
    "dolist",
    "dotimes",
    "map",
    "mapcar",
    "mapc",
    "format",
    "print",
    "princ",
    "prin1",
    "write",
    "terpri",
    "and",
    "or",
    "not",
    "null",
    "values",
    "multiple-value-bind",
    "multiple-value-setq",
    "declare",
    "the",
    "locally",
    "eval-when",
    "quote",
    "backquote",
    "error",
    "signal",
    "warn",
    "assert",
    "check-type",
    "handler-case",
    "handler-bind",
    "restart-case",
    "ignore-errors",
    "unwind-protect",
    "catch",
    "throw",
    "with-open-file",
    "with-output-to-string",
    "with-input-from-string",
    "with-slots",
    "with-accessors",
    "defvar",
    "defparameter",
    "defconstant",
    "in-package",
    "require",
    "provide",
    "use-package",
    "t",
    "nil",
];
fn cl_id<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let parts: Vec<String> = parts
        .into_iter()
        .map(|s| {
            s.chars()
                .map(|c| match c {
                    '=' => "_eq".into(),
                    '<' => "_lt".into(),
                    '>' => "_gt".into(),
                    '?' => "_p".into(),
                    '!' => "_bang".into(),
                    '+' => "_plus".into(),
                    '*' => "_star".into(),
                    '/' => "_slash".into(),
                    '%' => "_pct".into(),
                    '&' => "_amp".into(),
                    _ => c.to_string(),
                })
                .collect()
        })
        .collect();
    make_id(parts.iter().map(String::as_str))
}
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
    bodies: Vec<(String, Vec<Node<'tree>>)>,
    package: Option<String>,
    labels: HashMap<String, String>,
    pairs: HashSet<(String, String)>,
}
impl<'tree> Extractor<'tree, '_> {
    fn node(&mut self, id: &str, label: &str, line: usize, typ: &str) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":typ,"source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        self.edges.push(json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}));
    }
    fn import_stub(&mut self, name: &str) -> String {
        let id = cl_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn class_ref(&mut self, name: &str) -> String {
        let local = cl_id([self.stem.as_str(), name]);
        if self.seen.contains(&local) {
            local
        } else {
            self.import_stub(name)
        }
    }
    fn first_sym(&self, node: Node<'_>) -> Option<String> {
        children(node)
            .find(|n| n.kind() == "sym_lit")
            .map(|n| read_text(n, self.source))
    }
    fn keyword(&self, node: Node<'_>) -> String {
        read_text(node, self.source)
            .trim_start_matches(':')
            .trim_start_matches(['#', ':'])
            .into()
    }
    fn owner(&self) -> String {
        if let Some(package) = self.package.as_deref().filter(|p| !p.is_empty()) {
            let id = cl_id([self.stem.as_str(), package]);
            if self.seen.contains(&id) {
                return id;
            }
        }
        self.file_id.clone()
    }
    fn defpackage(&mut self, node: Node<'tree>) {
        let mut package = None;
        for n in children(node) {
            if n.kind() == "kwd_lit" {
                package = Some(self.keyword(n));
                break;
            }
            if n.kind() == "sym_lit" && read_text(n, self.source) != "defpackage" {
                package = Some(read_text(n, self.source));
                break;
            }
        }
        let Some(package) = package.filter(|p| !p.is_empty()) else {
            return;
        };
        self.package = Some(package.clone());
        let id = cl_id([self.stem.as_str(), package.as_str()]);
        let line = node.start_position().row + 1;
        self.node(&id, &package, line, "code");
        self.edge(&self.file_id.clone(), &id, "contains", line);
        for n in children(node).filter(|n| n.kind() == "list_lit") {
            if self.first_sym(n).is_none() {
                for key in children(n) {
                    if key.kind() == "kwd_lit" && self.keyword(key) == "use" {
                        for name in
                            children(n).filter(|c| c.kind() == "kwd_lit" && c.id() != key.id())
                        {
                            let name = self.keyword(name);
                            if name != "use" {
                                let target = self.import_stub(&name);
                                self.edge(&id, &target, "imports", n.start_position().row + 1);
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    fn defclass(&mut self, node: Node<'tree>) {
        let Some(name) = children(node).filter(|n| n.kind() == "sym_lit").nth(1) else {
            return;
        };
        let name = read_text(name, self.source);
        let id = cl_id([self.stem.as_str(), name.as_str()]);
        let line = node.start_position().row + 1;
        self.node(&id, &name, line, "code");
        self.edge(&self.owner(), &id, "contains", line);
        if let Some(list) = children(node).find(|n| n.kind() == "list_lit") {
            for parent in children(list).filter(|n| n.kind() == "sym_lit") {
                let name = read_text(parent, self.source);
                let target = self.class_ref(&name);
                self.edge(&id, &target, "inherits", list.start_position().row + 1);
            }
        }
    }
    fn docstring(&mut self, node: Node<'tree>, owner: &str) {
        if let Some(doc) = children(node).find(|n| n.kind() == "str_lit") {
            let text = read_text(doc, self.source);
            let text = text.trim_matches('"');
            if !text.is_empty() {
                let id = cl_id([owner, "rationale"]);
                let label: String = text.chars().take(120).collect();
                let line = doc.start_position().row + 1;
                self.node(&id, &label, line, "rationale");
                self.edge(&id, owner, "rationale_for", line);
            }
        }
    }
    fn defun(&mut self, node: Node<'tree>) {
        let Some(header) = children(node).find(|n| n.kind() == "defun_header") else {
            return;
        };
        let mut keyword = "defun";
        let mut name = None;
        for n in children(header) {
            if n.kind() == "defun_keyword"
                && let Some(k) = children(n)
                    .find(|n| matches!(n.kind(), "defun" | "defmethod" | "defgeneric" | "defmacro"))
            {
                keyword = k.kind();
            }
            if n.kind() == "sym_lit" && name.is_none() {
                name = Some(read_text(n, self.source));
            }
        }
        let Some(name) = name.filter(|s| !s.is_empty()) else {
            return;
        };
        let line = node.start_position().row + 1;
        let id = cl_id([self.stem.as_str(), name.as_str()]);
        let label = match keyword {
            "defmethod" => format!(".{name}()"),
            "defmacro" => format!("{name} (macro)"),
            "defgeneric" => format!("{name} (generic)"),
            _ => format!("{name}()"),
        };
        self.node(&id, &label, line, "code");
        self.edge(
            &self.owner(),
            &id,
            if keyword == "defmethod" {
                "method"
            } else {
                "contains"
            },
            line,
        );
        if keyword == "defmethod"
            && let Some(params) = children(header).find(|n| n.kind() == "list_lit")
        {
            for param in children(params).filter(|n| n.kind() == "list_lit") {
                if let Some(spec) = children(param).filter(|n| n.kind() == "sym_lit").nth(1) {
                    let target = cl_id([self.stem.as_str(), read_text(spec, self.source).as_str()]);
                    self.edge(&id, &target, "specializes", param.start_position().row + 1);
                }
            }
        }
        self.docstring(node, &id);
        let body: Vec<_> = children(node)
            .filter(|n| !matches!(n.kind(), "(" | ")" | "defun_header" | "str_lit"))
            .collect();
        if !body.is_empty() {
            self.bodies.push((id, body));
        }
    }
    fn def_name(&self, node: Node<'tree>, keyword: &str) -> Option<String> {
        let mut seen = false;
        for n in children(node) {
            if !seen {
                if n.kind() == "sym_lit" && read_text(n, self.source).to_lowercase() == keyword {
                    seen = true;
                }
                continue;
            }
            if n.kind() == "sym_lit" {
                return Some(read_text(n, self.source));
            }
            if n.kind() == "list_lit" {
                return self.first_sym(n);
            }
        }
        None
    }
    fn def_body(&self, node: Node<'tree>, keyword: &str) -> Vec<Node<'tree>> {
        let nodes: Vec<_> = children(node)
            .filter(|n| !matches!(n.kind(), "(" | ")"))
            .collect();
        let mut index = 0;
        if nodes.get(index).is_some_and(|n| {
            n.kind() == "sym_lit" && read_text(*n, self.source).to_lowercase() == keyword
        }) {
            index += 1;
        }
        if nodes
            .get(index)
            .is_some_and(|n| matches!(n.kind(), "sym_lit" | "list_lit"))
        {
            index += 1;
        }
        if nodes.get(index).is_some_and(|n| n.kind() == "list_lit") {
            index += 1;
        }
        if nodes.get(index).is_some_and(|n| n.kind() == "str_lit") {
            index += 1;
        }
        nodes[index..].to_vec()
    }
    fn def_form(&mut self, node: Node<'tree>, keyword: &str) {
        let Some(name) = self.def_name(node, keyword).filter(|s| !s.is_empty()) else {
            return;
        };
        let line = node.start_position().row + 1;
        let id = cl_id([self.stem.as_str(), name.as_str()]);
        let keyword = keyword.to_lowercase();
        let (label, callable) = if DATA.contains(&keyword.as_str()) {
            (name, false)
        } else if MACROS.contains(&keyword.as_str()) || keyword.contains("macro") {
            (format!("{name} (macro)"), true)
        } else {
            (format!("{name}()"), true)
        };
        self.node(&id, &label, line, "code");
        self.edge(&self.owner(), &id, "contains", line);
        if !VALUES.contains(&keyword.as_str()) {
            self.docstring(node, &id);
        }
        if callable {
            let body = self.def_body(node, &keyword);
            if !body.is_empty() {
                self.bodies.push((id, body));
            }
        }
    }
    fn process_form(&mut self, node: Node<'tree>) -> bool {
        if let Some(n) = children(node).find(|n| n.kind() == "defun") {
            self.defun(n);
            return true;
        }
        let Some(first) = self.first_sym(node).filter(|s| !s.is_empty()) else {
            return false;
        };
        let first = first.to_lowercase();
        match first.as_str() {
            "defpackage" => self.defpackage(node),
            "in-package" => {
                for n in children(node) {
                    if n.kind() == "kwd_lit" {
                        self.package = Some(self.keyword(n));
                        break;
                    }
                    if n.kind() == "sym_lit" && read_text(n, self.source) != "in-package" {
                        self.package = Some(read_text(n, self.source));
                        break;
                    }
                }
            }
            // `define-condition` has defclass's shape; dispatching it here (before
            // the data-definer fallback) keeps its superclass edges (upstream PR #3544).
            "defclass" | "define-condition" => self.defclass(node),
            "require" | "ql:quickload" => {
                if let Some(n) = children(node).find(|n| matches!(n.kind(), "kwd_lit" | "str_lit"))
                {
                    let name = if n.kind() == "kwd_lit" {
                        self.keyword(n)
                    } else {
                        read_text(n, self.source).trim_matches('"').into()
                    };
                    if !name.is_empty() {
                        let target = self.import_stub(&name);
                        self.edge(
                            &self.file_id.clone(),
                            &target,
                            "imports",
                            node.start_position().row + 1,
                        );
                    }
                }
            }
            keyword
                if DATA.contains(&keyword)
                    || MACROS.contains(&keyword)
                    || (!NOT_DEFINERS.contains(&keyword)
                        && (keyword.starts_with("def") || keyword.starts_with("define-"))) =>
            {
                self.def_form(node, keyword)
            }
            _ => return false,
        }
        true
    }
    fn walk_forms(&mut self, parent: Node<'tree>) {
        for node in children(parent) {
            if node.kind() == "list_lit" {
                if !self.process_form(node) {
                    self.walk_forms(node);
                }
            } else if node.kind() == "include_reader_macro" {
                self.walk_forms(node);
            }
        }
    }
    fn walk_calls(&mut self, node: Node<'tree>, caller: &str) {
        if node.kind() == "defun" {
            return;
        }
        if node.kind() == "list_lit"
            && let Some(callee) = self
                .first_sym(node)
                .filter(|s| !s.is_empty() && !SPECIAL.contains(&s.to_lowercase().as_str()))
            && let Some(target) = self
                .labels
                .get(&callee.to_lowercase())
                .filter(|t| !t.is_empty() && *t != caller)
                .cloned()
            && self.pairs.insert((caller.into(), target.clone()))
        {
            self.edge(caller, &target, "calls", node.start_position().row + 1);
        }
        for child in children(node) {
            self.walk_calls(child, caller);
        }
    }
}
pub fn extract_commonlisp(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&match crate::grammar!(
        "grammar-commonlisp",
        "tree_sitter_commonlisp",
        tree_sitter_commonlisp::LANGUAGE_COMMONLISP
    ) {
        Ok(l) => l,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    }) {
        return json!({"nodes":[],"edges":[],"error":e.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Common Lisp parse cancelled"});
    };
    let stem = make_id([file_stem(path).as_str()]);
    let path_text = fsutil::path_string(path);
    let id = cl_id([stem.as_str()]);
    let mut ex = Extractor {
        source: &source,
        path: path_text.clone(),
        stem,
        file_id: id.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        package: None,
        labels: HashMap::new(),
        pairs: HashSet::new(),
    };
    let parts = path_syntax::posix_parts(&path_text);
    ex.node(&id, path_syntax::posix_name(&parts), 1, "code");
    ex.walk_forms(tree.root_node());
    for node in &ex.nodes {
        let label = node["label"]
            .as_str()
            .unwrap_or("")
            .replace(" (macro)", "")
            .replace(" (generic)", "")
            .trim_matches(['(', ')'])
            .trim_start_matches('.')
            .to_lowercase();
        ex.labels
            .insert(label, node["id"].as_str().unwrap_or("").into());
    }
    for (caller, body) in ex.bodies.clone() {
        for node in body {
            ex.walk_calls(node, &caller);
        }
    }
    ex.edges.retain(|e| {
        ex.seen.contains(e["source"].as_str().unwrap_or(""))
            && (ex.seen.contains(e["target"].as_str().unwrap_or(""))
                || matches!(e["relation"].as_str(), Some("imports" | "imports_from")))
    });
    json!({"nodes":ex.nodes,"edges":ex.edges})
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-commonlisp")]
    fn grammar_matches_the_pinned_python_package() {
        let language: tree_sitter::Language = tree_sitter_commonlisp::LANGUAGE_COMMONLISP.into();
        assert_eq!(language.node_kind_count(), 182);
        assert_eq!(language.parse_state_count(), 4165);
        assert_eq!(language.abi_version(), 14);
    }

    #[test]
    fn define_condition_keeps_its_superclass_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.lisp");
        std::fs::write(
            &path,
            "(define-condition app-error (error) ())\n(define-condition my-error (app-error) ((code :initarg :code)))\n",
        )
        .unwrap();
        let result = super::extract_commonlisp(&path);
        let inherits: Vec<(String, String)> = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "inherits")
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                )
            })
            .collect();
        let id = |label: &str| -> String {
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["label"] == label)
                .map(|n| n["id"].as_str().unwrap().into())
                .unwrap_or_else(|| panic!("{label}: {:?}", result["nodes"]))
        };
        assert_eq!(
            inherits,
            [
                (id("app-error"), id("error")),
                (id("my-error"), id("app-error"))
            ]
        );
    }
}
