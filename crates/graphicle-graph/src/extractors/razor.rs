// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/razor.py
//! Port of graphify/extractors/razor.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use super::base::{file_stem, make_id};
use crate::{fsutil, regex_compat::compile, security::sanitize_metadata};
use regex::Regex;
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::LazyLock};

static USING: LazyLock<Regex> = LazyLock::new(|| compile(r"^@using\s+(.+)"));
static INJECT: LazyLock<Regex> = LazyLock::new(|| compile(r"^@inject\s+([\w.<>\[\]]+)\s+(\w+)"));
static INHERITS: LazyLock<Regex> = LazyLock::new(|| compile(r"^@inherits\s+([\w.<>\[\]]+)"));
static MODEL: LazyLock<Regex> = LazyLock::new(|| compile(r"^@model\s+([\w.<>\[\]]+)"));
static PAGE: LazyLock<Regex> = LazyLock::new(|| compile(r#"^@page\s+"([^"]+)""#));
static COMPONENT: LazyLock<Regex> = LazyLock::new(|| compile(r"<([A-Z][A-Za-z0-9]+)[\s/>]"));
// @code is the Blazor (.razor) spelling; @functions is the Razor Pages / MVC
// (.cshtml) spelling of the same member block.
static CODE: LazyLock<Regex> = LazyLock::new(|| compile(r"@(?:code|functions)\s*\{"));
static METHOD: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?:public|private|protected|internal|static|async|override|virtual|abstract)\s+[\w<>\[\],\s]+\s+(\w+)\s*\(",
    )
});
const HTML_TAGS: &[&str] = &[
    "DOCTYPE", "Html", "Head", "Body", "Div", "Span", "Table", "Form", "Input", "Button", "Select",
    "Option", "Label", "Textarea", "Script", "Style", "Link", "Meta", "Title", "Header", "Footer",
    "Nav", "Main", "Section", "Article", "Aside",
];

fn trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

struct Output {
    file_nid: String,
    path: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
}
impl Output {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if !id.is_empty() && self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn reference(&mut self, name: &str, relation: &str, line: usize) {
        let id = make_id([name]);
        if id.is_empty() {
            return;
        }
        self.node(&id, name, line);
        self.edges.push(
            json!({"source":self.file_nid,"target":id,"relation":relation,"confidence":"EXTRACTED",
            "source_file":self.path,"source_location":format!("L{line}"),"weight":1.0}),
        );
    }
}

pub fn extract_razor(path: &Path) -> Value {
    let str_path = fsutil::path_string(path);
    let Ok(src) = super::base::read_source_text(path, true) else {
        return json!({"nodes":[],"edges":[],"error":format!("cannot read {str_path}")});
    };
    let file_nid = make_id([str_path.as_str()]);
    let parts = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&parts);
    let nodes = vec![
        json!({"id":file_nid,"label":name,"file_type":"code","source_file":str_path,"source_location":null}),
    ];
    let mut out = Output {
        seen: HashSet::from([file_nid.clone()]),
        file_nid,
        path: str_path,
        nodes,
        edges: Vec::new(),
    };
    for (index, line) in fsutil::split_lines(&src).enumerate() {
        let line_number = index + 1;
        if let Some(captures) = USING.captures(line) {
            let raw = trim(&captures[1]).trim_end_matches(';');
            let (kind, target, alias) = if let Some(target) = raw.strip_prefix("static ") {
                ("static", trim(target), None)
            } else if let Some((alias, target)) = raw.split_once('=') {
                ("alias", trim(target), Some(trim(alias)))
            } else {
                ("namespace", raw, None)
            };
            if !target.is_empty() {
                let id = make_id([target]);
                out.node(&id, target, line_number);
                let mut metadata =
                    json!({"using_kind":kind,"target_fqn":target,"scope_kind":"file"});
                if let Some(alias) = alias.filter(|alias| !alias.is_empty()) {
                    metadata["alias"] = json!(alias);
                }
                out.edges.push(json!({"source":out.file_nid,"target":id,"relation":"imports","context":"import","confidence":"EXTRACTED",
                    "source_file":out.path,"source_location":format!("L{line_number}"),"weight":1.0,"metadata":sanitize_metadata(Some(&metadata))}));
            }
            continue;
        }
        if let Some(captures) = INJECT.captures(line) {
            let raw = trim(&captures[1]);
            let base = trim(raw.split('<').next().unwrap_or(""));
            let (prefix, token, qualified) = base
                .rsplit_once('.')
                .map(|(a, b)| (a, b, true))
                .unwrap_or(("", base, false));
            if !token.is_empty() {
                let id = make_id([token]);
                if !id.is_empty() && out.seen.insert(id.clone()) {
                    out.nodes.push(json!({"id":id,"label":token,"file_type":"code","source_file":"","source_location":"","origin_file":out.path}));
                }
                let mut metadata = json!({"ref_token":token});
                if qualified {
                    metadata["qualified"] = json!(true);
                    if !prefix.is_empty() {
                        metadata["ref_qualifier"] = json!(prefix);
                    }
                }
                out.edges.push(json!({"source":out.file_nid,"target":id,"relation":"references","context":"field","confidence":"EXTRACTED",
                    "source_file":out.path,"source_location":format!("L{line_number}"),"weight":1.0,"metadata":sanitize_metadata(Some(&metadata))}));
            }
            continue;
        }
        if let Some(captures) = INHERITS.captures(line) {
            out.reference(&captures[1], "inherits", line_number);
            continue;
        }
        if let Some(captures) = MODEL.captures(line) {
            out.reference(&captures[1], "references", line_number);
            continue;
        }
        if let Some(captures) = PAGE.captures(line) {
            let route = &captures[1];
            let id = make_id(["route", route]);
            if !id.is_empty() && out.seen.insert(id.clone()) {
                out.nodes.push(json!({"id":id,"label":format!("route:{route}"),"file_type":"concept","source_file":out.path,"source_location":format!("L{line_number}")}));
                out.edges.push(json!({"source":out.file_nid,"target":id,"relation":"references","confidence":"EXTRACTED","source_file":out.path,"weight":1.0}));
            }
        }
    }
    for captures in COMPONENT.captures_iter(&src) {
        let component = &captures[1];
        if HTML_TAGS.contains(&component) {
            continue;
        }
        let line = src[..captures.get(0).unwrap().start()]
            .bytes()
            .filter(|c| *c == b'\n')
            .count()
            + 1;
        out.reference(component, "calls", line);
    }
    for block in CODE.find_iter(&src) {
        let mut depth = 1usize;
        let mut end = block.end();
        while end < src.len() && depth > 0 {
            match src.as_bytes()[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        let code = if depth == 0 {
            &src[block.end()..end - 1]
        } else {
            ""
        };
        for captures in METHOD.captures_iter(code) {
            let name = &captures[1];
            let position = block.end() + captures.get(0).unwrap().start();
            let line = src[..position].bytes().filter(|c| *c == b'\n').count() + 1;
            let stem = file_stem(path);
            let id = make_id([stem.as_str(), name]);
            if !id.is_empty() && !out.seen.contains(&id) {
                out.node(&id, name, line);
                out.edges.push(json!({"source":out.file_nid,"target":id,"relation":"contains","confidence":"EXTRACTED","source_file":out.path,"weight":1.0}));
            }
        }
    }
    json!({"nodes":out.nodes,"edges":out.edges})
}
