// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/dm.py
//! DreamMaker source, DMI PNG metadata, DMM map dictionaries and DMF controls.
use super::base::{file_stem, make_id, read_text};
use crate::{Result, fsutil, io_compat, path_syntax, regex_compat};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;

fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    children(node).find(|n| n.kind() == kind)
}
fn read_failure(path: &Path, error: &std::io::Error) -> Value {
    json!({"nodes":[],"edges":[],"error":io_compat::read_error(error,path)})
}
fn file_node(path: &str, id: &str) -> Value {
    let parts = path_syntax::posix_parts(path);
    json!({"id":id,"label":path_syntax::posix_name(&parts),"file_type":"code","source_file":path,"source_location":"L1"})
}
fn edge(path: &str, source: &str, target: &str, relation: &str, line: usize) -> Value {
    json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":path,"source_location":format!("L{line}"),"weight":1.0})
}
struct Extractor<'tree, 'source> {
    source: &'source [u8],
    path: String,
    stem: String,
    file_id: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<(String, Node<'tree>)>,
    labels: HashMap<String, Vec<String>>,
    paths: HashMap<String, Vec<String>>,
    pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
}
impl<'tree> Extractor<'tree, '_> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if !id.is_empty() && self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(&mut self, source: &str, target: &str, relation: &str, line: usize) {
        if !source.is_empty() && !target.is_empty() && source != target {
            self.edges
                .push(edge(&self.path, source, target, relation, line));
        }
    }
    fn ensure_type(&mut self, path: &str, line: usize) -> String {
        let id = make_id([self.stem.as_str(), path]);
        self.node(&id, path, line);
        id
    }
    fn walk(
        &mut self,
        node: Node<'tree>,
        owner_path: Option<&str>,
        owner_id: Option<&str>,
    ) -> Result<()> {
        let line = node.start_position().row + 1;
        match node.kind() {
            "preproc_include" => {
                let raw = node
                    .child_by_field_name("file")
                    .map(|n| {
                        if n.kind() == "string_literal" {
                            children(n)
                                .filter(|n| n.kind() == "string_content")
                                .map(|n| read_text(n, self.source))
                                .collect::<String>()
                        } else {
                            read_text(n, self.source).trim_matches(['\'', '"']).into()
                        }
                    })
                    .unwrap_or_default();
                if !raw.is_empty() {
                    let norm = raw.replace('\\', "/");
                    let norm = norm.strip_prefix("./").unwrap_or(&norm);
                    let resolved = fsutil::resolve_missing(
                        &Path::new(&self.path)
                            .parent()
                            .unwrap_or(Path::new("."))
                            .join(norm),
                    )
                    .map_err(crate::Error::from)?;
                    let exists = resolved.exists();
                    let target = if exists {
                        make_id([fsutil::path_string(&resolved).as_str()])
                    } else {
                        make_id([norm])
                    };
                    let mut value = edge(
                        &self.path,
                        &self.file_id,
                        &target,
                        if exists { "imports_from" } else { "imports" },
                        line,
                    );
                    value["context"] = json!("import");
                    if !exists {
                        value["external"] = json!(true);
                    }
                    self.edges.push(value);
                }
                return Ok(());
            }
            "type_definition" => {
                let Some(path) = child(node, "type_path") else {
                    return Ok(());
                };
                let path = trim(&read_text(path, self.source)).to_string();
                let id = self.ensure_type(&path, line);
                self.edge(&self.file_id.clone(), &id, "contains", line);
                if let Some(body) = child(node, "type_body") {
                    for n in children(body) {
                        self.walk(n, Some(&path), Some(&id))?;
                    }
                }
                return Ok(());
            }
            "type_proc_definition" | "type_proc_override" => {
                let (Some(owner), Some(owner_path), Some(name)) =
                    (owner_id, owner_path, node.child_by_field_name("name"))
                else {
                    return Ok(());
                };
                let name = read_text(name, self.source);
                let id = make_id([self.stem.as_str(), owner_path, name.as_str()]);
                self.node(&id, &format!("{owner_path}/{name}()"), line);
                self.edge(owner, &id, "method", line);
                if let Some(body) = child(node, "block") {
                    self.bodies.push((id, body));
                }
                return Ok(());
            }
            "proc_definition" | "proc_override" => {
                let owner_path =
                    child(node, "type_path").map(|n| trim(&read_text(n, self.source)).to_string());
                let owner = owner_path.as_deref().map(|p| self.ensure_type(p, line));
                if let Some(id) = &owner {
                    self.edge(&self.file_id.clone(), id, "contains", line);
                }
                let Some(name) = node.child_by_field_name("name") else {
                    return Ok(());
                };
                let name = read_text(name, self.source);
                let id = if let (Some(path), Some(owner)) = (
                    owner_path.as_deref().filter(|p| !p.is_empty()),
                    owner.as_deref().filter(|p| !p.is_empty()),
                ) {
                    let id = make_id([self.stem.as_str(), path, name.as_str()]);
                    self.node(&id, &format!("{path}/{name}()"), line);
                    self.edge(owner, &id, "method", line);
                    id
                } else {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file_id.clone(), &id, "contains", line);
                    id
                };
                if let Some(body) = child(node, "block") {
                    self.bodies.push((id, body));
                }
                return Ok(());
            }
            "operator_override" | "type_operator_override" => return Ok(()),
            _ => (),
        }
        for n in children(node) {
            self.walk(n, owner_path, owner_id)?;
        }
        Ok(())
    }
    fn emit_call(&mut self, caller: &str, callee: &str, line: usize, member: bool) {
        let target = self
            .labels
            .get(&callee.to_lowercase())
            .filter(|v| v.len() == 1)
            .map(|v| v[0].clone());
        if let Some(target) = target.filter(|id| !id.is_empty() && id != caller) {
            if self.pairs.insert((caller.into(), target.clone())) {
                let mut value = edge(&self.path, caller, &target, "calls", line);
                value["context"] = json!("call");
                self.edges.push(value);
            }
        } else {
            self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn walk_calls(&mut self, node: Node<'tree>, caller: &str) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "proc_definition"
            | "proc_override"
            | "type_proc_definition"
            | "type_proc_override"
            | "type_definition" => return,
            "call_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    if !name.is_empty() && name != ".." {
                        self.emit_call(caller, &name, line, false);
                    }
                }
            }
            "field_proc_expression" => {
                if let Some(name) = node.child_by_field_name("proc") {
                    let name = read_text(name, self.source);
                    if !name.is_empty() {
                        self.emit_call(caller, &name, line, true);
                    }
                }
            }
            "new_expression" => {
                if let Some(typ) = child(node, "type_path") {
                    let text = trim(&read_text(typ, self.source)).to_lowercase();
                    let target = self
                        .paths
                        .get(&text)
                        .filter(|v| v.len() == 1)
                        .map(|v| v[0].clone());
                    if let Some(target) = target.filter(|id| !id.is_empty() && id != caller)
                        && self.pairs.insert((caller.into(), target.clone()))
                    {
                        let mut value = edge(&self.path, caller, &target, "instantiates", line);
                        value["context"] = json!("call");
                        self.edges.push(value);
                    }
                }
            }
            _ => (),
        }
        for n in children(node) {
            self.walk_calls(n, caller);
        }
    }
}
pub fn extract_dm(path: &Path) -> Result<Value> {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return Ok(read_failure(path, &e)),
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&crate::grammar!(
            "grammar-dm",
            "tree_sitter_dm",
            tree_sitter_dm::LANGUAGE
        )?)
        .map_err(crate::Error::from)?;
    let tree = parser
        .parse(&source, None)
        .ok_or("DreamMaker parse cancelled")?;
    let path_text = fsutil::path_string(path);
    let file_id = make_id([path_text.as_str()]);
    let mut ex = Extractor {
        source: &source,
        path: path_text.clone(),
        stem: file_stem(path),
        file_id: file_id.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        labels: HashMap::new(),
        paths: HashMap::new(),
        pairs: HashSet::new(),
        raw_calls: vec![],
    };
    let parts = path_syntax::posix_parts(&path_text);
    ex.node(&file_id, path_syntax::posix_name(&parts), 1);
    ex.walk(tree.root_node(), None, None)?;
    for node in &ex.nodes {
        let label = node["label"]
            .as_str()
            .unwrap_or("")
            .trim_matches(['(', ')']);
        let last = label.rsplit('/').next().unwrap_or("");
        let id = node["id"].as_str().unwrap_or("");
        if !last.is_empty() {
            ex.labels
                .entry(last.to_lowercase())
                .or_default()
                .push(id.into());
        }
        if label.starts_with('/') {
            ex.paths
                .entry(label.to_lowercase())
                .or_default()
                .push(id.into());
        }
    }
    for (caller, body) in ex.bodies.clone() {
        ex.walk_calls(body, &caller);
    }
    Ok(json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls}))
}

pub fn read_dmi_description(data: &[u8]) -> Result<String> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(String::new());
    }
    let mut offset = 8;
    while offset + 8 <= data.len() {
        let len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = &data[offset + 4..offset + 8];
        let payload = &data[offset + 8..(offset + 8 + len).min(data.len())];
        if kind == b"tEXt" || kind == b"zTXt" {
            let Some(null) = payload.iter().position(|b| *b == 0) else {
                return Ok(String::new());
            };
            if &payload[..null] == b"Description" {
                if kind == b"zTXt" {
                    let mut decoder = flate2::Decompress::new(true);
                    let mut output = vec![0; 1024 * 1024];
                    decoder
                        .decompress(
                            payload.get(null + 2..).unwrap_or_default(),
                            &mut output,
                            flate2::FlushDecompress::None,
                        )
                        .map_err(|e| {
                            format!(
                                "Error -3 while decompressing data: {}",
                                e.message().unwrap_or("invalid compressed data")
                            )
                        })?;
                    return Ok(
                        String::from_utf8_lossy(&output[..decoder.total_out() as usize])
                            .into_owned(),
                    );
                }
                return Ok(String::from_utf8_lossy(&payload[null + 1..]).into_owned());
            }
        }
        offset += 8 + len + 4;
    }
    Ok(String::new())
}
pub fn extract_dmi(path: &Path) -> Result<Value> {
    let data = match std::fs::read(path) {
        Ok(s) => s,
        Err(e) => return Ok(read_failure(path, &e)),
    };
    let path_text = fsutil::path_string(path);
    let stem = file_stem(path);
    let file_id = make_id([path_text.as_str()]);
    let mut nodes = vec![file_node(&path_text, &file_id)];
    let mut edges = vec![];
    let mut seen = HashSet::from([file_id.clone()]);
    let description = read_dmi_description(&data)?;
    for (line, text) in fsutil::split_lines(&description).enumerate() {
        let text = trim(text);
        if !text.starts_with("state =") {
            continue;
        }
        let value = trim(text.split_once('=').unwrap().1);
        let state = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if state.is_empty() {
            continue;
        }
        let id = make_id([stem.as_str(), "state", state]);
        if !seen.insert(id.clone()) {
            continue;
        }
        nodes.push(json!({"id":id,"label":format!("\"{state}\""),"file_type":"code","source_file":path_text,"source_location":format!("L{}",line+1)}));
        edges.push(edge(&path_text, &file_id, &id, "contains", line + 1));
    }
    Ok(json!({"nodes":nodes,"edges":edges}))
}
pub fn split_dmm_tile(body: &str) -> Vec<String> {
    let mut out = vec![];
    let mut buf = String::new();
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escape = false;
    for c in body.chars() {
        if escape {
            buf.push(c);
            escape = false;
            continue;
        }
        if in_string {
            buf.push(c);
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                buf.push(c);
            }
            '(' | '{' | '[' => {
                depth += 1;
                buf.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                buf.push(c);
            }
            ',' if depth == 0 => {
                out.push(trim(&buf).into());
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    if !trim(&buf).is_empty() {
        out.push(trim(&buf).into());
    }
    out
}
pub fn dmm_type_path(entry: &str) -> &str {
    trim(entry.split_once('{').map(|(a, _)| a).unwrap_or(entry))
}
static GRID: LazyLock<regex::Regex> = LazyLock::new(|| {
    let digits = format!(
        "[{}]",
        crate::decimal_data::DECIMAL
            .iter()
            .map(|(a, b)| format!("\\u{{{a:x}}}-\\u{{{b:x}}}"))
            .collect::<String>()
    );
    regex_compat::compile(&r"(?m)^\(\s*\d+\s*,\s*\d+\s*,\s*\d+\s*\)\s*=".replace(r"\d", &digits))
});
pub fn extract_dmm(path: &Path) -> Value {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > 50 * 1024 * 1024 => {
            return json!({"nodes":[],"edges":[],"error":"file too large (>50 MB)"});
        }
        Err(e) => return read_failure(path, &e),
        _ => (),
    }
    let text = match super::base::read_source_text(path, true) {
        Ok(s) => s,
        Err(e) => return read_failure(path, &e),
    };
    let path_text = fsutil::path_string(path);
    let file_id = make_id([path_text.as_str()]);
    let nodes = vec![file_node(&path_text, &file_id)];
    let mut edges = vec![];
    let dictionary = GRID
        .find(&text)
        .map(|m| &text[..m.start()])
        .unwrap_or(&text);
    let mut seen = HashSet::new();
    let mut buf = String::new();
    let mut open_line = 0;
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escape = false;
    for (line, text) in fsutil::split_lines(dictionary).enumerate() {
        for c in text.chars() {
            if escape {
                escape = false;
            } else if in_string {
                if c == '\\' {
                    escape = true;
                } else if c == '"' {
                    in_string = false;
                }
            } else if c == '"' {
                in_string = true;
            } else if c == '(' {
                if depth == 0 {
                    open_line = line + 1;
                }
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
            buf.push(c);
        }
        buf.push('\n');
        if depth == 0 && !buf.is_empty() {
            let chunk = std::mem::take(&mut buf);
            let (Some(left), Some(right)) = (chunk.find('('), chunk.rfind(')')) else {
                continue;
            };
            if right <= left {
                continue;
            }
            for entry in split_dmm_tile(&chunk[left + 1..right]) {
                let path = dmm_type_path(&entry);
                if !path.starts_with('/') {
                    continue;
                }
                let target = make_id([path]);
                if !seen.insert(target.clone()) {
                    continue;
                }
                let mut value = edge(&path_text, &file_id, &target, "uses", open_line);
                value["context"] = json!("map");
                edges.push(value);
            }
        }
    }
    json!({"nodes":nodes,"edges":edges})
}
static WINDOW: LazyLock<regex::Regex> =
    LazyLock::new(|| regex_compat::compile(r#"^\s*window\s+"([^"]+)"\s*$"#));
static ELEM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex_compat::compile(r#"^\s*elem\s+"([^"]+)"\s*$"#));
static TYPE: LazyLock<regex::Regex> = LazyLock::new(|| {
    let space = crate::fsutil::PY_WHITESPACE;
    let nonspace = format!("[^{}]", &space[1..space.len() - 1]);
    regex_compat::compile(&r"^\s*type\s*=\s*(\S+)\s*$".replace(r"\S", &nonspace))
});
pub fn extract_dmf(path: &Path) -> Value {
    let text = match super::base::read_source_text(path, true) {
        Ok(s) => s,
        Err(e) => return read_failure(path, &e),
    };
    let path_text = fsutil::path_string(path);
    let file_id = make_id([path_text.as_str()]);
    let stem = file_stem(path);
    let mut nodes = vec![file_node(&path_text, &file_id)];
    let mut edges = vec![];
    let mut seen = HashSet::from([file_id.clone()]);
    // The current window's node id and name.
    let mut window: Option<(String, String)> = None;
    let mut elem: Option<(String, String)> = None;
    for (line, text) in fsutil::split_lines(&text).enumerate() {
        if let Some(m) = WINDOW.captures(text) {
            let name = &m[1];
            let id = make_id([stem.as_str(), "window", name]);
            if seen.insert(id.clone()) {
                nodes.push(json!({"id":id,"label":format!("window \"{name}\""),"file_type":"code","source_file":path_text,"source_location":format!("L{}",line+1)}));
                edges.push(edge(&path_text, &file_id, &id, "contains", line + 1));
            }
            window = Some((id, name.to_string()));
            elem = None;
            continue;
        }
        if let (Some(m), Some((window, window_name))) = (ELEM.captures(text), window.as_ref()) {
            let name = &m[1];
            // Keyed on the window's name, not its id: the id embeds the stem,
            // which is absolute at extraction time, and the id remap only
            // rewrites a leading stem (#4153).
            let id = make_id([stem.as_str(), "elem", window_name.as_str(), name]);
            if seen.insert(id.clone()) {
                nodes.push(json!({"id":id,"label":format!("elem \"{name}\""),"file_type":"code","source_file":path_text,"source_location":format!("L{}",line+1)}));
                edges.push(edge(&path_text, window, &id, "contains", line + 1));
            }
            elem = Some((id, name.into()));
            continue;
        }
        if let (Some(m), Some((id, name))) = (TYPE.captures(text), elem.as_ref())
            && let Some(node) = nodes
                .iter_mut()
                .find(|n| n["id"] == *id && !n["label"].as_str().unwrap_or("").contains(" ["))
        {
            node["label"] = json!(format!("elem \"{name}\" [{}]", &m[1]));
        }
    }
    json!({"nodes":nodes,"edges":edges})
}

pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "dm.description" => {
            let bytes: Vec<u8> =
                serde_json::from_value(request["data"].clone()).map_err(crate::Error::from)?;
            Ok(json!(read_dmi_description(&bytes)?))
        }
        "dm.split_tile" => Ok(json!(split_dmm_tile(
            request["text"].as_str().ok_or("Expected tile text")?
        ))),
        "dm.type_path" => Ok(json!(dmm_type_path(
            request["text"].as_str().ok_or("Expected type path")?
        ))),
        _ => Err("Unknown DreamMaker helper".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-dm")]
    fn grammar_matches_the_pinned_python_package() {
        let language: tree_sitter::Language = tree_sitter_dm::LANGUAGE.into();
        assert_eq!(language.node_kind_count(), 264);
        assert_eq!(language.parse_state_count(), 1805);
        assert_eq!(language.abi_version(), 15);
    }
}
