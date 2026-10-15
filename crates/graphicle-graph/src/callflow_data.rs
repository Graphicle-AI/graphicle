// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/callflow_html.py
//! Data normalization, display helpers and section selection from
//! graphify/callflow_html.py. Static styles are retained as source assets.
use crate::{Result, fsutil, path_syntax, paths, python_compat, security};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub static ASSETS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/callflow/templates.json"))
        .expect("checked callflow assets")
});
pub fn render(key: &str, values: &[String]) -> String {
    ASSETS["templates"][key]
        .as_array()
        .expect("known template")
        .iter()
        .map(|v| match v {
            Value::String(s) => s.as_str(),
            _ => values[v.as_u64().unwrap() as usize].as_str(),
        })
        .collect()
}
pub fn truth(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::String(s) => !s.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
pub fn s(v: &Value) -> String {
    python_compat::string(v)
}
pub fn get(v: &Value, key: &str, default: &str) -> String {
    v.get(key).map(s).unwrap_or_else(|| default.into())
}
pub fn text(v: &Value) -> String {
    if truth(v) { s(v) } else { String::new() }
}
pub fn ar(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn first(v: &Value, keys: &[&str], default: Value) -> Value {
    keys.iter()
        .filter_map(|k| v.get(*k))
        .find(|v| !v.is_null() && **v != json!(""))
        .cloned()
        .unwrap_or(default)
}
pub fn first_list(values: &[&Value]) -> Vec<Value> {
    values
        .iter()
        .find_map(|v| v.as_array().cloned())
        .unwrap_or_default()
}
pub fn float(v: &Value, default: f64) -> f64 {
    match v {
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::String(s) => s.trim().parse().unwrap_or(default),
        _ => v.as_f64().unwrap_or(default),
    }
}
pub fn endpoint(v: &Value) -> String {
    text(&if v.is_object() {
        first(
            v,
            &["id", "node_id", "key", "name", "qualified_name"],
            Value::Null,
        )
    } else {
        v.clone()
    })
}
pub fn normalize_node(raw: &Value, index: usize) -> Value {
    let mut n = raw.clone();
    let id = first(
        &n,
        &[
            "id",
            "node_id",
            "key",
            "uid",
            "name",
            "qualified_name",
            "fqname",
            "symbol",
        ],
        json!(format!("node_{}", index + 1)),
    );
    let src = first(
        &n,
        &[
            "source_file",
            "file",
            "file_path",
            "filepath",
            "path",
            "module_path",
            "defined_in",
        ],
        json!(""),
    );
    let label = first(
        &n,
        &[
            "label",
            "display_name",
            "title",
            "name",
            "qualified_name",
            "fqname",
            "symbol",
        ],
        id.clone(),
    );
    let cid = first(
        &n,
        &[
            "community",
            "community_id",
            "cluster",
            "cluster_id",
            "group",
            "group_id",
            "modularity_class",
        ],
        json!("unknown"),
    );
    let kind = first(&n, &["node_type", "kind", "type", "category"], json!(""));
    let mut ft = first(
        &n,
        &["file_type", "content_type", "artifact_type"],
        json!(""),
    );
    if !truth(&ft) {
        ft = json!(if [".md", ".mdx", ".rst", ".txt"].contains(
            &path_syntax::suffix(&basename(&s(&src)))
                .to_lowercase()
                .as_str()
        ) {
            "document"
        } else {
            "code"
        });
    }
    n["id"] = json!(s(&id));
    n["label"] = json!(s(&label));
    n["community"] = cid;
    n["source_file"] = json!(text(&src));
    n["node_type"] = json!(text(&kind));
    n["file_type"] = json!(if truth(&ft) { s(&ft) } else { "code".into() });
    n
}
pub fn normalize_edge(raw: &Value, index: usize) -> Option<Value> {
    let mut e = raw.clone();
    let src = endpoint(&first(
        &e,
        &["source", "src", "from", "from_id", "start", "u"],
        Value::Null,
    ));
    let tgt = endpoint(&first(
        &e,
        &["target", "dst", "to", "to_id", "end", "v"],
        Value::Null,
    ));
    if src.is_empty() || tgt.is_empty() {
        return None;
    }
    let rel = first(
        &e,
        &["relation", "type", "kind", "label", "predicate"],
        json!("relates"),
    );
    let confidence = first(
        &e,
        &["confidence", "evidence", "provenance"],
        json!("EXTRACTED"),
    );
    let score = first(
        &e,
        &["confidence_score", "score", "weight", "probability"],
        json!(1.0),
    );
    e["id"] = json!(s(&first(
        &e,
        &["id", "edge_id"],
        json!(format!("edge_{}", index + 1))
    )));
    e["source"] = json!(src);
    e["target"] = json!(tgt);
    e["relation"] = json!(if truth(&rel) {
        s(&rel).to_lowercase()
    } else {
        "relates".into()
    });
    e["confidence"] = json!(if truth(&confidence) {
        s(&confidence).to_uppercase()
    } else {
        "EXTRACTED".into()
    });
    e["confidence_score"] = json!(float(&score, 1.0));
    Some(e)
}
pub fn node_link(data: &Value) -> Option<(Vec<Value>, Vec<Value>)> {
    if !data["nodes"].is_array() || (!data["links"].is_array() && !data["edges"].is_array()) {
        return None;
    }
    let mut d = data.clone();
    d["directed"] = json!(true);
    d["multigraph"] = json!(true);
    let g = paths::load_node_link_graph(&d).ok()?;
    let nodes = g
        .nodes
        .iter()
        .map(|n| {
            let mut v = Value::Object(n.attributes.to_map());
            v["id"] = n.id.clone();
            v
        })
        .collect();
    let edges = g
        .ordered_edges()
        .iter()
        .enumerate()
        .map(|(i, (u, v, e))| {
            let mut x = Value::Object(e.attributes.to_map());
            x["source"] = x
                .get("_src")
                .or_else(|| x.get("source"))
                .cloned()
                .unwrap_or(g.nodes[*u].id.clone());
            x["target"] = x
                .get("_tgt")
                .or_else(|| x.get("target"))
                .cloned()
                .unwrap_or(g.nodes[*v].id.clone());
            if x.get("id").is_none() {
                x["id"] = json!(format!("edge_{}", i + 1));
            }
            x
        })
        .collect();
    Some((nodes, edges))
}
pub fn read_json(path: Option<&str>, default: Value) -> Result<Value> {
    let Some(p) = path
        .filter(|s| !s.is_empty())
        .map(Path::new)
        .filter(|p| p.exists())
    else {
        return Ok(default);
    };
    let t = std::fs::read_to_string(p).map_err(|e| crate::io_compat::read_error(&e, p))?;
    serde_json::from_str(&t)
        .map_err(|e| format!("ERROR: invalid JSON in {}: {e}", p.display()))
        .map_err(crate::Error::from)
}
pub fn load_data(data: &Value, path: &str) -> Result<Value> {
    if !data.is_object() {
        return Err(format!("ERROR: graph file must contain a JSON object: {path}").into());
    }
    let empty = json!({});
    let gb = data
        .get("graph")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    let mb = data
        .get("metadata")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    let (nodes, edges) = node_link(data).unwrap_or_else(|| {
        (
            first_list(&[
                &data["nodes"],
                &data["vertices"],
                &gb["nodes"],
                &gb["vertices"],
            ]),
            first_list(&[&data["links"], &data["edges"], &gb["links"], &gb["edges"]]),
        )
    });
    let nodes = nodes
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_object())
        .map(|(i, n)| normalize_node(n, i))
        .collect::<Vec<_>>();
    let edges = edges
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_object())
        .filter_map(|(i, e)| normalize_edge(e, i))
        .collect::<Vec<_>>();
    let hyper = first_list(&[
        &data["hyperedges"],
        &gb["hyperedges"],
        &data["groups"],
        &gb["groups"],
    ]);
    let mut meta = gb.as_object().unwrap().clone();
    meta.extend(mb.as_object().unwrap().clone());
    for k in [
        "built_at_commit",
        "commit",
        "project_name",
        "repo",
        "repository",
        "language_breakdown",
    ] {
        if truth(&data[k]) && !meta.get(k).is_some_and(truth) {
            meta.insert(k.into(), data[k].clone());
        }
    }
    if meta.get("commit").is_some_and(truth) && !meta.get("built_at_commit").is_some_and(truth) {
        meta.insert("built_at_commit".into(), meta["commit"].clone());
    }
    Ok(json!([nodes, edges, hyper, meta]))
}
pub fn load_graph(path: &str, cap: Option<u64>) -> Result<Value> {
    if !path.is_empty() {
        if let Some(cap) = cap {
            security::check_graph_file_size_cap_with(Path::new(path), cap)
        } else {
            security::check_graph_file_size_cap(Path::new(path))
        }
        .map_err(|e| format!("ERROR: {e}"))?;
    }
    load_data(&read_json(Some(path), Value::Null)?, path)
}
pub fn labels_data(mut d: Value) -> Value {
    if !d.is_object() {
        return json!({});
    }
    if d["labels"].is_object() {
        d = d["labels"].clone()
    }
    if d["communities"].is_object() {
        d = d["communities"].clone()
    }
    Value::Object(
        d.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    json!(s(&if v.is_object() {
                        first(v, &["label", "name", "title"], json!(k))
                    } else {
                        v.clone()
                    })),
                )
            })
            .collect(),
    )
}
pub fn load_sections(path: Option<&str>) -> Result<Value> {
    let mut d = read_json(path, json!([]))?;
    if d.is_object() && d["sections"].is_array() {
        d = d["sections"].clone()
    }
    if !d.is_array() {
        return Err(format!(
            "ERROR: sections file must contain a JSON array: {}",
            path.unwrap_or("None")
        )
        .into());
    }
    Ok(d)
}
pub fn load_report(path: Option<&str>) -> Result<String> {
    let Some(p) = path
        .filter(|s| !s.is_empty())
        .map(Path::new)
        .filter(|p| p.exists())
    else {
        return Ok(String::new());
    };
    std::fs::read_to_string(p)
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(crate::Error::from)
}
pub fn esc(s: &str) -> String {
    escape(s, true)
}
pub fn escape(s: &str, quote: bool) -> String {
    let s = s
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if quote {
        s.replace('"', "&quot;").replace('\'', "&#x27;")
    } else {
        s
    }
}
fn whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}
pub fn collapse(s: &str) -> String {
    s.split(whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn safe_mermaid(t: &str) -> String {
    escape(
        &collapse(
            &t.replace('"', "'")
                .replace(['`', '#', '{', '}'], "")
                .replace('|', " ")
                .replace("->>", " to ")
                .replace("-->", " to ")
                .replace("->", " to "),
        ),
        false,
    )
}
pub fn comment(t: &str) -> String {
    t.replace("--", "- -").replace('\n', " ")
}
pub fn digest(t: &str, len: usize) -> String {
    format!("{:x}", Sha1::digest(t.as_bytes()))[..len].into()
}
pub fn stable_ascii_id(raw: &str, prefix: &str, limit: i64) -> String {
    let mut slug = regex::Regex::new("[^A-Za-z0-9_]+")
        .unwrap()
        .replace_all(raw, "_")
        .into_owned();
    slug = regex::Regex::new("_+")
        .unwrap()
        .replace_all(&slug, "_")
        .trim_matches('_')
        .into();
    if slug.is_empty() {
        slug = prefix.into()
    }
    if slug.chars().next().is_some_and(|c| c.is_numeric()) {
        slug = format!("{prefix}_{slug}")
    };
    let n = if limit < 0 {
        (slug.chars().count() as i64 + limit).max(0)
    } else {
        limit
    };
    format!(
        "{}_{}",
        slug.chars()
            .take(n as usize)
            .collect::<String>()
            .trim_end_matches('_'),
        digest(raw, 8)
    )
}
pub fn node_id(n: &Value) -> String {
    stable_ascii_id(&text(n.get("id").unwrap_or(&json!("unknown"))), "node", 48)
}
pub fn section_id(s: &str) -> String {
    stable_ascii_id(s, "section", 48).to_uppercase()
}
pub fn short_path(s: &str) -> String {
    let p = s.split('/').collect::<Vec<_>>();
    p[p.len().saturating_sub(3)..].join("/")
}
pub fn safe_filename(s: &str, fallback: &str) -> String {
    let r = regex::Regex::new("[^A-Za-z0-9._-]+")
        .unwrap()
        .replace_all(s, "-");
    let r = r.trim_matches(['-', '.', '_']);
    if r.is_empty() {
        fallback.into()
    } else {
        r.into()
    }
}
pub fn basename(s: &str) -> String {
    path_syntax::posix_name(&path_syntax::posix_parts(s)).into()
}
pub fn infer_project_name(p: &str, meta: &Value) -> Result<String> {
    if truth(&meta["project_name"]) {
        return Ok(s(&meta["project_name"]));
    }
    let p = fsutil::resolve_missing(Path::new(p)).map_err(crate::Error::from)?;
    let parent = p.parent().unwrap_or(Path::new("/"));
    if parent
        .file_name()
        .is_some_and(|s| s == paths::OUT_DIR_NAME.as_str())
    {
        return Ok(parent
            .parent()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default());
    }
    Ok(parent
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Project".into()))
}
pub fn expanduser(p: &str) -> Result<PathBuf> {
    let Some(rest) = p.strip_prefix('~') else {
        return Ok(PathBuf::from(p));
    };
    let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let home = if name.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            Some(PathBuf::from(home))
        } else {
            #[cfg(unix)]
            {
                nix::unistd::User::from_uid(nix::unistd::Uid::current())
                    .map_err(crate::Error::from)?
                    .map(|u| u.dir)
            }
            #[cfg(not(unix))]
            {
                std::env::var_os("USERPROFILE").map(PathBuf::from)
            }
        }
    } else {
        #[cfg(unix)]
        {
            nix::unistd::User::from_name(name)
                .map_err(crate::Error::from)?
                .map(|u| u.dir)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
    .ok_or("Could not determine home directory.")?;
    Ok(if home.as_os_str().is_empty() {
        PathBuf::from("/").join(tail)
    } else {
        home.join(tail)
    })
}
pub fn resolve_paths(args: &Value) -> Result<Value> {
    let opt = |k: &str| {
        args.get(k)
            .filter(|v| truth(v))
            .map(|v| expanduser(&s(v)))
            .transpose()
    };
    let base = opt("project")?.unwrap_or(std::env::current_dir().map_err(crate::Error::from)?);
    let out = if let Some(o) = opt("graph_dir")? {
        o
    } else if let Some(g) = opt("graph")? {
        g.parent().unwrap_or(Path::new(".")).into()
    } else if base.join("graph.json").exists() {
        base.clone()
    } else {
        base.join(paths::OUT_DIR.as_str())
    };
    let root = if out
        .file_name()
        .is_some_and(|s| s == paths::OUT_DIR_NAME.as_str())
    {
        out.parent().unwrap_or(Path::new(".")).into()
    } else {
        base
    };
    Ok(
        json!({"base":fsutil::path_string(&root),"graph_dir":fsutil::path_string(&out),"graph":fsutil::path_string(&opt("graph")?.unwrap_or_else(||out.join("graph.json"))),"report":fsutil::path_string(&opt("report")?.unwrap_or_else(||out.join("GRAPH_REPORT.md"))),"labels":fsutil::path_string(&opt("labels")?.unwrap_or_else(||out.join(".graphicle_labels.json"))),"sections":opt("sections")?.map(|p|fsutil::path_string(&p))}),
    )
}
pub fn zh(lang: &str) -> bool {
    lang.to_lowercase().starts_with("zh")
}
pub fn pick<'a>(lang: &str, z: &'a str, en: &'a str) -> &'a str {
    if zh(lang) { z } else { en }
}
pub fn detect_lang(lang: &str, nodes: &[Value], labels: &Value) -> String {
    if !lang.is_empty() && lang.to_lowercase() != "auto" {
        return lang.into();
    }
    let values = labels
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .take(50)
        .map(s)
        .chain(nodes.iter().take(200).map(|n| get(n, "label", "")))
        .chain(nodes.iter().take(100).map(|n| get(n, "source_file", "")));
    if values
        .flat_map(|s| s.chars().collect::<Vec<_>>())
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        "zh-CN".into()
    } else {
        "en".into()
    }
}
pub fn truncate(t: &str, limit: i64) -> String {
    let t = collapse(t);
    if t.chars().count() as i64 <= limit {
        t
    } else {
        format!(
            "{}...",
            t.chars()
                .take((limit - 3).max(0) as usize)
                .collect::<String>()
                .trim_end_matches(whitespace)
        )
    }
}
pub fn humanize(label: &str, source: &str) -> String {
    let mut l = label.trim_matches(whitespace).to_string();
    if l.is_empty() {
        return if source.is_empty() {
            "Unknown".into()
        } else {
            basename(source)
        };
    }
    if l.starts_with('.') && l.ends_with("()") {
        return l[1..].into();
    }
    if [
        ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".rb",
    ]
    .iter()
    .any(|x| l.ends_with(x))
    {
        return basename(&l);
    }
    if l.contains('_') && !l.contains(' ') && l.chars().count() > 28 {
        let p = l.split('_').filter(|s| !s.is_empty()).collect::<Vec<_>>();
        if !p.is_empty() {
            l = p[p.len().saturating_sub(3)..].join(" ");
        }
    }
    truncate(&l, 42)
}
fn any(s: &str, words: &[&str]) -> bool {
    words.iter().any(|w| s.contains(w))
}
pub fn kind(n: &Value) -> &'static str {
    let raw = text(&n["label"]);
    let l = if raw.is_empty() {
        text(&n["id"])
    } else {
        raw.clone()
    }
    .to_lowercase();
    let src = text(&n["source_file"]).to_lowercase();
    let ft = text(&n["file_type"]).to_lowercase();
    let nt = text(&n["node_type"]).to_lowercase();
    for (members, k) in [
        (
            &[
                "class",
                "klass",
                "struct",
                "interface",
                "enum",
                "trait",
                "model",
            ][..],
            "klass",
        ),
        (&["module", "file", "package", "namespace"][..], "module"),
        (
            &["endpoint", "route", "api", "handler", "controller"][..],
            "api",
        ),
        (&["test", "spec"][..], "test"),
        (&["component", "hook", "view", "page"][..], "ui"),
    ] {
        if members.contains(&nt.as_str()) {
            return k;
        }
    }
    if ["rationale", "document"].contains(&ft.as_str()) {
        return "concept";
    }
    if src.contains("test") || l.starts_with("test_") || src.contains("spec") {
        return "test";
    }
    for (words, k) in [
        (&["endpoint", "router", "api", "route"][..], "api"),
        (&["cli", "command", "click", "typer"][..], "entry"),
        (&["async", "await", "stream", "sse"][..], "async"),
    ] {
        if any(&l, words) {
            return k;
        }
    }
    let hook = raw.starts_with("use")
        && raw
            .chars()
            .nth(3)
            .is_some_and(|c| c.is_uppercase() || c == '_' || c == '-');
    if any(&l, &["component", "props", "hook", "store"])
        || hook
        || [".tsx", ".jsx", ".vue", ".svelte"]
            .iter()
            .any(|s| src.ends_with(s))
    {
        return "ui";
    }
    if raw.chars().next().is_some_and(char::is_uppercase) && !raw.ends_with("()") {
        return "klass";
    }
    if [
        ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".kt", ".rb", ".php", ".cs",
        ".swift", ".vue", ".svelte",
    ]
    .iter()
    .any(|s| raw.ends_with(s))
    {
        return "module";
    }
    "function"
}
pub fn relation(r: &str, lang: &str) -> String {
    let r = r.trim_matches(whitespace);
    let x = match r {
        "calls" => ("调用", "calls"),
        "uses" => ("使用", "uses"),
        "imports" | "imports_from" => ("导入", "imports"),
        "method" => ("方法", "method"),
        "contains" => ("包含", "contains"),
        "rationale_for" => ("说明", "explains"),
        "conceptually_related_to" => ("相关", "relates"),
        "participate_in" => ("参与", "joins"),
        "form" => ("组成", "forms"),
        _ => return safe_mermaid(&r.replace('_', " ")),
    };
    safe_mermaid(pick(lang, x.0, x.1))
}
pub fn include(e: &Value) -> bool {
    let c = get(e, "confidence", "EXTRACTED").to_uppercase();
    c == "EXTRACTED" || (c == "INFERRED" && float(&e["confidence_score"], 1.0) >= 0.85)
}
pub fn preferred(edges: &[Value], structure: bool) -> Vec<Value> {
    let chosen = edges
        .iter()
        .filter(|e| include(e))
        .filter(|e| {
            ["calls", "uses", "method", "imports", "imports_from"]
                .contains(&get(e, "relation", "").as_str())
                || (structure
                    && ["contains", "rationale_for", "conceptually_related_to"]
                        .contains(&get(e, "relation", "").as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    if chosen.is_empty() {
        edges.iter().filter(|e| include(e)).cloned().collect()
    } else {
        chosen
    }
}
pub fn edge_score(e: &Value) -> f64 {
    let mut n = float(&e["confidence_score"], 1.0);
    if get(e, "confidence", "").to_uppercase() == "EXTRACTED" {
        n += 2.0
    }
    n + match get(e, "relation", "").as_str() {
        "calls" | "uses" | "method" => 1.0,
        "imports" | "imports_from" => 0.6,
        "contains" => -0.2,
        "rationale_for" => -0.6,
        _ => 0.0,
    }
}
pub fn mermaid_init(scale: f64, direction: &str) -> String {
    let scale = if scale == 0.0 { 1.0 } else { scale }.clamp(0.65, 1.8);
    let config = json!({"theme":"dark","themeVariables":{"fontSize":format!("{:.1}px",15.0*scale),"fontFamily":"Segoe UI, system-ui, sans-serif","primaryColor":"#1e293b","primaryTextColor":"#e2e8f0","primaryBorderColor":"#38bdf8","secondaryColor":"#0f172a","tertiaryColor":"#334155","lineColor":"#64748b","textColor":"#e2e8f0"},"flowchart":{"htmlLabels":true,"curve":"basis","nodeSpacing":(48.0*scale).round_ties_even() as i64,"rankSpacing":(64.0*scale).round_ties_even() as i64,"padding":(14.0*scale).round_ties_even() as i64,"diagramPadding":(10.0*scale).round_ties_even() as i64,"useMaxWidth":true}});
    let mut v = vec![];
    crate::python_json::write(&mut v, &config, None, false).unwrap();
    render(
        "mermaid_init:625",
        &[String::from_utf8(v).unwrap(), direction.into()],
    )
}
pub fn classes() -> Vec<String> {
    ar(&ASSETS["classes"]).iter().map(s).collect()
}
pub fn community_index(nodes: &[Value]) -> Value {
    let mut m = Map::new();
    for n in nodes {
        m.entry(get(n, "community", "unknown"))
            .or_insert(json!([]))
            .as_array_mut()
            .unwrap()
            .push(n.clone());
    }
    Value::Object(m)
}
pub fn anchor(raw: &str, fallback: &str, used: &mut HashSet<String>) -> String {
    let raw = if raw.is_empty() { fallback } else { raw };
    let re = regex::Regex::new("[^a-z0-9]+").unwrap();
    let slug = |s: &str| {
        re.replace_all(&s.to_lowercase(), "-")
            .trim_matches('-')
            .to_string()
    };
    let mut base = slug(raw);
    if base.is_empty() {
        base = slug(if fallback.is_empty() {
            "section"
        } else {
            fallback
        });
    }
    if base.is_empty() {
        base = "section".into()
    }
    base = base
        .chars()
        .take(48)
        .collect::<String>()
        .trim_matches('-')
        .into();
    if base.is_empty() {
        base = "section".into()
    }
    let mut v = base.clone();
    if used.contains(&v) {
        v = format!("{base}-{}", digest(raw, 6))
    }
    let mut i = 2;
    while used.contains(&v) {
        v = format!("{base}-{i}");
        i += 1;
    }
    used.insert(v.clone());
    v
}
pub fn normalize_communities(v: &Value) -> Value {
    if v.is_array() {
        v.clone()
    } else if v.is_null() || *v == json!("") {
        json!([])
    } else if let Some(s) = v.as_str() {
        json!(
            s.split(',')
                .map(|s| s.trim_matches(whitespace))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        )
    } else {
        json!([v])
    }
}
pub fn normalize_sections(sections: &[Value], lang: &str) -> Vec<Value> {
    let name = pick(lang, "架构总览", "Architecture Overview");
    let mut out = vec![json!({"id":"overview","name":name,"communities":[]})];
    let mut used = HashSet::from(["overview".into(), "hyperedges".into(), "stats".into()]);
    for (i, r) in sections.iter().enumerate() {
        if !r.is_object() {
            continue;
        }
        let rid = ["id", "key", "name"]
            .iter()
            .map(|k| &r[*k])
            .find(|v| truth(v))
            .map(s)
            .unwrap_or_else(|| format!("section-{}", i + 1));
        let rn = ["name", "label"]
            .iter()
            .map(|k| &r[*k])
            .find(|v| truth(v))
            .map(s)
            .unwrap_or_else(|| rid.clone());
        if rid.to_lowercase() == "overview" {
            out[0]["name"] = json!(if rn.is_empty() { name } else { &rn });
            continue;
        }
        out.push(json!({"id":anchor(&rid,&format!("section-{}",i+1),&mut used),"name":rn,"communities":normalize_communities(r.get("communities").unwrap_or(&r["community"]))}));
    }
    out
}
pub fn counter(values: impl IntoIterator<Item = String>) -> Vec<(String, usize)> {
    let mut pos: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::<(String, usize)>::new();
    for v in values {
        if let Some(&i) = pos.get(&v) {
            out[i].1 += 1
        } else {
            pos.insert(v.clone(), out.len());
            out.push((v, 1));
        }
    }
    out
}
pub fn most_common(mut v: Vec<(String, usize)>, n: usize) -> Vec<(String, usize)> {
    v.sort_by_key(|item| std::cmp::Reverse(item.1));
    v.truncate(n);
    v
}
pub fn keywords(nodes: &[Value], limit: usize) -> Vec<String> {
    let stop = [
        "the", "and", "for", "with", "from", "this", "that", "class", "function", "method", "file",
        "src", "lib", "core", "index", "main", "init", "py", "ts", "tsx", "js", "jsx", "go", "rs",
        "java", "html", "css",
    ];
    let mut words = vec![];
    for n in nodes {
        let t = format!("{} {}", get(n, "label", ""), get(n, "source_file", ""))
            .replace(['/', '_', '-'], " ");
        for raw in t.split(whitespace) {
            let word = raw
                .to_lowercase()
                .chars()
                .filter(|c| {
                    let cp = *c as u32;
                    let ranges = crate::unicode15::WORD_RANGES;
                    let i = ranges.partition_point(|&(a, _)| a <= cp);
                    i > 0 && cp <= ranges[i - 1].1 && *c != '_'
                })
                .collect::<String>();
            if word.chars().count() >= 3 && !stop.contains(&word.as_str()) {
                words.push(word);
            }
        }
    }
    most_common(counter(words), limit)
        .into_iter()
        .map(|p| p.0)
        .collect()
}
fn title(s: &str) -> String {
    let mut prev = false;
    let mut o = String::new();
    for c in s.chars() {
        if prev {
            o.extend(c.to_lowercase())
        } else {
            o.extend(c.to_uppercase())
        }
        prev = c.is_lowercase() || c.is_uppercase();
    }
    o
}
pub fn label_community(cid: &str, labels: &Value, nodes: &[Value], lang: &str) -> String {
    if truth(&labels[cid]) {
        return s(&labels[cid]);
    }
    let k = keywords(nodes, 3);
    if !k.is_empty() {
        return k.iter().map(|w| title(w)).collect::<Vec<_>>().join(" ");
    }
    if zh(lang) {
        format!("社区 {cid}")
    } else {
        format!("Community {cid}")
    }
}
pub fn community_text(nodes: &[Value], label: &str) -> String {
    let mut v = vec![label.into()];
    for n in nodes.iter().take(80) {
        for k in ["label", "source_file", "node_type", "file_type"] {
            v.push(get(n, k, ""));
        }
    }
    v.join(" ").to_lowercase()
}
pub fn keyword_score(t: &str, keywords: &[Value]) -> usize {
    let bytes = t.as_bytes();
    keywords
        .iter()
        .map(|v| {
            let k = s(v);
            let mut pos = 0;
            let mut count = 0;
            while !k.is_empty() && pos <= t.len() {
                let Some(i) = t[pos..].find(&k) else { break };
                let i = i + pos;
                let end = i + k.len();
                let letter = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
                if (i == 0 || !letter(bytes[i - 1])) && (end == bytes.len() || !letter(bytes[end]))
                {
                    count += 1;
                    pos = end
                } else {
                    pos = i + t[i..].chars().next().unwrap().len_utf8();
                }
            }
            count
        })
        .sum()
}
pub fn ranked_sections(grouped: &Value, max: i64) -> (Vec<Value>, Vec<Value>) {
    let mut v = grouped
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .cloned()
        .collect::<Vec<_>>();
    v.sort_by(|a, b| {
        a["priority"]
            .as_i64()
            .cmp(&b["priority"].as_i64())
            .then_with(|| b["node_count"].as_i64().cmp(&a["node_count"].as_i64()))
            .then_with(|| s(&a["id"]).cmp(&s(&b["id"])))
    });
    let cap = (if max == 0 { 15 } else { max }).max(1) as usize;
    let overflow = v
        .iter()
        .skip(cap)
        .flat_map(|s| ar(&s["communities"]).iter().cloned())
        .collect();
    v.truncate(cap);
    (v, overflow)
}
pub fn derive_sections(nodes: &[Value], labels: &Value, lang: &str, max: i64) -> Vec<Value> {
    let idx = community_index(nodes);
    let mut communities = idx.as_object().unwrap().iter().collect::<Vec<_>>();
    communities.sort_by(|(a, v), (b, w)| ar(w).len().cmp(&ar(v).len()).then(a.cmp(b)));
    let mut grouped = json!({});
    let mut unassigned = vec![];
    for (cid, nodes) in communities {
        let label = label_community(cid, labels, ar(nodes), lang);
        let t = community_text(ar(nodes), &label);
        let mut best = None;
        let mut score = 0;
        for (priority, ty) in ar(&ASSETS["SECTION_ARCHETYPES"]).iter().enumerate() {
            let sc = keyword_score(&t, ar(&ty[3]));
            if sc > score {
                score = sc;
                best = Some((priority, ty));
            }
        }
        if let Some((p, ty)) = best.filter(|_| score >= 2) {
            let id = s(&ty[0]);
            if grouped.get(&id).is_none() {
                grouped[&id] = json!({"id":id,"name":pick(lang,ty[1].as_str().unwrap(),ty[2].as_str().unwrap()),"communities":[],"node_count":0,"priority":p});
            }
            grouped[&id]["communities"]
                .as_array_mut()
                .unwrap()
                .push(json!(cid));
            grouped[&id]["node_count"] =
                json!(grouped[&id]["node_count"].as_u64().unwrap() + ar(nodes).len() as u64);
        } else {
            unassigned.push((cid.clone(), label));
        }
    }
    let max = if max == 0 { 15 } else { max };
    let (selected, mut overflow) = ranked_sections(&grouped, max.max(1) - 1);
    let mut out = vec![
        json!({"id":"overview","name":pick(lang,"架构总览","Architecture Overview"),"communities":[]}),
    ];
    out.extend(
        selected
            .into_iter()
            .map(|s| json!({"id":s["id"],"name":s["name"],"communities":s["communities"]})),
    );
    let slots = (max - out.len() as i64).max(0) as usize;
    for (cid, label) in unassigned.iter().take(slots) {
        out.push(json!({"id":if label.is_empty(){format!("community-{cid}")}else{label.clone()},"name":label,"communities":[cid]}));
    }
    overflow.extend(unassigned.iter().skip(slots).map(|(cid, _)| json!(cid)));
    if !overflow.is_empty() {
        out.push(json!({"id":"other","name":pick(lang,"其他","Other"),"communities":overflow}));
    }
    out
}
pub fn section_node_map(sections: &[Value], idx: &Value) -> Value {
    let mut out = Map::new();
    for sec in sections {
        let id = s(&sec["id"]);
        let nodes = if id == "overview" {
            vec![]
        } else {
            ar(&sec["communities"])
                .iter()
                .flat_map(|cid| ar(&idx[s(cid)]).iter().cloned())
                .collect::<Vec<_>>()
        };
        out.insert(id, json!(nodes));
    }
    Value::Object(out)
}
pub fn classify(edges: &[Value], sections: &Value) -> Value {
    let mut ns = json!({});
    if let Some(m) = sections.as_object() {
        for (sid, nodes) in m {
            for n in ar(nodes) {
                ns[s(&n["id"])] = json!(sid);
            }
        }
    }
    let mut intra = json!({});
    let mut inter = vec![];
    let mut orphan = vec![];
    for e in edges {
        let a = &ns[get(e, "source", "")];
        let b = &ns[get(e, "target", "")];
        if a.is_null() || b.is_null() {
            orphan.push(e.clone())
        } else if a == b {
            let k = s(a);
            if intra.get(&k).is_none() {
                intra[&k] = json!([])
            }
            intra[&k].as_array_mut().unwrap().push(e.clone());
        } else {
            inter.push(e.clone())
        }
    }
    json!({"intra":intra,"inter":inter,"orphan":orphan,"node_section":ns})
}
