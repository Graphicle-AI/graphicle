// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/pascal.py
//! Pascal/Delphi AST traversal and the original regex fallback.
use super::{
    base::{file_stem, make_id, read_text},
    pascal_paths,
};
use crate::{Result, fsutil, io_compat, regex_compat};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;
macro_rules! pattern {
    ($name:ident,$text:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| regex_compat::compile($text));
    };
}
pattern!(TOKEN, r"(?s)'(?:''|[^'])*'|\{[^}]*\}|\(\*.*?\*\)|//[^\n]*");
pattern!(
    MODULE,
    r"(?i)\b(unit|program|library)\s+([A-Za-z_][\w.]*)\s*;"
);
pattern!(USES, r"(?is)\buses\b\s*([^;]+);");
pattern!(
    TYPE,
    r"(?i)\b(?P<name>[A-Za-z_]\w*)(?:\s*<[^>]+>)?\s*=\s*(?:packed\s+)?(?P<kind>class|interface)\b(?:\s*\(\s*(?P<bases>[^)]*)\s*\))?"
);
pattern!(END, r"(?i)\bend\s*;");
pattern!(
    METHOD,
    r"(?i)\b(?:procedure|function|constructor|destructor)\s+(?P<name>[A-Za-z_]\w*)(?:\s*\([^)]*\))?(?:\s*:\s*[\w<>,\s.]+)?\s*;"
);
pattern!(
    IMPL,
    r"(?i)\b(?:procedure|function|constructor|destructor)\s+(?P<qual>[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?)(?:\s*<[^>]+>)?(?:\s*\([^)]*\))?(?:\s*:\s*[\w<>,\s.]+)?\s*;"
);
pattern!(BLOCK, r"(?i)\b(begin|end|case|try|asm|record)\b");
pattern!(BEGIN, r"(?i)\bbegin\b");
pattern!(CALL, r"\b([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*[(;]");
pattern!(INTERFACE, r"(?i)\binterface\b");
pattern!(IMPLEMENTATION, r"(?i)\bimplementation\b");
pattern!(FINAL, r"(?i)\b(initialization|finalization)\b");
pattern!(IN, r"(?i)\s+in\s+");
pattern!(UNIT_NAME, r"^[A-Za-z_][\w.]*$");
pattern!(BASE_NAME, r"^[A-Za-z_]\w*$");
const KEYWORDS: &[&str] = &[
    "begin",
    "end",
    "if",
    "then",
    "else",
    "while",
    "do",
    "for",
    "to",
    "downto",
    "repeat",
    "until",
    "case",
    "of",
    "try",
    "finally",
    "except",
    "with",
    "inherited",
    "result",
    "var",
    "const",
    "type",
    "nil",
    "true",
    "false",
    "exit",
    "break",
    "continue",
    "uses",
    "unit",
    "program",
    "library",
    "interface",
    "implementation",
    "initialization",
    "finalization",
    "procedure",
    "function",
    "constructor",
    "destructor",
    "class",
    "record",
    "object",
    "array",
    "string",
    "integer",
    "boolean",
    "real",
    "char",
    "writeln",
    "write",
    "readln",
    "read",
    "assigned",
    "length",
    "high",
    "low",
    "inc",
    "dec",
    "new",
    "dispose",
    "setlength",
    "copy",
    "pos",
    "trim",
    "format",
    "inttostr",
    "strtoint",
    "ord",
    "chr",
    "sizeof",
    "create",
    "free",
    "destroy",
];
fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
pub fn strip_comments(s: &str) -> String {
    TOKEN
        .replace_all(s, |c: &regex::Captures| {
            let t = &c[0];
            if t.starts_with('\'') {
                t.to_string()
            } else {
                t.chars()
                    .map(|c| if c == '\n' { '\n' } else { ' ' })
                    .collect()
            }
        })
        .into_owned()
}
pub fn split_sections(s: &str) -> (&str, usize, &str, usize) {
    if let (Some(a), Some(b)) = (INTERFACE.find(s), IMPLEMENTATION.find(s)) {
        let end = FINAL
            .find(&s[b.end()..])
            .map_or(s.len(), |m| b.end() + m.start());
        let iface = if a.end() > b.start() {
            ""
        } else {
            &s[a.end()..b.start()]
        };
        (iface, a.end(), &s[b.end()..end], b.end())
    } else {
        ("", 0, s, 0)
    }
}
pub fn split_uses(s: &str) -> Vec<String> {
    s.split(',')
        .filter_map(|p| {
            let p = trim(p);
            let p = IN.find(p).map_or(p, |m| &p[..m.start()]);
            let p = trim(p).trim_matches(';');
            (!p.is_empty() && UNIT_NAME.is_match(p)).then(|| p.to_string())
        })
        .collect()
}
pub fn split_bases(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i64;
    let mut buf = String::new();
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut buf));
                continue;
            }
            _ => {}
        }
        buf.push(c);
    }
    out.push(buf);
    out.into_iter()
        .filter_map(|b| {
            let b = trim(&b);
            // Python's <.*$ is not DOTALL: a generic spanning a newline
            // must not turn into a valid base merely by truncating at '<'.
            let line_start = b.rfind('\n').map_or(0, |i| i + 1);
            let b = b[line_start..]
                .find('<')
                .map_or(b, |i| &b[..line_start + i]);
            BASE_NAME.is_match(b).then(|| b.to_string())
        })
        .collect()
}
pub fn find_body(s: &str, start: usize) -> (usize, usize) {
    let Some(m) = BEGIN.find_at(s, start.min(s.len())) else {
        return (0, 0);
    };
    let a = m.end();
    let mut depth = 1;
    for m in BLOCK.find_iter(&s[a..]) {
        let kw = m.as_str().to_lowercase();
        if kw == "end" {
            depth -= 1;
            if depth == 0 {
                return (a, a + m.start());
            }
        } else {
            depth += 1;
        }
    }
    (a, s.len())
}
#[derive(Default)]
pub struct CalleeResolver {
    bases: HashMap<String, Vec<String>>,
    classes: HashMap<String, HashMap<String, Vec<String>>>,
    module: HashMap<String, Vec<String>>,
    global: HashMap<String, Vec<String>>,
    owners: HashMap<String, String>,
}
impl CalleeResolver {
    pub fn new(records: &[(String, String, String)], edges: &[Value], module: &str) -> Self {
        let mut s = Self::default();
        for e in edges {
            if e["relation"] == "inherits"
                && let (Some(a), Some(b)) = (e["source"].as_str(), e["target"].as_str())
            {
                s.bases.entry(a.into()).or_default().push(b.into());
            }
        }
        for (id, container, name) in records {
            s.owners.insert(id.clone(), container.clone());
            s.global.entry(name.clone()).or_default().push(id.clone());
            if container == module {
                s.module.entry(name.clone()).or_default().push(id.clone());
            } else {
                s.classes
                    .entry(container.clone())
                    .or_default()
                    .entry(name.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        s
    }
    pub fn resolve(&self, caller: &str, name: &str) -> Option<String> {
        let one = |xs: &Vec<String>| {
            if xs.len() == 1 {
                Some(xs[0].clone())
            } else {
                None
            }
        };
        if let Some(owner) = self.owners.get(caller) {
            if let Some(xs) = self
                .classes
                .get(owner)
                .and_then(|m| m.get(name))
                .filter(|xs| !xs.is_empty())
            {
                return one(xs);
            }
            let mut queue = VecDeque::from(self.bases.get(owner).cloned().unwrap_or_default());
            let mut seen = HashSet::new();
            while let Some(base) = queue.pop_front() {
                if !seen.insert(base.clone()) {
                    continue;
                }
                if let Some(xs) = self
                    .classes
                    .get(&base)
                    .and_then(|m| m.get(name))
                    .filter(|xs| !xs.is_empty())
                {
                    return one(xs);
                }
                queue.extend(self.bases.get(&base).cloned().unwrap_or_default());
            }
        }
        if let Some(xs) = self.module.get(name).filter(|xs| !xs.is_empty()) {
            return one(xs);
        }
        self.global.get(name).and_then(one)
    }
}
struct Output {
    path: String,
    stem: String,
    file: String,
    module: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    edge_keys: HashSet<(String, String, String)>,
    call_pairs: HashSet<(String, String)>,
    raw: Vec<Value>,
}
impl Output {
    fn new(path: &Path) -> Self {
        let name = fsutil::path_string(path);
        let id = make_id([name.as_str()]);
        let mut out = Self {
            path: name,
            stem: file_stem(path),
            file: id.clone(),
            module: id.clone(),
            nodes: vec![],
            edges: vec![],
            seen: HashSet::new(),
            edge_keys: HashSet::new(),
            call_pairs: HashSet::new(),
            raw: vec![],
        };
        out.node(
            &id,
            &path.file_name().unwrap_or_default().to_string_lossy(),
            1,
        );
        out
    }
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, a: &str, b: &str, rel: &str, line: usize, context: Option<&str>) {
        if self.edge_keys.insert((a.into(), b.into(), rel.into())) {
            let mut e = json!({"source":a,"target":b,"relation":rel,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
            if let Some(c) = context.filter(|v| !v.is_empty()) {
                e["context"] = json!(c);
            }
            self.edges.push(e);
        }
    }
    fn inherit(&mut self, class: &str, base: &str, line: usize) {
        let mut id = make_id([self.stem.as_str(), base]);
        if !self.seen.contains(&id) {
            if let Some(resolved) = pascal_paths::resolve_class(Path::new(&self.path), base) {
                id = resolved;
            } else {
                id = make_id([base]);
                self.node(&id, base, line);
            }
        }
        self.edge(class, &id, "inherits", line, None);
    }
    fn emit_call(&mut self, resolver: &CalleeResolver, caller: &str, name: &str, line: usize) {
        let target = resolver.resolve(caller, name);
        if target.as_deref() == Some(caller) {
            return;
        }
        let Some(target) = target else {
            self.raw.push(json!({"source_file":self.path,"source_location":format!("L{line}"),"caller_nid":caller,"callee":name}));
            return;
        };
        if self.call_pairs.insert((caller.into(), target.clone())) {
            self.edge(caller, &target, "calls", line, Some("call"));
        }
    }
    fn finish(self) -> Value {
        json!({"nodes":self.nodes,"edges":self.edges,"input_tokens":0,"output_tokens":0,"raw_calls":self.raw})
    }
}
fn line(s: &str, byte: usize) -> usize {
    s[..byte].bytes().filter(|c| *c == b'\n').count() + 1
}
pub fn extract_regex(path: &Path) -> Value {
    let raw = match super::base::read_source_text(path, false) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut out = Output::new(path);
    let stripped = strip_comments(&raw);
    if let Some(m) = MODULE.captures(&stripped) {
        let name = &m[2];
        let id = make_id([out.stem.as_str(), name]);
        let n = line(&stripped, m.get(0).unwrap().start());
        out.node(&id, name, n);
        out.edge(&out.file.clone(), &id, "contains", n, None);
        out.module = id;
    }
    let (iface, iface_off, imp, imp_off) = split_sections(&stripped);
    for (text, off) in [(iface, iface_off), (imp, imp_off)] {
        for m in USES.captures_iter(text) {
            let n = line(&stripped, off + m.get(0).unwrap().start());
            for unit in split_uses(&m[1]) {
                let id = pascal_paths::resolve_unit(path, &unit);
                out.edge(&out.module.clone(), &id, "imports", n, Some("import"));
            }
        }
    }
    let (search, off) = if iface.is_empty() {
        (stripped.as_str(), 0)
    } else {
        (iface, iface_off)
    };
    let mut pos = 0;
    while pos < search.len() {
        let Some(m) = TYPE.captures_at(search, pos) else {
            break;
        };
        let whole = m.get(0).unwrap();
        let name = &m["name"];
        let n = line(&stripped, off + whole.start());
        let id = make_id([out.stem.as_str(), name]);
        out.node(&id, name, n);
        out.edge(&out.module.clone(), &id, "contains", n, None);
        for base in split_bases(m.name("bases").map_or("", |m| m.as_str())) {
            out.inherit(&id, &base, n);
        }
        let end = END.find_at(search, whole.end());
        let body = end.map_or("", |e| &search[whole.end()..e.start()]);
        for mm in METHOD.captures_iter(body) {
            let name = &mm["name"];
            let n = line(&stripped, off + whole.end() + mm.get(0).unwrap().start());
            let mid = make_id([id.as_str(), name]);
            out.node(&mid, &format!("{name}()"), n);
            out.edge(&id, &mid, "method", n, None);
        }
        pos = end.map_or(search.len(), |m| m.end());
    }
    let mut records = Vec::new();
    let mut bodies = Vec::new();
    for m in IMPL.captures_iter(imp) {
        let qualified = &m["qual"];
        let n = line(&stripped, imp_off + m.get(0).unwrap().start());
        let (container, relation, label, name) =
            if let Some((class, method)) = qualified.split_once('.') {
                let class = make_id([out.stem.as_str(), class]);
                let owned = out.seen.contains(&class);
                (
                    if owned { class } else { out.module.clone() },
                    if owned { "method" } else { "contains" },
                    format!("{method}()"),
                    method.to_lowercase(),
                )
            } else {
                (
                    out.module.clone(),
                    "contains",
                    format!("{qualified}()"),
                    qualified.to_lowercase(),
                )
            };
        let id = make_id([out.stem.as_str(), qualified]);
        out.node(&id, &label, n);
        out.edge(&container, &id, relation, n, None);
        let (a, b) = find_body(imp, m.get(0).unwrap().end());
        records.push((id.clone(), container, name));
        bodies.push((id, n, if a == 0 { "" } else { &imp[a..b] }));
    }
    let resolver = CalleeResolver::new(&records, &out.edges, &out.module);
    for (caller, n, body) in bodies {
        for m in CALL.captures_iter(body) {
            let name = m[1].rsplit('.').next().unwrap().to_lowercase();
            if KEYWORDS.contains(&name.as_str()) {
                continue;
            }
            let n = n + line(body, m.get(0).unwrap().start()) - 1;
            out.emit_call(&resolver, &caller, &name, n);
        }
    }
    out.finish()
}
fn children(n: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..n.child_count()).filter_map(move |i| n.child(i))
}
fn proc_name(n: Node<'_>, source: &[u8]) -> Option<String> {
    n.child_by_field_name("name")
        .or_else(|| {
            children(n).find(|c| matches!(c.kind(), "identifier" | "genericDot" | "genericTpl"))
        })
        .map(|n| read_text(n, source))
}
struct Extractor<'a, 'tree> {
    source: &'a [u8],
    out: Output,
    bodies: Vec<(String, Node<'tree>, String, String)>,
}
impl<'tree> Extractor<'_, 'tree> {
    fn walk(&mut self, node: Node<'tree>, parent: &str) {
        let n = node.start_position().row + 1;
        match node.kind() {
            "unit" | "program" | "library" => {
                let name = children(node)
                    .find(|c| c.kind() == "moduleName")
                    .map(|n| read_text(n, self.source))
                    .unwrap_or_else(|| {
                        Path::new(&self.out.path)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    });
                let id = make_id([self.out.stem.as_str(), &name]);
                self.out.node(&id, &name, n);
                self.out
                    .edge(&self.out.file.clone(), &id, "contains", n, None);
                self.out.module = id.clone();
                for child in children(node) {
                    self.walk(child, &id);
                }
                return;
            }
            "declUses" => {
                for child in children(node).filter(|c| c.kind() == "moduleName") {
                    let id = pascal_paths::resolve_unit(
                        Path::new(&self.out.path),
                        &read_text(child, self.source),
                    );
                    self.out.edge(parent, &id, "imports", n, Some("import"));
                }
                return;
            }
            "declType" => {
                let name = children(node)
                    .find(|c| c.kind() == "identifier")
                    .map(|n| read_text(n, self.source));
                // An enumerated type (`TColor = (clRed, clGreen);`) nests its
                // values under type -> declEnum -> declEnumValue; each value is a
                // `case_of` case (Graphify 0.9.76, b462631).
                let enumerated = children(node)
                    .filter(|c| c.kind() == "type")
                    .find_map(|c| children(c).find(|gc| gc.kind() == "declEnum"));
                if let (Some(name), Some(enumerated)) = (name.as_deref(), enumerated)
                    && !name.is_empty()
                {
                    let id = make_id([self.out.stem.as_str(), name]);
                    self.out.node(&id, name, n);
                    self.out.edge(parent, &id, "contains", n, None);
                    for value in children(enumerated).filter(|c| c.kind() == "declEnumValue") {
                        let Some(value_name) = children(value)
                            .find(|c| c.kind() == "identifier")
                            .map(|c| read_text(c, self.source))
                            .filter(|s| !s.is_empty())
                        else {
                            continue;
                        };
                        let value_line = value.start_position().row + 1;
                        let value_id = make_id([id.as_str(), &value_name]);
                        self.out.node(&value_id, &value_name, value_line);
                        self.out.edge(&id, &value_id, "case_of", value_line, None);
                    }
                    return;
                }
                let kind = children(node)
                    .find(|c| matches!(c.kind(), "declClass" | "declIntf" | "declHelper"));
                if let (Some(name), Some(kind)) = (name, kind)
                    && !name.is_empty()
                {
                    let id = make_id([self.out.stem.as_str(), &name]);
                    self.out.node(&id, &name, n);
                    self.out.edge(parent, &id, "contains", n, None);
                    for child in children(kind).filter(|c| c.kind() == "typeref") {
                        self.out.inherit(&id, &read_text(child, self.source), n);
                    }
                    for child in children(kind) {
                        self.walk(child, &id);
                    }
                    return;
                }
            }
            "declProcFwd" => {
                if let Some(header) = children(node).find(|c| c.kind() == "declProc")
                    && let Some(name) =
                        proc_name(header, self.source).filter(|s| !s.is_empty() && !s.contains('.'))
                {
                    let id = make_id([parent, &name]);
                    self.out.node(&id, &format!("{name}()"), n);
                    self.out.edge(parent, &id, "method", n, None);
                }
                return;
            }
            "defProc" => {
                if let Some(header) = children(node).find(|c| c.kind() == "declProc")
                    && let Some(name) = proc_name(header, self.source).filter(|s| !s.is_empty())
                {
                    let mut container = parent.to_string();
                    let label = if let Some((class, method)) = name.split_once('.') {
                        let id = make_id([self.out.stem.as_str(), class]);
                        if self.out.seen.contains(&id) {
                            container = id;
                        }
                        format!("{method}()")
                    } else {
                        format!("{name}()")
                    };
                    let id = make_id([self.out.stem.as_str(), &name]);
                    self.out.node(&id, &label, n);
                    self.out.edge(
                        &container,
                        &id,
                        if container != parent {
                            "method"
                        } else {
                            "contains"
                        },
                        n,
                        None,
                    );
                    if let Some(body) = children(node).find(|c| c.kind() == "block") {
                        self.bodies.push((
                            id,
                            body,
                            container,
                            label.strip_suffix("()").unwrap_or(&label).to_lowercase(),
                        ));
                    }
                    return;
                }
            }
            _ => {}
        }
        for child in children(node) {
            self.walk(child, parent);
        }
    }
    fn calls(&mut self, node: Node<'tree>, caller: &str, resolver: &CalleeResolver) {
        let n = node.start_position().row + 1;
        if node.kind() == "exprCall" {
            if let Some(child) = children(node).find(|c| c.is_named() && c.kind() != "exprArgs") {
                let text = read_text(child, self.source);
                let name = text.rsplit('.').next().unwrap_or("");
                if !name.is_empty() {
                    self.out
                        .emit_call(resolver, caller, &name.to_lowercase(), n);
                }
            }
        } else if node.kind() == "statement" {
            let named = children(node).filter(|n| n.is_named()).collect::<Vec<_>>();
            if named.len() == 1 && named[0].kind() == "identifier" {
                let text = read_text(named[0], self.source);
                self.out
                    .emit_call(resolver, caller, &text.to_lowercase(), n);
            }
        }
        for child in children(node) {
            self.calls(child, caller, resolver);
        }
    }
}
pub fn extract(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(_) => return Ok(extract_regex(path)),
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&graphicle_tree_sitter_pascal::LANGUAGE.into())
        .map_err(crate::Error::from)?;
    let Some(tree) = parser.parse(&source, None) else {
        return Ok(extract_regex(path));
    };
    let mut e = Extractor {
        source: &source,
        out: Output::new(path),
        bodies: vec![],
    };
    e.walk(tree.root_node(), &e.out.file.clone());
    let records = e
        .bodies
        .iter()
        .map(|(a, _, b, c)| (a.clone(), b.clone(), c.clone()))
        .collect::<Vec<_>>();
    let resolver = CalleeResolver::new(&records, &e.out.edges, &e.out.module);
    for (id, body, _, _) in e.bodies.clone() {
        e.calls(body, &id, &resolver);
    }
    Ok(e.out.finish())
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let text = r["text"].as_str().unwrap_or("");
    let path = Path::new(r["path"].as_str().unwrap_or(""));
    match r["operation"].as_str().unwrap_or("") {
        "pascal.extract" => extract(path),
        "pascal.regex" => Ok(extract_regex(path)),
        "pascal.strip" => Ok(json!(strip_comments(text))),
        "pascal.uses" => Ok(json!(split_uses(text))),
        "pascal.bases" => Ok(json!(split_bases(text))),
        "pascal.sections" => {
            let (a, ai, b, bi) = split_sections(text);
            Ok(json!([
                a,
                text[..ai].chars().count(),
                b,
                text[..bi].chars().count()
            ]))
        }
        "pascal.body" => {
            let start = r["start"].as_u64().unwrap_or(0) as usize;
            let byte = text.char_indices().nth(start).map_or(text.len(), |p| p.0);
            let (a, b) = find_body(text, byte);
            Ok(json!([
                text[..a].chars().count(),
                text[..b].chars().count()
            ]))
        }
        "pascal.resolve" => {
            let records = r["records"]
                .as_array()
                .ok_or("Expected records")?
                .iter()
                .map(|v| {
                    let row = v.as_array().ok_or("Expected record")?;
                    let n = row.len();
                    if n < 3 {
                        return Err(crate::Error::invalid("Expected record fields"));
                    }
                    Ok((
                        row[0].as_str().ok_or("Expected ID")?.to_string(),
                        row[n - 2].as_str().ok_or("Expected owner")?.to_string(),
                        row[n - 1].as_str().ok_or("Expected name")?.to_string(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let resolver = CalleeResolver::new(
                &records,
                r["edges"].as_array().ok_or("Expected edges")?,
                r["module"].as_str().ok_or("Expected module")?,
            );
            Ok(json!(resolver.resolve(
                r["caller"].as_str().ok_or("Expected caller")?,
                r["name"].as_str().ok_or("Expected name")?
            )))
        }
        "pascal.project_root" => Ok(json!(fsutil::path_string(&pascal_paths::project_root(
            path
        )))),
        "pascal.resolve_unit" => Ok(json!(pascal_paths::resolve_unit(
            path,
            r["name"].as_str().ok_or("Expected name")?
        ))),
        "pascal.resolve_class" => Ok(json!(pascal_paths::resolve_class(
            path,
            r["name"].as_str().ok_or("Expected name")?
        ))),
        "pascal.clear" => {
            pascal_paths::clear();
            Ok(Value::Null)
        }
        _ => Err("Unknown Pascal operation".into()),
    }
}
