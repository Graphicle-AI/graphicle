// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
use super::{
    base::{make_id, read_text},
    engine::Engine,
    engine_types::{children, first_child, named, set},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;
pub fn lookup_barrier(n: Node<'_>, s: &[u8]) -> bool {
    fn visit(n: Node<'_>, s: &[u8], direct: bool) -> bool {
        for c in children(n) {
            match c.kind() {
                "class" | "module" => continue,
                "singleton_class" | "method" | "singleton_method" => {
                    if !direct {
                        return true;
                    }
                    continue;
                }
                "alias" | "undef" => return true,
                _ => {}
            }
            if c.kind() == "call" {
                let recv = c.child_by_field_name("receiver");
                let method = c.child_by_field_name("method");
                if recv.is_none_or(|r| read_text(r, s) == "self")
                    && method
                        .is_some_and(|m| set("_RUBY_LOOKUP_MUTATORS").contains(&read_text(m, s)))
                {
                    return true;
                }
            }
            if visit(c, s, false) {
                return true;
            }
        }
        false
    }
    visit(n, s, true)
}
pub fn new_class_name(n: Option<Node<'_>>, s: &[u8]) -> Option<String> {
    let n = n.filter(|n| n.kind() == "call")?;
    let recv = n
        .child_by_field_name("receiver")
        .filter(|n| n.kind() == "constant")?;
    let meth = n.child_by_field_name("method")?;
    (read_text(meth, s) == "new").then(|| read_text(recv, s))
}
pub fn local_class_bindings(n: Node<'_>, s: &[u8]) -> BTreeMap<String, Option<String>> {
    fn visit(n: Node<'_>, s: &[u8], out: &mut BTreeMap<String, Option<String>>) {
        for c in children(n) {
            if matches!(c.kind(), "method" | "singleton_method") {
                continue;
            }
            if c.kind() == "assignment"
                && let Some(l) = c
                    .child_by_field_name("left")
                    .filter(|l| l.kind() == "identifier")
            {
                let var = read_text(l, s);
                let class = new_class_name(c.child_by_field_name("right"), s);
                if let Some(old) = out.get_mut(&var) {
                    if *old != class {
                        *old = None;
                    }
                } else if class.is_some() {
                    out.insert(var, class);
                }
            }
            visit(c, s, out);
        }
    }
    let mut out = BTreeMap::new();
    visit(n, s, &mut out);
    out
}
/// `_ruby_local_names` (0.9.74 #3960): every name bound as a local or
/// parameter in one method body, so a paren-less `identifier` can be told
/// apart as a send to `self` (`build`) or a variable read (`x`). Nested
/// `def` bodies open their own scope and are skipped; a missed binding only
/// suppresses a candidate call.
pub fn local_names(body: Node<'_>, s: &[u8]) -> BTreeSet<String> {
    const PARAM_LISTS: [&str; 4] = [
        "method_parameters",
        "block_parameters",
        "lambda_parameters",
        "bare_parameters",
    ];
    const PARAM_WRAPPERS: [&str; 7] = [
        "optional_parameter",
        "splat_parameter",
        "hash_splat_parameter",
        "keyword_parameter",
        "block_parameter",
        "destructured_parameter",
        "forward_parameter",
    ];
    fn first_identifier(n: Node<'_>) -> Option<Node<'_>> {
        if n.kind() == "identifier" {
            return Some(n);
        }
        children(n).into_iter().find_map(first_identifier)
    }
    fn add_params(list: Node<'_>, s: &[u8], names: &mut BTreeSet<String>) {
        for c in children(list) {
            if c.kind() == "identifier" {
                names.insert(read_text(c, s));
            } else if PARAM_WRAPPERS.contains(&c.kind())
                && let Some(ident) = first_identifier(c)
            {
                names.insert(read_text(ident, s));
            }
        }
    }
    fn add_targets(n: Node<'_>, s: &[u8], names: &mut BTreeSet<String>) {
        if n.kind() == "identifier" {
            names.insert(read_text(n, s));
            return;
        }
        for c in children(n) {
            add_targets(c, s, names);
        }
    }
    fn visit(n: Node<'_>, s: &[u8], names: &mut BTreeSet<String>) {
        for c in children(n) {
            if matches!(c.kind(), "method" | "singleton_method") {
                continue;
            }
            match c.kind() {
                "assignment" | "operator_assignment" => {
                    if let Some(left) = c.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            names.insert(read_text(left, s));
                        } else if !matches!(left.kind(), "call" | "element_reference") {
                            add_targets(left, s, names);
                        }
                    }
                }
                kind if PARAM_LISTS.contains(&kind) => add_params(c, s, names),
                "exception_variable" => {
                    if let Some(ident) = first_identifier(c) {
                        names.insert(read_text(ident, s));
                    }
                }
                "for" => {
                    for sub in children(c) {
                        if sub.kind() == "identifier" {
                            names.insert(read_text(sub, s));
                        } else if sub.kind() == "left_assignment_list" {
                            add_targets(sub, s, names);
                        }
                    }
                }
                _ => {}
            }
            visit(c, s, names);
        }
    }
    let mut names = BTreeSet::new();
    if let Some(parent) = body.parent() {
        for c in children(parent) {
            if PARAM_LISTS.contains(&c.kind()) {
                add_params(c, s, &mut names);
            }
        }
    }
    visit(body, s, &mut names);
    names
}
fn full(n: Node<'_>, s: &[u8]) -> String {
    if matches!(n.kind(), "constant" | "scope_resolution") {
        read_text(n, s).trim().into()
    } else {
        String::new()
    }
}
pub fn external_owners(root: Node<'_>, s: &[u8]) -> BTreeSet<String> {
    fn refs(n: Node<'_>, s: &[u8], rhs: bool, array: bool) -> BTreeSet<String> {
        if matches!(n.kind(), "constant" | "scope_resolution") {
            return BTreeSet::from([full(n, s)]);
        }
        let traverse = if rhs {
            matches!(
                n.kind(),
                "parenthesized_statements" | "right_assignment_list"
            ) || (array && n.kind() == "array")
        } else {
            matches!(
                n.kind(),
                "left_assignment_list" | "parenthesized_statements" | "splat_argument"
            )
        };
        if !traverse {
            return BTreeSet::new();
        }
        named(n)
            .into_iter()
            .flat_map(|c| refs(c, s, rhs, array))
            .collect()
    }
    fn visit(n: Node<'_>, s: &[u8], out: &mut BTreeSet<String>) {
        if matches!(n.kind(), "assignment" | "operator_assignment")
            && let Some(left) = n.child_by_field_name("left")
        {
            let lhs = refs(left, s, false, false);
            if !lhs.is_empty() {
                out.extend(lhs.into_iter().map(|s| format!("{s}::*")));
                if n.kind() == "assignment"
                    && let Some(right) = n.child_by_field_name("right")
                {
                    out.extend(
                        refs(right, s, true, left.kind() == "left_assignment_list")
                            .into_iter()
                            .map(|s| format!("{s}::*")),
                    );
                }
            }
        }
        let field = match n.kind() {
            "class" | "module" => Some("name"),
            "singleton_method" => Some("object"),
            "singleton_class" => Some("value"),
            _ => None,
        };
        if let Some(field) = field {
            if let Some(name) = n.child_by_field_name(field)
                && (!matches!(n.kind(), "class" | "module") || name.kind() == "scope_resolution")
                && !full(name, s).is_empty()
            {
                out.insert(full(name, s));
            }
            if n.kind() == "singleton_method" {
                return;
            }
        }
        if n.kind() == "call"
            && let (Some(r), Some(m)) = (
                n.child_by_field_name("receiver"),
                n.child_by_field_name("method"),
            )
            && (set("_RUBY_LOOKUP_MUTATORS").contains(&read_text(m, s))
                || ["class_eval", "class_exec", "module_eval", "module_exec"]
                    .contains(&read_text(m, s).as_str()))
            && !full(r, s).is_empty()
        {
            out.insert(full(r, s));
        }
        if n.kind() == "method" {
            return;
        }
        for c in children(n) {
            visit(c, s, out);
        }
    }
    let mut out = BTreeSet::new();
    visit(root, s, &mut out);
    out
}
impl<'a, 't> Engine<'a, 't> {
    pub(crate) fn ruby_file_metadata(&mut self, root: Node<'t>) {
        let mut meta = json!({"ruby_resolution_schema":1});
        let mut stack = vec![root];
        let mut refinement = false;
        while let Some(n) = stack.pop() {
            if n.kind() == "call"
                && n.child_by_field_name("method")
                    .is_some_and(|m| ["refine", "using"].contains(&self.text(m).as_str()))
            {
                refinement = true;
                break;
            }
            stack.extend(children(n));
        }
        if lookup_barrier(root, self.source) || refinement {
            meta["ruby_lookup_unsafe"] = json!(true);
        }
        let owners = external_owners(root, self.source);
        if !owners.is_empty() {
            meta["ruby_external_method_owners"] = json!(owners);
        }
        self.nodes[0]["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&meta)));
    }
    pub(crate) fn ruby_extra(&mut self, n: Node<'t>, parent: Option<&str>) -> bool {
        if n.kind() == "singleton_class"
            && let (Some(parent), Some(value), Some(body)) = (
                parent,
                n.child_by_field_name("value"),
                n.child_by_field_name("body"),
            )
            && self.text(value) == "self"
        {
            if lookup_barrier(body, self.source)
                && let Some(c) = self.nodes.iter_mut().find(|c| c["id"] == parent)
            {
                if !c["metadata"].is_object() {
                    c["metadata"] = json!({});
                }
                c["metadata"]["ruby_lookup_unsafe"] = json!(true);
            }
            self.singleton_depth += 1;
            for c in children(body) {
                self.walk(c, Some(parent));
            }
            self.singleton_depth -= 1;
            return true;
        }
        if n.kind() != "assignment" {
            return false;
        }
        let (Some(left), Some(right)) = (
            n.child_by_field_name("left"),
            n.child_by_field_name("right"),
        ) else {
            return false;
        };
        if left.kind() != "constant" || right.kind() != "call" {
            return false;
        }
        let (Some(recv), Some(method)) = (
            right.child_by_field_name("receiver"),
            right.child_by_field_name("method"),
        ) else {
            return false;
        };
        if recv.kind() != "constant" {
            return false;
        }
        let r = self.text(recv);
        let m = self.text(method);
        if ![("Struct", "new"), ("Class", "new"), ("Data", "define")]
            .contains(&(r.as_str(), m.as_str()))
        {
            return false;
        }
        let name = self.text(left);
        if name.is_empty() {
            return false;
        }
        let segments = name.split("::").map(str::to_string).collect::<Vec<_>>();
        let mut names = self.ruby_namespace.clone();
        names.extend(segments.clone());
        let name = names.join("::");
        let line = n.start_position().row + 1;
        let id = make_id([self.stem.as_str(), name.as_str()]);
        self.add_node(&id, &name, line, None, json!({"ruby_lookup_unsafe":true}));
        self.callable.insert(id.clone());
        self.classes.insert(id.clone());
        let file = self.file.clone();
        self.edge(&file, &id, "contains", line, None, Value::Null);
        if r == "Class"
            && let Some(args) = first_child(right, &["argument_list"])
            && let Some(base) = first_child(args, &["constant", "scope_resolution"])
        {
            let raw = self.text(base);
            let name = raw.rsplit("::").next().unwrap_or("");
            let target = self.ensure(name, true);
            self.edge(&id,&target,"inherits",line,None,json!({"ruby_superclass_ref":raw,"ruby_lexical_scopes":self.ruby_lexical.iter().rev().collect::<Vec<_>>()}));
        }
        if let Some(b) = first_child(right, &["do_block", "block"]) {
            let body = first_child(b, &["body_statement"]).unwrap_or(b);
            self.ruby_namespace.extend(segments.clone());
            self.ruby_lexical.push(name);
            for c in children(body) {
                self.walk(c, Some(&id));
            }
            self.ruby_lexical.pop();
            self.ruby_namespace
                .truncate(self.ruby_namespace.len() - segments.len());
        }
        true
    }
}
