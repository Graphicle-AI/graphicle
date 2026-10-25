// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/pascal_forms.py
//! Port of graphify/extractors/pascal_forms.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use super::base::{file_stem, make_id};
use crate::{fsutil, io_compat, regex_compat::compile};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

static OBJECT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^\s*[oO][bB][jJ][eE][cC][tT]\s+\w+\s*:\s*(\w+)"));
static EVENT: LazyLock<Regex> = LazyLock::new(|| compile(r"^\s*[oO][nN]\w+\s*=\s*(\w+)"));
static END: LazyLock<Regex> = LazyLock::new(|| compile(r"^\s*[eE][nN][dD]\s*$"));

struct Output {
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    node_ids: HashSet<String>,
    edge_keys: HashSet<(String, String, String)>,
}

impl Output {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.node_ids.insert(id.to_owned()) {
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
        if !self
            .edge_keys
            .insert((source.into(), target.into(), relation.into()))
        {
            return;
        }
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED",
            "source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(context) = context {
            edge["context"] = json!(context);
        }
        self.edges.push(edge);
    }
}

fn parse(path: &Path, text: &str) -> Value {
    let str_path = fsutil::path_string(path);
    let stem = file_stem(path);
    let file_nid = make_id([str_path.as_str()]);
    let name_parts = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&name_parts);
    let mut out = Output {
        path: str_path,
        nodes: Vec::new(),
        edges: Vec::new(),
        node_ids: HashSet::new(),
        edge_keys: HashSet::new(),
    };
    out.node(&file_nid, name, 1);
    let mut stack = vec![file_nid];
    for (index, line) in fsutil::split_lines(text).enumerate() {
        let line_number = index + 1;
        if let Some(captures) = OBJECT.captures(line) {
            let class_name = &captures[1];
            let id = make_id([stem.as_str(), class_name]);
            out.node(&id, class_name, line_number);
            out.edge(stack.last().unwrap(), &id, "contains", line_number, None);
            stack.push(id);
            continue;
        }
        if let Some(captures) = EVENT.captures(line)
            && stack.len() > 1
        {
            let handler = &captures[1];
            let id = make_id([stem.as_str(), handler]);
            out.node(&id, &format!("{handler}()"), line_number);
            out.edge(
                stack.last().unwrap(),
                &id,
                "references",
                line_number,
                Some("event"),
            );
            continue;
        }
        if END.is_match(line) && stack.len() > 1 {
            stack.pop();
        }
    }
    json!({"nodes":out.nodes,"edges":out.edges,"input_tokens":0,"output_tokens":0})
}

pub fn extract_lazarus_form(path: &Path) -> Value {
    match super::base::read_source_text(path, true) {
        Ok(text) => parse(path, &text),
        Err(error) => json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)}),
    }
}

pub fn extract_delphi_form(path: &Path) -> Value {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&error,path)});
        }
    };
    if bytes.starts_with(&[0xff, 0x0a]) {
        let parts = crate::path_syntax::posix_parts(&fsutil::path_string(path));
        let name = crate::path_syntax::posix_name(&parts);
        return json!({"nodes":[],"edges":[],"error":format!("binary DFM (convert to text in Delphi IDE to index): {name}")});
    }
    parse(path, &String::from_utf8_lossy(&bytes))
}
