// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/verilog.py
//! Native Verilog tree traversal and the upstream SystemVerilog semantic pass.
use super::base::{file_stem, make_id, read_text};
use crate::{Result, fsutil, io_compat, path_syntax, regex_compat, unicode15::WORD_RANGES};
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
fn child<'a>(node: Option<Node<'a>>, kind: &str) -> Option<Node<'a>> {
    children(node?).find(|n| n.kind() == kind)
}
pub fn first_identifier(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    for n in children(node?) {
        if n.kind() == "simple_identifier" {
            return Some(read_text(n, source));
        }
        if let Some(found) = first_identifier(Some(n), source).filter(|s| !s.is_empty()) {
            return Some(found);
        }
    }
    None
}
const BUILTIN_TYPES: &[&str] = &[
    "bit",
    "logic",
    "reg",
    "wire",
    "int",
    "integer",
    "shortint",
    "longint",
    "byte",
    "time",
    "real",
    "shortreal",
    "void",
    "string",
    "type",
    "event",
    "mailbox",
    "semaphore",
    "process",
    "chandle",
];
const NON_TYPE_WORDS: &[&str] = &[
    "return",
    "if",
    "else",
    "for",
    "foreach",
    "while",
    "case",
    "begin",
    "end",
    "function",
    "task",
    "class",
    "endclass",
    "endfunction",
    "endtask",
];
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
fn fancy(pattern: &str) -> fancy_regex::Regex {
    let mut word = String::from("[");
    for (a, b) in WORD_RANGES {
        word.push_str(&format!(r"\u{{{a:x}}}-\u{{{b:x}}}"));
    }
    word.push(']');
    fancy_regex::Regex::new(
        &pattern
            .replace(r"\w", &word)
            .replace(r"\s", fsutil::PY_WHITESPACE),
    )
    .expect("static SystemVerilog regex")
}
struct Patterns {
    block_comment: regex::Regex,
    line_comment: regex::Regex,
    class: fancy_regex::Regex,
    extends: fancy_regex::Regex,
    implements: fancy_regex::Regex,
    type_param: fancy_regex::Regex,
    remove_function: fancy_regex::Regex,
    function: fancy_regex::Regex,
    field: regex::Regex,
    param: regex::Regex,
    head: regex::Regex,
    generic: regex::Regex,
}
static P: LazyLock<Patterns> = LazyLock::new(|| {
    let inner = r"(?:[^()]|\([^()]*\))*";
    let parens = format!(r"\({inner}\)");
    Patterns {
        block_comment: regex::Regex::new(r"(?s)/\*.*?\*/").unwrap(),
        line_comment: regex::Regex::new(r"//[^\n]*").unwrap(),
        class: fancy(
            r"(?s)(?<!\w)(?:(interface)\s+)?class\s+(\w+)([^;{]*)\s*;(.*?)(?<!\w)endclass(?!\w)",
        ),
        extends: fancy(r"(?<!\w)extends\s+(\w+)"),
        implements: fancy(r"(?<!\w)implements\s+([^;{]+)"),
        type_param: fancy(r"(?<!\w)type\s+(\w+)"),
        remove_function: fancy(r"(?s)(?<!\w)function(?!\w).*?(?<!\w)endfunction(?!\w)"),
        function: fancy(&format!(
            r"(?m)(?<!\w)function\s+([A-Za-z_]\w*(?:\s*#\s*{parens})?)\s+(\w+)\s*\(({inner})\)\s*;"
        )),
        field: regex_compat::compile(
            r"(?m)^\s*(?:(?:rand|randc|local|protected|static|const|automatic|var)\s+)*([A-Za-z_]\w*(?:\s*#\s*\([^;]+?\))?)\s+\w+\s*;",
        ),
        param: regex_compat::compile(&format!(
            r"^\s*(?:input|output|inout|ref|const\s+ref)?\s*([A-Za-z_]\w*(?:\s*#\s*{parens})?)\s+\w+"
        )),
        head: regex_compat::compile(r"^([A-Za-z_]\w*)"),
        generic: regex_compat::compile(&format!(r"#\s*\(({inner})\)")),
    }
});
pub fn strip_comments(text: &str) -> String {
    P.line_comment
        .replace_all(&P.block_comment.replace_all(text, ""), "")
        .into_owned()
}
pub fn split_type_list(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let item = trim(&text[start..idx]);
                if !item.is_empty() {
                    parts.push(item.to_string());
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let item = trim(&text[start..]);
    if !item.is_empty() {
        parts.push(item.to_string());
    }
    parts
}
pub fn collect_type_refs(
    text: &str,
    generic: bool,
    skip: &HashSet<String>,
) -> Vec<(String, String)> {
    let text = trim(text);
    let mut refs = Vec::new();
    if text.is_empty() {
        return refs;
    }
    if let Some(head) = P.head.captures(text) {
        let name = &head[1];
        if !BUILTIN_TYPES.contains(&name) && !NON_TYPE_WORDS.contains(&name) && !skip.contains(name)
        {
            refs.push((
                name.to_string(),
                if generic { "generic_arg" } else { "type" }.to_string(),
            ));
        }
    }
    if let Some(params) = P.generic.captures(text) {
        for arg in split_type_list(&params[1]) {
            refs.extend(collect_type_refs(&arg, true, skip));
        }
    }
    refs
}
struct Extractor<'a> {
    source: &'a [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    modules: HashSet<String>,
    labels: HashMap<String, String>,
}
impl Extractor<'_> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}"),"confidence_score":1.0}));
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
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","confidence_score":1.0,"source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
    fn scan_modules(&mut self, node: Node<'_>) {
        if node.kind() == "module_declaration"
            && let Some(name) = first_identifier(child(Some(node), "module_header"), self.source)
                .filter(|s| !s.is_empty())
        {
            self.modules.insert(name);
        }
        for n in children(node) {
            self.scan_modules(n);
        }
    }
    fn walk(&mut self, node: Node<'_>, module: Option<&str>) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "class_declaration" | "interface_class_declaration" => return,
            "module_declaration" => {
                if let Some(name) =
                    first_identifier(child(Some(node), "module_header"), self.source)
                        .filter(|s| !s.is_empty())
                {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &name, line);
                    self.edge(&self.file.clone(), &id, "defines", line, None);
                    for n in children(node) {
                        self.walk(n, Some(&id));
                    }
                    return;
                }
            }
            "function_declaration" | "task_declaration" => {
                let function = node.kind() == "function_declaration";
                let body = child(
                    Some(node),
                    if function {
                        "function_body_declaration"
                    } else {
                        "task_body_declaration"
                    },
                );
                let name = first_identifier(
                    child(
                        body,
                        if function {
                            "function_identifier"
                        } else {
                            "task_identifier"
                        },
                    ),
                    self.source,
                );
                if let Some(name) = name.filter(|s| !s.is_empty()) {
                    let parent = module
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&self.file)
                        .to_string();
                    let id = make_id([parent.as_str(), name.as_str()]);
                    self.node(
                        &id,
                        &if function { format!("{name}()") } else { name },
                        line,
                    );
                    self.edge(&parent, &id, "contains", line, None);
                }
            }
            "package_import_declaration" => {
                for n in children(node).filter(|n| n.kind() == "package_import_item") {
                    let text = read_text(n, self.source);
                    let name = trim(text.split("::").next().unwrap_or(""));
                    if !name.is_empty() {
                        let id = make_id([name]);
                        self.node(&id, name, line);
                        let from = module
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&self.file)
                            .to_string();
                        self.edge(&from, &id, "imports_from", line, None);
                    }
                }
            }
            "module_instantiation" | "checker_instantiation" => {
                if let Some(module) = module.filter(|s| !s.is_empty()) {
                    let typ = if let Some(n) = node.child_by_field_name("module_type") {
                        Some(trim(&read_text(n, self.source)).to_string())
                    } else {
                        first_identifier(Some(node), self.source)
                    };
                    if let Some(typ) = typ.filter(|s| !s.is_empty()) {
                        let id = if self.modules.contains(&typ) {
                            make_id([self.stem.as_str(), typ.as_str()])
                        } else {
                            let id = make_id([typ.as_str()]);
                            if self.seen.insert(id.clone()) {
                                self.nodes.push(json!({"id":id,"label":typ,"file_type":"code","source_file":"","source_location":"","confidence_score":1.0}));
                            }
                            id
                        };
                        self.edge(module, &id, "instantiates", line, None);
                    }
                }
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n, module);
        }
    }
    fn augmented_node(&mut self, id: &str, label: &str, line: usize) {
        self.node(id, label, line);
        self.labels.insert(label.into(), id.into());
    }
    fn typed_edge(
        &mut self,
        source: &str,
        label: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        let target = if let Some(id) = self.labels.get(label) {
            id.clone()
        } else {
            let id = make_id([self.stem.as_str(), label]);
            self.augmented_node(&id, label, line);
            id
        };
        self.edge(source, &target, relation, line, context);
    }
    fn augment(&mut self, raw: &str) -> Result<()> {
        self.labels = self
            .nodes
            .iter()
            .filter_map(|n| Some((n["label"].as_str()?.into(), n["id"].as_str()?.into())))
            .collect();
        let text = strip_comments(raw);
        for capture in P.class.captures_iter(&text) {
            let capture = capture.map_err(crate::Error::from)?;
            let name = &capture[2];
            let header = &capture[3];
            let body = &capture[4];
            // The upstream line helper indexes the original text using a
            // character offset in comment-stripped text. Preserve that contract.
            let offset = text[..capture.get(0).unwrap().start()].chars().count();
            let line = raw.chars().take(offset).filter(|c| *c == '\n').count() + 1;
            let mut skip = HashSet::new();
            for c in P.type_param.captures_iter(header) {
                skip.insert(c.map_err(crate::Error::from)?[1].to_string());
            }
            let id = make_id([self.stem.as_str(), name]);
            self.augmented_node(&id, name, line);
            self.edge(&self.file.clone(), &id, "defines", line, None);
            if let Some(ext) = P.extends.captures(header).map_err(crate::Error::from)? {
                self.typed_edge(&id, &ext[1], "inherits", line, None);
            }
            if let Some(imp) = P.implements.captures(header).map_err(crate::Error::from)? {
                for interface in split_type_list(&imp[1]) {
                    self.typed_edge(
                        &id,
                        trim(interface.split('#').next().unwrap_or("")),
                        "implements",
                        line,
                        None,
                    );
                }
            }
            let mut fields = String::new();
            let mut start = 0;
            for m in P.remove_function.find_iter(body) {
                let m = m.map_err(crate::Error::from)?;
                fields.push_str(&body[start..m.start()]);
                fields.push_str(&"\n".repeat(m.as_str().bytes().filter(|b| *b == b'\n').count()));
                start = m.end();
            }
            fields.push_str(&body[start..]);
            for field in P.field.captures_iter(&fields) {
                let field_line = line
                    + fields[..field.get(1).unwrap().start()]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
                for (name, role) in collect_type_refs(&field[1], false, &skip) {
                    self.typed_edge(
                        &id,
                        &name,
                        "references",
                        field_line,
                        Some(if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "field"
                        }),
                    );
                }
            }
            for fm in P.function.captures_iter(body) {
                let fm = fm.map_err(crate::Error::from)?;
                let name = &fm[2];
                let function_line = line
                    + body[..fm.get(0).unwrap().start()]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
                let function = make_id([id.as_str(), name]);
                self.augmented_node(&function, name, function_line);
                self.edge(&id, &function, "method", function_line, None);
                for (name, role) in collect_type_refs(&fm[1], false, &skip) {
                    self.typed_edge(
                        &function,
                        &name,
                        "references",
                        function_line,
                        Some(if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "return_type"
                        }),
                    );
                }
                for param in split_type_list(&fm[3]) {
                    if let Some(pm) = P.param.captures(&param) {
                        for (name, role) in collect_type_refs(&pm[1], false, &skip) {
                            self.typed_edge(
                                &function,
                                &name,
                                "references",
                                function_line,
                                Some(if role == "generic_arg" {
                                    "generic_arg"
                                } else {
                                    "parameter_type"
                                }),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
pub fn extract_verilog(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)})),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&crate::grammar!(
        "grammar-verilog",
        "tree_sitter_verilog",
        tree_sitter_verilog::LANGUAGE
    )?) {
        return Ok(json!({"nodes":[],"edges":[],"error":e.to_string()}));
    }
    let Some(tree) = parser.parse(&source, None) else {
        return Ok(json!({"nodes":[],"edges":[],"error":"Verilog parse cancelled"}));
    };
    let text = fsutil::path_string(path);
    let file = make_id([text.as_str()]);
    let parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts).to_string();
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem: file_stem(path),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        modules: HashSet::new(),
        labels: HashMap::new(),
    };
    ex.node(&file, &name, 1);
    ex.scan_modules(tree.root_node());
    ex.walk(tree.root_node(), None);
    ex.augment(&String::from_utf8_lossy(&source))?;
    Ok(json!({"nodes":ex.nodes,"edges":ex.edges}))
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-verilog")]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_verilog::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 1223);
        assert_eq!(l.parse_state_count(), 20534);
        assert_eq!(l.abi_version(), 14);
    }
}
