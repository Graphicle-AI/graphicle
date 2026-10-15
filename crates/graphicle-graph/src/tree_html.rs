// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/tree_html.py
//! Port of graphify/tree_html.py. Its browser code is retained verbatim in
//! assets/tree.html.template; hierarchy construction and rendering are native.
use crate::{Result, build_labels, fsutil, path_syntax, python_compat, security};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};
pub const DEFAULT_MAX_CHILDREN: i64 = 200;
pub fn common_root(paths: &[String]) -> String {
    let mut parts = paths
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| path_syntax::posix_parts(p));
    let Some(mut prefix) = parts.next() else {
        return String::new();
    };
    for p in parts {
        let n = prefix.iter().zip(&p).take_while(|(a, b)| a == b).count();
        prefix.truncate(n);
    }
    if prefix.is_empty() {
        String::new()
    } else {
        path_syntax::posix_join(&prefix)
    }
}
pub fn truncation_leaf(extra: i64) -> Value {
    json!({"name":format!("(+{extra} more)"),"total_count":extra,"children":[]})
}
#[derive(Clone)]
struct TreeNode {
    name: String,
    total: i64,
    children: Vec<usize>,
}
fn push(nodes: &mut Vec<TreeNode>, name: String, total: i64, children: Vec<usize>) -> usize {
    let i = nodes.len();
    nodes.push(TreeNode {
        name,
        total,
        children,
    });
    i
}
fn parent(parts: &[String]) -> Vec<String> {
    let mut p = parts.to_vec();
    if !path_syntax::posix_name(&p).is_empty() {
        p.pop();
    }
    p
}
fn ensure_dir(
    parts: &[String],
    index: &mut HashMap<String, usize>,
    nodes: &mut Vec<TreeNode>,
) -> usize {
    let key = path_syntax::posix_join(parts);
    if let Some(i) = index.get(&key) {
        return *i;
    }
    let up = parent(parts);
    if up == parts {
        return 0;
    }
    let parent = ensure_dir(&up, index, nodes);
    let i = push(nodes, path_syntax::posix_name(parts).into(), 0, vec![]);
    index.insert(key, i);
    nodes[parent].children.push(i);
    i
}
fn finish(i: usize, nodes: &mut [TreeNode]) -> i64 {
    let mut kids = std::mem::take(&mut nodes[i].children);
    kids.sort_by(|a, b| {
        nodes[*a]
            .children
            .is_empty()
            .cmp(&nodes[*b].children.is_empty())
            .then_with(|| {
                nodes[*a]
                    .name
                    .to_lowercase()
                    .cmp(&nodes[*b].name.to_lowercase())
            })
    });
    if kids.is_empty() {
        return if nodes[i].total == 0 {
            1
        } else {
            nodes[i].total
        };
    }
    let n = kids.iter().map(|c| finish(*c, nodes)).sum::<i64>();
    nodes[i].total = if n == 0 { 1 } else { n };
    nodes[i].children = kids;
    nodes[i].total
}
fn value(i: usize, nodes: &[TreeNode]) -> Value {
    json!({"name":nodes[i].name,"total_count":nodes[i].total,"children":nodes[i].children.iter().map(|c|value(*c,nodes)).collect::<Vec<_>>()})
}
pub fn build_tree(
    graph: &Value,
    root: Option<&str>,
    max_children: i64,
    project_label: Option<&str>,
) -> Result<Value> {
    let empty = vec![];
    let nodes = graph
        .get("nodes")
        .map(|v| v.as_array().ok_or("TypeError: graph nodes"))
        .transpose()?
        .unwrap_or(&empty);
    let mut files: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut paths = Vec::new();
    for n in nodes {
        if let Some(src) = n
            .get("source_file")
            .filter(|v| !v.is_null() && **v != json!("") && **v != json!(false))
        {
            let src = src
                .as_str()
                .ok_or("TypeError: source_file must be a path")?;
            files.entry(src.into()).or_default().push(n);
            paths.push(src.to_string());
        }
    }
    if files.is_empty() {
        return Ok(json!({"name":"(empty graph)","total_count":0,"children":[]}));
    }
    let inferred = common_root(&paths);
    let explicit = root.is_some();
    let root = root.unwrap_or(&inferred);
    let root_parts = path_syntax::posix_parts(root);
    let root_name = path_syntax::posix_name(&root_parts);
    let label = project_label
        .filter(|s| !s.is_empty())
        .unwrap_or(if !root_name.is_empty() {
            root_name
        } else if !root.is_empty() {
            root
        } else {
            "/"
        });
    let mut arena = vec![TreeNode {
        name: label.into(),
        total: 0,
        children: vec![],
    }];
    let mut index = HashMap::from([(path_syntax::posix_join(&root_parts), 0)]);
    let mut matched = 0;
    for (src, syms) in &files {
        let parts = path_syntax::posix_parts(src);
        let absolute = |p: &[String]| p.first().is_some_and(|s| s == "/" || s == "//");
        let parent = if absolute(&parts) == absolute(&root_parts) && parts.starts_with(&root_parts)
        {
            matched += 1;
            parent(&parts)
        } else {
            root_parts.clone()
        };
        let p = ensure_dir(&parent, &mut index, &mut arena);
        let mut children = Vec::new();
        for n in syms {
            let fallback = json!("?");
            let label = n.get("label").or_else(|| n.get("id")).unwrap_or(&fallback);
            if n["file_type"] == "code" && build_labels::is_file_node_label(label, &json!(src)) {
                continue;
            }
            let label = label
                .as_str()
                .ok_or("TypeError: symbol label must be a string")?;
            children.push(push(&mut arena, label.into(), 1, vec![]));
        }
        children.sort_by(|a, b| {
            arena[*a]
                .name
                .starts_with('_')
                .cmp(&arena[*b].name.starts_with('_'))
                .then_with(|| {
                    arena[*a]
                        .name
                        .to_lowercase()
                        .cmp(&arena[*b].name.to_lowercase())
                })
        });
        if children.len() as i64 > max_children {
            let extra = children.len() as i64 - max_children;
            let n = if max_children < 0 {
                children
                    .len()
                    .saturating_sub(max_children.unsigned_abs() as usize)
            } else {
                max_children as usize
            };
            children.truncate(n);
            children.push(push(&mut arena, format!("(+{extra} more)"), extra, vec![]));
        }
        let i = push(
            &mut arena,
            path_syntax::posix_name(&parts).into(),
            children.len().max(1) as i64,
            children,
        );
        arena[p].children.push(i);
    }
    if explicit && matched == 0 {
        return Err(format!(
            "--root {root} matched 0 of {} source files (source_file paths are stored repo-relative, e.g. {})",
            files.len(),
            python_compat::repr(&json!(files.keys().next().unwrap()))
        ).into());
    }
    finish(0, &mut arena);
    Ok(value(0, &arena))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
fn compact_json(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let raw = serde_json::to_string(s).unwrap();
            let mut out = String::new();
            for c in raw.chars() {
                let n = c as u32;
                if n < 127 {
                    out.push(c);
                } else if n <= 65535 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 65536;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 1023)
                    ));
                }
            }
            out
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(compact_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}:{}", compact_json(&json!(k)), compact_json(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Number(_) => python_compat::string(v),
        Value::Bool(v) => v.to_string(),
        Value::Null => "null".into(),
    }
}
pub fn emit_html(tree: &Value, title: &str, header: &str, width: i64, height: i64) -> String {
    let fields = HashMap::from([
        ("title", escape(title)),
        ("header", escape(header)),
        ("svg_width", width.to_string()),
        ("svg_height", height.to_string()),
        // Every `<` escaped, not just `</`: an unclosed `<!--` followed by a
        // `<script` in a label would otherwise keep the real `</script>` from
        // closing the element (#4124).
        ("data_json", compact_json(tree).replace('<', "\\u003c")),
    ]);
    let template = include_str!("assets/tree.html.template");
    let mut out = String::with_capacity(template.len() + fields["data_json"].len());
    let mut remaining = template;
    while let Some(i) = remaining.find(['{', '}']) {
        out.push_str(&remaining[..i]);
        remaining = &remaining[i..];
        if remaining.starts_with("{{") || remaining.starts_with("}}") {
            out.push(remaining.as_bytes()[0] as char);
            remaining = &remaining[2..];
        } else if let Some(end) = remaining.find('}') {
            let name = &remaining[1..end];
            out.push_str(&fields[name]);
            remaining = &remaining[end + 1..];
        } else {
            unreachable!("verified static template")
        }
    }
    out.push_str(remaining);
    out
}
pub fn write_tree_html(
    graph_path: &Path,
    output: &Path,
    root: Option<&str>,
    max_children: i64,
    label: Option<&str>,
) -> Result<String> {
    security::check_graph_file_size_cap(graph_path)?;
    let text = std::fs::read_to_string(graph_path)
        .map_err(|e| crate::io_compat::read_error(&e, graph_path))?;
    let graph: Value = serde_json::from_str(&text).map_err(crate::Error::from)?;
    let tree = build_tree(&graph, root, max_children, label)?;
    let name = tree["name"].as_str().ok_or("Invalid tree name")?;
    let html = emit_html(
        &tree,
        &format!("{name} — Graphicle tree viewer"),
        &format!("{name} — Knowledge Graph"),
        6000,
        8000,
    );
    if let Some(p) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p).map_err(crate::Error::from)?;
    }
    std::fs::write(output, html).map_err(crate::Error::from)?;
    Ok(fsutil::path_string(output))
}
pub fn dispatch(r: &Value) -> Result<Value> {
    match r["operation"].as_str().unwrap_or("") {
        "tree.common_root" => {
            let paths: Vec<String> =
                serde_json::from_value(r["paths"].clone()).map_err(|_| "Expected paths")?;
            Ok(json!(common_root(&paths)))
        }
        "tree.truncation" => Ok(truncation_leaf(
            r["extra"].as_i64().ok_or("Expected extra")?,
        )),
        "tree.build" => build_tree(
            &r["graph"],
            r["root"].as_str(),
            r["max_children"].as_i64().unwrap_or(DEFAULT_MAX_CHILDREN),
            r["project_label"].as_str(),
        ),
        "tree.emit" => Ok(json!(emit_html(
            &r["tree"],
            r["title"].as_str().ok_or("Expected title")?,
            r["header"].as_str().ok_or("Expected header")?,
            r["svg_width"].as_i64().unwrap_or(6000),
            r["svg_height"].as_i64().unwrap_or(8000)
        ))),
        "tree.write" => Ok(json!(write_tree_html(
            Path::new(r["graph_path"].as_str().ok_or("Expected graph_path")?),
            Path::new(r["output_path"].as_str().ok_or("Expected output_path")?),
            r["root"].as_str(),
            r["max_children"].as_i64().unwrap_or(DEFAULT_MAX_CHILDREN),
            r["project_label"].as_str()
        )?)),
        _ => Err("Unknown tree operation".into()),
    }
}
