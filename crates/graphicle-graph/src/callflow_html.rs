// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/callflow_html.py
//! Native graphify/callflow_html.py document generation. HTML/CSS/JS literals
//! are retained; all graph selection and rendering decisions execute in Rust.

#[derive(Clone, Copy)]
pub struct FlowchartOptions<'a> {
    pub lang: &'a str,
    pub scale: f64,
    pub max_nodes: i64,
    pub max_edges: i64,
}
pub use crate::callflow_data::*;
use crate::{Result, fsutil};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
pub fn degree_scores(edges: &[Value]) -> Value {
    let mut out = Map::new();
    for e in edges {
        let score = edge_score(e);
        for k in ["source", "target"] {
            let id = get(e, k, "");
            let n = out.get(&id).map(|v| float(v, 0.0)).unwrap_or(0.0) + score;
            out.insert(id, json!(n));
        }
    }
    Value::Object(out)
}
pub fn importance(n: &Value) -> f64 {
    for k in [
        "pagerank",
        "page_rank",
        "pageRank",
        "rank",
        "centrality",
        "score",
    ] {
        if let Some(v) = n.get(k) {
            return float(v, 0.0);
        }
    }
    0.0
}
pub fn select_nodes(nodes: &[Value], edges: &[Value], max: i64) -> Vec<Value> {
    let mut by_id = Map::new();
    for n in nodes {
        by_id.insert(s(&n["id"]), n.clone());
    }
    let mut edges = preferred(edges, false);
    if edges.is_empty() {
        edges = preferred(&edges, true)
    }
    let scores = degree_scores(&edges);
    let outgoing = counter(edges.iter().map(|e| get(e, "source", "")))
        .into_iter()
        .collect::<HashMap<_, _>>();
    let incoming = counter(edges.iter().map(|e| get(e, "target", "")))
        .into_iter()
        .collect::<HashMap<_, _>>();
    let mut selected = vec![];
    let mut seen = HashSet::new();
    let add = |id: &str, selected: &mut Vec<Value>, seen: &mut HashSet<String>| -> bool {
        let Some(n) = by_id.get(id) else { return false };
        if seen.contains(id) || (!truth(n)) {
            return false;
        }
        if kind(n) == "concept" && selected.len() as i64 >= 4.max(max.div_euclid(3)) {
            return false;
        }
        selected.push(n.clone());
        seen.insert(id.into());
        selected.len() as i64 >= max
    };
    let mut entries = by_id.keys().cloned().collect::<Vec<_>>();
    let out = |id: &str| *outgoing.get(id).unwrap_or(&0) as i64;
    let inc = |id: &str| *incoming.get(id).unwrap_or(&0) as i64;
    entries.sort_by(|a, b| {
        (out(b) - inc(b))
            .cmp(&(out(a) - inc(a)))
            .then(out(b).cmp(&out(a)))
            .then(a.cmp(b))
    });
    for id in entries.iter().take(3.max(max.div_euclid(3)) as usize) {
        if out(id) > 0 && add(id, &mut selected, &mut seen) {
            return selected;
        }
    }
    edges.sort_by(|a, b| edge_score(b).total_cmp(&edge_score(a)));
    for e in edges {
        for k in ["source", "target"] {
            if add(&s(&e[k]), &mut selected, &mut seen) {
                return selected;
            }
        }
    }
    let mut fallback = nodes.to_vec();
    fallback.sort_by(|a, b| {
        let ka = kind(a) == "concept";
        let kb = kind(b) == "concept";
        ka.cmp(&kb)
            .then_with(|| {
                float(&scores[get(b, "id", "")], 0.0)
                    .total_cmp(&float(&scores[get(a, "id", "")], 0.0))
            })
            .then_with(|| importance(b).total_cmp(&importance(a)))
            .then_with(|| {
                short_path(&get(a, "source_file", "")).cmp(&short_path(&get(b, "source_file", "")))
            })
            .then_with(|| {
                humanize(&get(a, "label", &get(a, "id", "")), "")
                    .cmp(&humanize(&get(b, "label", &get(b, "id", "")), ""))
            })
    });
    for n in fallback {
        let id = s(&n["id"]);
        if seen.insert(id) {
            selected.push(n)
        }
        if selected.len() as i64 >= max {
            break;
        }
    }
    selected
}
pub fn node_label(n: &Value) -> String {
    let src = get(n, "source_file", "");
    let label = humanize(
        &if truth(&n["label"]) {
            s(&n["label"])
        } else {
            text(&n["id"])
        },
        &src,
    );
    let source = short_path(&src);
    if !source.is_empty() && !label.ends_with(&basename(&source)) {
        format!(
            "{}<br/><small>{}</small>",
            safe_mermaid(&label),
            safe_mermaid(&source)
        )
    } else {
        safe_mermaid(&label)
    }
}
pub fn group_files(nodes: &[Value]) -> Value {
    let mut m = Map::new();
    for n in nodes {
        let mut source = short_path(&get(n, "source_file", ""));
        if source.is_empty() {
            source = "External / generated".into()
        }
        m.entry(source)
            .or_insert(json!([]))
            .as_array_mut()
            .unwrap()
            .push(n.clone());
    }
    let mut entries = m.into_iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| ar(&b.1).len().cmp(&ar(&a.1).len()).then(a.0.cmp(&b.0)));
    Value::Object(entries.into_iter().collect())
}
#[derive(Clone)]
pub struct SectionSummary {
    pub source: String,
    pub target: String,
    pub count: usize,
    pub relations: Vec<(String, usize)>,
}
pub fn edge_summary(classified: &Value) -> Vec<SectionSummary> {
    let ns = &classified["node_section"];
    let mut out: Vec<SectionSummary> = vec![];
    for e in ar(&classified["inter"]) {
        if !include(e) {
            continue;
        }
        let a = &ns[s(&e["source"])];
        let b = &ns[s(&e["target"])];
        if !truth(a) || !truth(b) || a == b {
            continue;
        }
        let source = s(a);
        let target = s(b);
        let idx = out
            .iter()
            .position(|x| x.source == source && x.target == target)
            .unwrap_or_else(|| {
                out.push(SectionSummary {
                    source,
                    target,
                    count: 0,
                    relations: vec![],
                });
                out.len() - 1
            });
        let entry = &mut out[idx];
        entry.count += 1;
        let rel = get(e, "relation", "relates");
        if let Some(r) = entry.relations.iter_mut().find(|r| r.0 == rel) {
            r.1 += 1
        } else {
            entry.relations.push((rel, 1));
        }
    }
    out
}
pub fn overview(
    sections: &[Value],
    map: &Value,
    classified: &Value,
    lang: &str,
    scale: f64,
) -> String {
    let mut lines = vec![mermaid_init(scale, "LR")];
    let sections = sections
        .iter()
        .filter(|s| s["id"] != "overview")
        .collect::<Vec<_>>();
    for sec in &sections {
        let id = s(&sec["id"]);
        let sid = section_id(&id);
        let label = format!(
            "{}<br/><small>{} nodes</small>",
            safe_mermaid(&get(sec, "name", &id)),
            ar(&map[&id]).len()
        );
        lines.push(format!("    {sid}(\"{label}\")"));
        lines.push(format!("    class {sid} module;"));
    }
    let mut summary = edge_summary(classified);
    summary.sort_by_key(|item| std::cmp::Reverse(item.count));
    for item in summary.iter().take(12) {
        let r = most_common(item.relations.clone(), 1);
        let mut label = relation(&r[0].0, lang);
        if item.count > 1 {
            label = format!("{label} x{}", item.count)
        }
        lines.push(format!(
            "    {} -->|{label}| {}",
            section_id(&item.source),
            section_id(&item.target)
        ));
    }
    if summary.is_empty() && sections.len() > 1 {
        for p in sections.windows(2) {
            lines.push(format!(
                "    {} -.-> {}",
                section_id(&s(&p[0]["id"])),
                section_id(&s(&p[1]["id"]))
            ));
        }
    }
    lines.extend(classes());
    lines.join("\n")
}
pub fn flowchart(
    id: &str,
    name: &str,
    nodes: &[Value],
    edges: &[Value],
    context: FlowchartOptions<'_>,
) -> String {
    let FlowchartOptions {
        lang,
        scale,
        max_nodes,
        max_edges,
    } = context;

    let mut lines = vec![
        mermaid_init(scale, "LR"),
        format!(
            "    %% Section: {} ({} nodes, {} edges)",
            safe_mermaid(name),
            nodes.len(),
            edges.len()
        ),
    ];
    if nodes.is_empty() {
        lines.push(format!(
            "    empty(\"{}\")",
            safe_mermaid(&if zh(lang) {
                format!("{name} - 无节点")
            } else {
                format!("{name} - no nodes")
            })
        ));
        lines.extend(classes());
        return lines.join("\n");
    }
    let selected = select_nodes(nodes, edges, max_nodes);
    let ids = selected.iter().map(|n| s(&n["id"])).collect::<HashSet<_>>();
    let filter = |v: Vec<Value>| {
        v.into_iter()
            .filter(|e| ids.contains(&s(&e["source"])) && ids.contains(&s(&e["target"])))
            .collect::<Vec<_>>()
    };
    let mut visible = filter(preferred(edges, false));
    if visible.is_empty() {
        visible = filter(preferred(edges, true))
    }
    let groups = group_files(&selected);
    let mut class_lines = vec![];
    let group_count = groups.as_object().unwrap().len();
    for (src, group) in groups.as_object().unwrap() {
        let gid = node_id(&json!({"id":format!("{id}_{src}")}));
        let sub = group_count > 1 && ar(group).len() > 1;
        let indent = if sub {
            lines.push(format!("    subgraph {gid}[\"{}\"]", safe_mermaid(src)));
            "        "
        } else {
            "    "
        };
        for n in ar(group) {
            let mid = node_id(n);
            lines.push(format!("{indent}{mid}(\"{}\")", node_label(n)));
            class_lines.push(format!("    class {mid} {};", kind(n)));
        }
        if sub {
            lines.push("    end".into());
        }
    }
    visible.sort_by(|a, b| edge_score(b).total_cmp(&edge_score(a)));
    let mut included = 0usize;
    for e in &visible {
        if included as i64 >= max_edges {
            break;
        }
        lines.push(format!(
            "    {} -->|{}| {}",
            node_id(&json!({"id":e.get("source").unwrap_or(&json!(""))})),
            relation(&get(e, "relation", ""), lang),
            node_id(&json!({"id":e.get("target").unwrap_or(&json!(""))}))
        ));
        included += 1;
    }
    let omitted_nodes = nodes.len().saturating_sub(selected.len());
    let omitted_edges = visible.len().saturating_sub(included);
    if omitted_nodes > 0 || omitted_edges > 0 {
        lines.push(format!(
            "    %% Omitted for readability: {omitted_nodes} nodes, {omitted_edges} edges"
        ));
    }
    lines.extend(class_lines);
    lines.extend(classes());
    lines.join("\n")
}
pub fn nav(sections: &[Value]) -> String {
    format!(
        "<div class=\"nav\">\n{}\n</div>",
        sections
            .iter()
            .map(|sec| format!(
                "    <a href=\"#{}\">{}</a>",
                esc(&s(&sec["id"])),
                esc(&s(&sec["name"]))
            ))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
pub fn display_name(n: &Value, fallback: &str) -> String {
    if !truth(n) {
        return fallback.into();
    }
    let label = [&n["label"], &n["id"]]
        .into_iter()
        .find(|v| truth(v))
        .map(s)
        .unwrap_or(fallback.into());
    humanize(&label, &get(n, "source_file", ""))
}
pub fn node_refs(ids: &[String], nodes: &Value, lang: &str, empty: &str, limit: i64) -> String {
    if ids.is_empty() {
        return esc(empty);
    }
    let mut ids = ids.to_vec();
    ids.sort_by(|a, b| {
        display_name(&nodes[a], a)
            .to_lowercase()
            .cmp(&display_name(&nodes[b], b).to_lowercase())
            .then(a.cmp(b))
    });
    let cap = if limit < 0 {
        (ids.len() as i64 + limit).max(0) as usize
    } else {
        limit as usize
    };
    let mut parts = vec![];
    for id in ids.iter().take(cap) {
        let node = &nodes[id];
        let label = display_name(node, id);
        let source = short_path(&get(node, "source_file", ""));
        parts.push(if source.is_empty() {
            format!("<code>{}</code>", esc(&label))
        } else {
            format!(
                "<code>{}</code><br><small style=\"color:var(--muted)\">{}</small>",
                esc(&label),
                esc(&source)
            )
        });
    }
    if ids.len() as i64 > limit {
        let n = ids.len() as i64 - limit;
        parts.push(esc(&if zh(lang) {
            format!("+{n} 个更多")
        } else {
            format!("+{n} more")
        }));
    }
    parts.join("<br>")
}
pub fn suggest_tag(label: &str, file_type: &str, lang: &str, kind: &str) -> String {
    let lower = label.to_lowercase();
    let known = match kind {
        "concept" => Some(("概念", "Concept", "tag-func")),
        "entry" => Some(("入口", "Entry", "tag-cmd")),
        "api" => Some(("API", "API", "tag-endpoint")),
        "async" => Some(("异步", "Async", "tag-async")),
        "klass" => Some(("类", "Class", "tag-class")),
        "ui" => Some(("UI", "UI", "tag-hook")),
        "module" => Some(("模块", "Module", "tag-class")),
        "test" => Some(("测试", "Test", "tag-func")),
        "function" => Some(("函数", "Function", "tag-func")),
        _ => None,
    };
    let any = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let (z, e, c) = known.unwrap_or_else(|| {
        if file_type == "rationale" {
            ("概念", "Concept", "tag-func")
        } else if any(&["cli", "command", "scan", "serve", "chat", "config"])
            && any(&["group", "command"])
        {
            ("CLI命令", "CLI", "tag-cmd")
        } else if any(&["router", "endpoint", "api", "/api/"]) {
            ("API端点", "API", "tag-endpoint")
        } else if any(&["async", "await", "stream"]) {
            ("异步", "Async", "tag-async")
        } else if any(&["class", "model", "schema", "dataclass", "pydantic"]) {
            ("类", "Class", "tag-class")
        } else if any(&["hook", "usestate", "useeffect", "store"]) {
            ("Hook", "Hook", "tag-hook")
        } else if any(&["component", "props", "tsx", "jsx", "render"]) {
            ("组件", "Component", "tag-class")
        } else {
            ("函数", "Function", "tag-func")
        }
    });
    format!("<span class=\"tag {c}\">{}</span>", pick(lang, z, e))
}
pub fn describe(label: &str, source: &str, file_type: &str, lang: &str) -> String {
    let lower = label.to_lowercase();
    let source = if source.is_empty() {
        pick(lang, "项目", "project")
    } else {
        source
    };
    let any = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let (z, e) = if file_type == "rationale" {
        (
            format!("设计说明：{label}"),
            format!("Design note for {label}."),
        )
    } else if file_type == "document" {
        (
            format!("文档入口，描述 {label} 相关能力。"),
            format!("Documentation node describing {label}."),
        )
    } else if [".py", ".tsx", ".ts"].iter().any(|s| label.ends_with(s)) {
        (
            format!("{source} 中的模块文件，承载该层主要实现。"),
            format!("Module file in {source}."),
        )
    } else {
        let v = if any(&["config"]) {
            Some((
                "读取、解析或持久化项目配置。",
                "Reads, resolves, or persists project configuration.",
            ))
        } else if any(&["scan"]) {
            Some((
                "触发项目扫描或处理扫描状态。",
                "Starts scanning or handles scan status.",
            ))
        } else if any(&["ingest", "clone", "git"]) {
            Some((
                "把本地目录或远程仓库转换为分析上下文。",
                "Turns a local path or remote repository into analysis context.",
            ))
        } else if any(&["prompt"]) {
            Some((
                "构造发送给 LLM 的结构化提示。",
                "Builds structured prompts for model calls.",
            ))
        } else if any(&["analy"]) {
            Some((
                "编排分析流程并产出结构化文档数据。",
                "Orchestrates analysis and returns structured documentation data.",
            ))
        } else if any(&["graph", "dependency"]) {
            Some((
                "构建依赖关系并提供排序或图形化数据。",
                "Builds dependency relationships and graph data.",
            ))
        } else if any(&["export", "markdown", "html"]) {
            Some((
                "将文档数据导出为目标格式。",
                "Exports documentation data to a target format.",
            ))
        } else if any(&["chat", "rag", "retrieve"]) {
            Some((
                "支撑检索增强问答或流式聊天。",
                "Supports retrieval-augmented Q&A or streaming chat.",
            ))
        } else if any(&["wiki", "page", "sidebar"]) {
            Some((
                "组织文档页面、侧边栏或内容读取。",
                "Organizes documentation pages, navigation, or content lookup.",
            ))
        } else if any(&["cache", "hash"]) {
            Some((
                "缓存分析结果或生成缓存键。",
                "Caches analysis results or computes cache keys.",
            ))
        } else if any(&["test"]) {
            Some((
                "验证导入、入口点或版本等基础行为。",
                "Verifies imports, entry points, or version behavior.",
            ))
        } else {
            None
        };
        if let Some((z, e)) = v {
            (z.into(), e.into())
        } else {
            (
                format!("{source} 中的 {label} 节点。"),
                format!("{label} node in {source}."),
            )
        }
    };
    pick(lang, &z, &e).into()
}
pub fn call_table(
    nodes: &[Value],
    edges: &[Value],
    lang: &str,
    all_edges: Option<&[Value]>,
    all_nodes: Option<&[Value]>,
) -> String {
    if nodes.is_empty() {
        return String::new();
    }
    let all_nodes = all_nodes.filter(|v| !v.is_empty()).unwrap_or(nodes);
    let mut by_id = json!({});
    for n in all_nodes {
        by_id[s(&n["id"])] = n.clone();
    }
    let mut callers = HashMap::<String, Vec<String>>::new();
    let mut callees = HashMap::<String, Vec<String>>::new();
    for e in all_edges.unwrap_or(edges) {
        if [
            "calls",
            "imports",
            "imports_from",
            "uses",
            "method",
            "indirect_call",
        ]
        .contains(&get(e, "relation", "").as_str())
        {
            let a = get(e, "source", "");
            let b = get(e, "target", "");
            let v = callers.entry(b.clone()).or_default();
            if !v.contains(&a) {
                v.push(a.clone())
            }
            let v = callees.entry(a).or_default();
            if !v.contains(&b) {
                v.push(b)
            }
        }
    }
    let mut rows = vec![];
    for (i, n) in nodes.iter().take(30).enumerate() {
        let id = get(n, "id", "");
        let label = get(n, "label", &id);
        let source = short_path(&get(n, "source_file", ""));
        let ft = get(n, "file_type", "code");
        rows.push(render(
            "generate_call_table_rows:1277",
            &[
                (i + 1).to_string(),
                esc(&label),
                esc(&source),
                suggest_tag(&label, &ft, lang, kind(n)),
                node_refs(
                    callers.get(&id).map(Vec::as_slice).unwrap_or(&[]),
                    &by_id,
                    lang,
                    pick(
                        lang,
                        "外部入口 / 无直接入边",
                        "External entry / no inbound edge",
                    ),
                    3,
                ),
                node_refs(
                    callees.get(&id).map(Vec::as_slice).unwrap_or(&[]),
                    &by_id,
                    lang,
                    pick(lang, "无直接出边", "No direct outbound edge"),
                    3,
                ),
                esc(&describe(&label, &source, &ft, lang)),
            ],
        ));
    }
    rows.join("\n")
}
pub fn doc_title(meta: &Value, lang: &str) -> String {
    format!(
        "{} — {}",
        get(meta, "project_name", "Project"),
        if lang.starts_with("zh") {
            "完整调用流程与架构文档"
        } else {
            "Complete Call Flow & Architecture Documentation"
        }
    )
}
pub fn header(sections: &[Value], meta: &Value, lang: &str) -> String {
    let commit = get(meta, "built_at_commit", "unknown")
        .chars()
        .take(7)
        .collect::<String>();
    let n = get(meta, "node_count", "?");
    let e = get(meta, "edge_count", "?");
    let c = get(meta, "community_count", "?");
    let sub = if lang.starts_with("zh") {
        format!("由 Graphicle 知识图谱生成：{n} 个节点、{e} 条边、{c} 个社区。Commit: {commit}")
    } else {
        format!(
            "Generated from Graphicle knowledge graph: {n} nodes, {e} edges, {c} communities. Commit: {commit}"
        )
    };
    render(
        "generate_header:1379",
        &[esc(&doc_title(meta, lang)), esc(&sub), nav(sections)],
    )
}
pub fn flow_chain(sections: &[Value], classified: &Value) -> String {
    let order = sections
        .iter()
        .filter(|s| s["id"] != "overview")
        .map(|s| get(s, "id", ""))
        .collect::<Vec<_>>();
    if order.is_empty() {
        return "Graph nodes -> documentation".into();
    }
    let names = sections
        .iter()
        .map(|s| (get(s, "id", ""), get(s, "name", &get(s, "id", ""))))
        .collect::<HashMap<_, _>>();
    let summary = edge_summary(classified);
    let mut incoming = HashMap::<String, usize>::new();
    for r in &summary {
        *incoming.entry(r.target.clone()).or_default() += r.count
    }
    let start = order
        .iter()
        .min_by_key(|s| *incoming.get(*s).unwrap_or(&0))
        .unwrap()
        .clone();
    let mut chain = vec![start.clone()];
    let mut current = start;
    while chain.len() < order.len().min(7) {
        let next = summary
            .iter()
            .filter(|r| r.source == current && !chain.contains(&r.target))
            .max_by(|a, b| a.count.cmp(&b.count).then(a.target.cmp(&b.target)))
            .map(|r| r.target.clone())
            .or_else(|| order.iter().find(|s| !chain.contains(s)).cloned());
        let Some(next) = next else { break };
        chain.push(next.clone());
        current = next;
    }
    chain
        .iter()
        .map(|s| names.get(s).unwrap_or(s).clone())
        .collect::<Vec<_>>()
        .join(" -> ")
}
pub fn overview_cards(sections: &[Value], map: &Value, classified: &Value, lang: &str) -> String {
    let rows = sections
        .iter()
        .filter(|s| s["id"] != "overview")
        .map(|sec| {
            let communities = ar(&sec["communities"])
                .iter()
                .map(s)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "<tr><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                esc(&s(&sec["name"])),
                ar(&map[s(&sec["id"])]).len(),
                esc(&communities)
            )
        })
        .collect::<String>();
    render(
        "generate_overview_cards:1436",
        &[
            pick(lang, "架构层次", "Architecture Layers").into(),
            pick(
                lang,
                "<tr><th>层</th><th>节点</th><th>社区</th></tr>",
                "<tr><th>Layer</th><th>Nodes</th><th>Communities</th></tr>",
            )
            .into(),
            rows,
            pick(lang, "核心数据流", "Core Flow").into(),
            esc(&flow_chain(sections, classified)),
        ],
    )
}
pub fn section_intro(sec: &Value, nodes: &[Value], edge_count: usize, lang: &str) -> String {
    let files = most_common(
        counter(
            nodes
                .iter()
                .filter(|n| truth(&n["source_file"]))
                .map(|n| s(&n["source_file"])),
        ),
        3,
    )
    .into_iter()
    .map(|p| short_path(&p.0))
    .collect::<Vec<_>>();
    let keys = keywords(nodes, 4);
    let name = get(sec, "name", &s(&sec["id"]));
    let text = if zh(lang) {
        let f = if files.is_empty() {
            "未标注源文件".into()
        } else {
            files.join("、")
        };
        let k = if keys.is_empty() {
            name.clone()
        } else {
            keys.join("、")
        };
        format!(
            "{name} 汇集了与 {k} 相关的实现，主要分布在 {f}。本节覆盖 {} 个节点、{edge_count} 条内部边，图中只展示最有代表性的调用关系以保持可读性。",
            nodes.len()
        )
    } else {
        let f = if files.is_empty() {
            "unmapped files".into()
        } else {
            files.join(", ")
        };
        let k = if keys.is_empty() {
            name.clone()
        } else {
            keys.join(", ")
        };
        format!(
            "{name} groups implementation around {k}, mostly in {f}. This section covers {} nodes and {edge_count} internal edges; the diagram shows only representative relationships to stay readable.",
            nodes.len()
        )
    };
    format!("<p>{}</p>", esc(&text))
}
pub fn section_cards(nodes: &[Value], edges: &[Value], lang: &str) -> String {
    let mut files = counter(
        nodes
            .iter()
            .filter(|n| truth(&n["source_file"]))
            .map(|n| s(&n["source_file"])),
    );
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let rows = if files.is_empty() {
        format!(
            "<tr><td colspan=\"2\">{}</td></tr>",
            esc(pick(lang, "无源文件映射", "No source file mapping"))
        )
    } else {
        files
            .iter()
            .take(8)
            .map(|(path, n)| {
                format!(
                    "<tr><td><code>{}</code></td><td>{n} {}</td></tr>",
                    esc(&short_path(path)),
                    esc(pick(lang, "个节点", "nodes"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut relations = most_common(
        counter(
            edges
                .iter()
                .filter(|e| include(e))
                .map(|e| get(e, "relation", "relates")),
        ),
        4,
    )
    .iter()
    .map(|(rel, n)| format!("{} x{n}", relation(rel, lang)))
    .collect::<Vec<_>>()
    .join(", ");
    if relations.is_empty() {
        relations = pick(
            lang,
            "未检测到高置信调用边",
            "No high-confidence call edges detected",
        )
        .into()
    }
    let note = if zh(lang) {
        format!(
            "本节由 Graphicle 社区聚类生成。关系概况：{relations}。图表优先展示高置信、跨节点调用或使用关系，完整节点清单位于表格中。"
        )
    } else {
        format!(
            "This section comes from Graphicle community clustering. Relationship summary: {relations}. The diagram prioritizes high-confidence calls or usage relationships; the table keeps the broader node inventory."
        )
    };
    render(
        "generate_section_cards:1521",
        &[
            pick(lang, "关键文件", "Key Files").into(),
            pick(lang, "覆盖节点", "Coverage").into(),
            rows,
            pick(lang, "设计备注", "Design Notes").into(),
            esc(&note),
        ],
    )
}
pub fn highlights(report: &str, lang: &str) -> String {
    if report.trim().is_empty() {
        return String::new();
    }
    let mut keep = vec![];
    let mut gods = false;
    let mut summary = false;
    let numbered = regex::Regex::new(r"^\d+\.").unwrap();
    for line in fsutil::split_lines(report) {
        let st = line.trim();
        if st.starts_with("## ") {
            summary = st == "## Summary";
            gods = st.starts_with("## God Nodes");
            continue;
        }
        if summary && st.starts_with("- ") {
            keep.push(&st[2..]);
        } else if gods && numbered.is_match(st) {
            keep.push(st);
        }
        if keep.len() >= 6 {
            break;
        }
    }
    if keep.is_empty() {
        return String::new();
    }
    render(
        "_report_highlights:1600",
        &[
            pick(lang, "图谱报告摘要", "Graph Report Highlights").into(),
            keep.iter()
                .map(|s| format!("      <li>{}</li>", esc(s)))
                .collect::<Vec<_>>()
                .join("\n"),
        ],
    )
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CallflowOptions {
    pub project: Option<String>,
    pub graph_dir: Option<String>,
    pub graph: Option<String>,
    pub report: Option<String>,
    pub labels: Option<String>,
    pub sections: Option<String>,
    pub output: Option<String>,
    pub lang: String,
    pub max_sections: i64,
    pub diagram_scale: f64,
    pub max_diagram_nodes: i64,
    pub max_diagram_edges: i64,
    pub verbose: bool,
}
impl Default for CallflowOptions {
    fn default() -> Self {
        Self {
            project: None,
            graph_dir: None,
            graph: None,
            report: None,
            labels: None,
            sections: None,
            output: None,
            lang: "auto".into(),
            max_sections: 15,
            diagram_scale: 1.0,
            max_diagram_nodes: 18,
            max_diagram_edges: 24,
            verbose: false,
        }
    }
}
#[derive(serde::Serialize)]
pub struct Output {
    pub path: PathBuf,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}
pub fn write_callflow_html(options: &CallflowOptions, now: Option<&str>) -> Result<Output> {
    let paths = resolve_paths(&serde_json::to_value(options).unwrap())?;
    let graph = get(&paths, "graph", "");
    if !Path::new(&graph).exists() {
        return Err(format!(
            "{program} output not found: {graph}. Run {program} first or pass --graph /path/to/graph.json.",
            program = crate::cli::program()
        ).into());
    }
    let data = load_graph(&graph, None)?;
    let nodes = ar(&data[0]);
    let edges = ar(&data[1]);
    let hyper = ar(&data[2]);
    let mut meta = data[3].clone();
    let labels = labels_data(read_json(paths["labels"].as_str(), json!({}))?);
    let lang = detect_lang(&options.lang, nodes, &labels);
    let sections = if paths["sections"].is_null() {
        derive_sections(nodes, &labels, &lang, options.max_sections)
    } else {
        ar(&load_sections(paths["sections"].as_str())?).to_vec()
    };
    let sections = normalize_sections(&sections, &lang);
    let report = load_report(paths["report"].as_str())?;
    if nodes.is_empty() {
        return Err("graph.json contains 0 nodes".into());
    }
    if sections.len() <= 1 {
        return Err("no sections defined".into());
    }
    let mut stderr = vec![];
    let mut stdout = vec![];
    if options.verbose && nodes.len() >= 5000 {
        stderr.push(
            "WARNING: Large graph -- Mermaid rendering may be slow. Consider --max-sections 5."
                .into(),
        )
    }
    let ids = nodes.iter().map(|n| s(&n["id"])).collect::<HashSet<_>>();
    let missing = edges
        .iter()
        .filter(|e| !ids.contains(&s(&e["source"])) || !ids.contains(&s(&e["target"])))
        .count();
    if options.verbose && missing > 0 {
        stderr.push(format!(
            "WARNING: {missing} edges reference nodes not present in graph.json."
        ));
    }
    meta["project_name"] = json!(infer_project_name(&graph, &meta)?);
    meta["node_count"] = json!(nodes.len());
    meta["edge_count"] = json!(edges.len());
    meta["hyperedge_count"] = json!(hyper.len());
    let output = if let Some(o) = options.output.as_deref().filter(|s| !s.is_empty()) {
        let o = expanduser(o)?;
        if o.is_absolute() {
            o
        } else {
            Path::new(paths["base"].as_str().unwrap()).join(o)
        }
    } else {
        Path::new(paths["graph_dir"].as_str().unwrap()).join(format!(
            "{}-callflow.html",
            safe_filename(&s(&meta["project_name"]), "project")
        ))
    };
    if options.verbose {
        stdout.push(format!(
            "Loaded: {} nodes, {} edges, {} sections",
            nodes.len(),
            edges.len(),
            sections.len()
        ));
        stdout.push(format!("Graph: {graph}"));
    }
    let idx = community_index(nodes);
    meta["community_count"] = json!(idx.as_object().unwrap().len());
    let map = section_node_map(&sections, &idx);
    let classified = classify(edges, &map);
    let mut html = vec![
        render(
            "write_callflow_html:1702",
            &[esc(&lang), esc(&doc_title(&meta, &lang)), s(&ASSETS["CSS"])],
        ),
        header(&sections, &meta, &lang),
        render(
            "write_callflow_html:1722",
            &[esc(&get(&sections[0], "name", "Architecture Overview"))],
        ),
        overview(&sections, &map, &classified, &lang, options.diagram_scale),
        "</div>\n".into(),
        overview_cards(&sections, &map, &classified, &lang),
    ];
    let card = highlights(&report, &lang);
    if !card.is_empty() {
        html.push(render("write_callflow_html:1733", &[card]));
    }
    html.push("<hr>".into());
    let mut num = 1;
    for sec in &sections {
        if sec["id"] == "overview" {
            continue;
        }
        num += 1;
        let sid = s(&sec["id"]);
        let name = get(sec, "name", &sid);
        let ns = ar(&map[&sid]);
        let es = ar(&classified["intra"][&sid]);
        html.push(render(
            "write_callflow_html:1756",
            &[
                num.to_string(),
                comment(&name),
                esc(&sid),
                num.to_string(),
                esc(&name),
                section_intro(sec, ns, es.len(), &lang),
                flowchart(
                    &sid,
                    &name,
                    ns,
                    es,
                    FlowchartOptions {
                        lang: &lang,
                        scale: options.diagram_scale,
                        max_nodes: options.max_diagram_nodes,
                        max_edges: options.max_diagram_edges,
                    },
                ),
                pick(&lang, "调用明细", "Call Details").into(),
                "#".into(),
                pick(&lang, "节点", "Node").into(),
                pick(&lang, "类型", "Type").into(),
                pick(&lang, "调用方", "Caller").into(),
                pick(&lang, "被调用/依赖", "Callees").into(),
                pick(&lang, "说明", "Description").into(),
                call_table(ns, es, &lang, Some(edges), Some(nodes)),
                section_cards(ns, es, &lang),
            ],
        ));
    }
    if !hyper.is_empty() {
        html.push(
            "<h2 id=\"hyperedges\">Group Relationships (Hyperedges)</h2>\n<div class=\"grid\">\n"
                .into(),
        );
        for he in hyper.iter().take(9) {
            let label = get(he, "label", &get(he, "id", "?"));
            let hn = ar(&he["nodes"]);
            html.push(render(
                "write_callflow_html:1791",
                &[
                    esc(&label),
                    esc(&get(he, "relation", "")),
                    hn.len().to_string(),
                ],
            ));
            for n in hn.iter().take(5) {
                html.push(format!("      <li><code>{}</code></li>", esc(&s(n))))
            }
            if hn.len() > 5 {
                html.push(format!("      <li>... and {} more</li>", hn.len() - 5));
            }
            html.push("    </ul>\n  </div>".into());
        }
        html.push("</div>\n<hr>".into());
    }
    html.push(render(
        "write_callflow_html:1804",
        &[
            nodes.len().to_string(),
            edges.len().to_string(),
            hyper.len().to_string(),
            idx.as_object().unwrap().len().to_string(),
            sections
                .iter()
                .filter(|s| s["id"] != "overview")
                .count()
                .to_string(),
            edges
                .iter()
                .filter(|e| e["confidence"] == "EXTRACTED")
                .count()
                .to_string(),
            edges
                .iter()
                .filter(|e| e["confidence"] == "INFERRED")
                .count()
                .to_string(),
            edges
                .iter()
                .filter(|e| e["confidence"] == "AMBIGUOUS")
                .count()
                .to_string(),
        ],
    ));
    let time = now
        .map(str::to_owned)
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string());
    html.push(render(
        "write_callflow_html:1829",
        &[esc(&get(&meta, "project_name", "Project")), time],
    ));
    html.push(s(&ASSETS["closing"]));
    let html = html.join("\n");
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(crate::Error::from)?;
    }
    std::fs::write(&output, &html).map_err(crate::Error::from)?;
    if options.verbose {
        stdout.push(format!(
            "Call-flow HTML written: {}",
            fsutil::path_string(&output)
        ));
        stdout.push(format!(
            "  Sections: {}  |  Mermaid diagrams: {}  |  Call tables: {}",
            html.matches("<h2 id=").count(),
            html.matches("<div class=\"mermaid\">").count(),
            html.matches("<table class=\"call-table\">").count()
        ));
        stdout.push(
            "  Diagrams use Mermaid init directives plus interactive zoom/pan controls.".into(),
        );
    }
    Ok(Output {
        path: PathBuf::from(fsutil::path_string(&output)),
        stdout,
        stderr,
    })
}
/// CLI entry for this module; the package's broad command router is a separate port.
pub fn cli(args: &[String]) -> Result<Output> {
    let mut o = CallflowOptions::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') {
            if o.project.is_some() {
                return Err(format!("unexpected argument: {arg}").into());
            }
            o.project = Some(arg.clone());
            i += 1;
            continue;
        }
        let (key, value) = if let Some((a, b)) = arg.split_once('=') {
            (a.to_string(), b.to_string())
        } else {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("{arg}: expected one argument"))?
                .clone();
            i += 1;
            (arg.clone(), v)
        };
        match key.as_str() {
            "--graph-dir" => o.graph_dir = Some(value),
            "--graph" => o.graph = Some(value),
            "--report" => o.report = Some(value),
            "--labels" => o.labels = Some(value),
            "--sections" => o.sections = Some(value),
            "--output" => o.output = Some(value),
            "--lang" => o.lang = value,
            "--max-sections" => {
                o.max_sections = value.parse().map_err(|_| "invalid max-sections")?
            }
            "--diagram-scale" => {
                o.diagram_scale = value.parse().map_err(|_| "invalid diagram-scale")?
            }
            "--max-diagram-nodes" => {
                o.max_diagram_nodes = value.parse().map_err(|_| "invalid max-diagram-nodes")?
            }
            "--max-diagram-edges" => {
                o.max_diagram_edges = value.parse().map_err(|_| "invalid max-diagram-edges")?
            }
            _ => return Err(format!("unrecognized argument: {key}").into()),
        }
        i += 1;
    }
    o.verbose = true;
    write_callflow_html(&o, None)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let name = r["function"].as_str().unwrap_or("");
    let args = ar(&r["args"]);
    let a = |i: usize| args.get(i).unwrap_or(&Value::Null);
    let st = |i| text(a(i));
    let arr = |i| ar(a(i));
    let int = |i, default| a(i).as_i64().unwrap_or(default);
    let b = |i| truth(a(i));
    let fl = |i, default| float(a(i), default);
    Ok(match name{
        "first_present"=>first(a(0),&arr(1).iter().map(|v|v.as_str().unwrap()).collect::<Vec<_>>(),a(2).clone()),"first_list"=>json!(first_list(&args.iter().collect::<Vec<_>>())),"to_float"=>json!(fl(0,fl(1,0.0))),"endpoint_id"=>json!(endpoint(a(0))),"normalize_node"=>normalize_node(a(0),int(1,0) as usize),"normalize_edge"=>json!(normalize_edge(a(0),int(1,0) as usize)),"_node_link_payload"=>json!(node_link(a(0))),"read_json"=>read_json(a(0).as_str(),a(1).clone())?,"load_graph"=>load_graph(&st(0),r["cap"].as_u64())?,"load_data"=>load_data(a(0),&st(1))?,"load_labels"=>labels_data(read_json(a(0).as_str(),json!({}))?),"load_sections"=>load_sections(a(0).as_str())?,"load_report"=>json!(load_report(a(0).as_str())?),
        "safe_mermaid_text"=>json!(safe_mermaid(&st(0))),"html_comment_text"=>json!(comment(&st(0))),"stable_ascii_id"=>json!(stable_ascii_id(&st(0),a(1).as_str().unwrap_or("node"),int(2,48))),"node_mermaid_id"=>json!(node_id(a(0))),"mermaid_section_id"=>json!(section_id(&st(0))),"safe_file_path"=>json!(short_path(&st(0))),"safe_filename"=>json!(safe_filename(&st(0),a(1).as_str().unwrap_or("project"))),"infer_project_name"=>json!(infer_project_name(&st(0),a(1))?),"resolve_output_paths"=>resolve_paths(a(0))?,"is_zh"=>json!(zh(&st(0))),"pick_text"=>json!(pick(&st(0),&st(1),&st(2))),"detect_lang"=>json!(detect_lang(&st(0),arr(1),a(2))),"truncate_text"=>json!(truncate(&st(0),int(1,0))),"humanize_label"=>json!(humanize(&st(0),&st(1))),"node_kind"=>json!(kind(a(0))),"relation_label"=>json!(relation(&st(0),&st(1))),"preferred_edges"=>json!(preferred(arr(0),b(1))),"edge_score"=>json!(edge_score(a(0))),"mermaid_init"=>json!(mermaid_init(fl(0,1.0),a(1).as_str().unwrap_or("LR"))),"mermaid_class_defs"=>json!(classes()),"build_community_index"=>community_index(arr(0)),
        "html_anchor_id"=>{let mut used=arr(2).iter().map(s).collect::<HashSet<_>>();let id=anchor(&st(0),&st(1),&mut used);let mut used=used.into_iter().collect::<Vec<_>>();used.sort();json!([id,used])},"normalize_communities"=>normalize_communities(a(0)),"normalize_sections"=>json!(normalize_sections(arr(0),&st(1))),"label_for_community"=>json!(label_community(&st(0),a(1),arr(2),&st(3))),"_community_text"=>json!(community_text(arr(0),&st(1))),"_keyword_score"=>json!(keyword_score(&st(0),arr(1))),"_rank_grouped_sections"=>json!(ranked_sections(a(0),int(1,15))),"derive_sections_from_communities"=>json!(derive_sections(arr(0),a(1),&st(2),int(3,15))),"build_section_node_map"=>section_node_map(arr(0),a(1)),"node_in_section"=>json!(arr(1).contains(a(0))),"classify_edges"=>classify(arr(0),a(1)),"should_include_edge"=>json!(include(a(0))),"node_degree_scores"=>degree_scores(arr(0)),"node_importance"=>json!(importance(a(0))),"select_diagram_nodes"=>json!(select_nodes(arr(0),arr(1),int(2,18))),"node_label"=>json!(node_label(a(0))),"group_nodes_by_file"=>group_files(arr(0)),"section_edge_summary"=>json!(edge_summary(a(0)).iter().map(|r|json!([[r.source,r.target],{"count":r.count,"relations":r.relations.iter().map(|(k,v)|(k.clone(),json!(v))).collect::<Map<_,_>>()}])).collect::<Vec<_>>()),
        "generate_overview_graph"=>json!(overview(arr(0),a(1),a(2),&st(4),fl(5,1.0))),"generate_section_flowchart"=>json!(flowchart(&st(0),&st(1),arr(2),arr(3),FlowchartOptions { lang: &st(4), scale: fl(5,1.0), max_nodes: int(6,18), max_edges: int(7,24) })),"generate_nav"=>json!(nav(arr(0))),"node_display_name"=>json!(display_name(a(0),&st(1))),"format_node_refs"=>json!(node_refs(&arr(0).iter().map(s).collect::<Vec<_>>(),a(1),&st(2),&st(3),int(4,3))),"generate_call_table_rows"=>json!(call_table(arr(0),arr(1),&st(2),a(3).as_array().map(Vec::as_slice),a(4).as_array().map(Vec::as_slice))),"_suggest_tag"=>json!(suggest_tag(&st(0),&st(1),&st(2),&st(3))),"_describe_node"=>json!(describe(&st(0),&st(1),&st(2),&st(3))),"generate_header"=>json!(header(arr(0),a(1),&st(2))),"derive_flow_chain"=>json!(flow_chain(arr(0),a(1))),"generate_overview_cards"=>json!(overview_cards(arr(2),a(3),a(4),&st(5))),"section_keywords"=>json!(keywords(arr(0),int(1,5).max(0) as usize)),"generate_section_intro"=>json!(section_intro(a(0),arr(1),int(2,0) as usize,&st(3))),"generate_section_cards"=>json!(section_cards(arr(1),arr(2),&st(3))),"_report_highlights"=>json!(highlights(&st(0),&st(1))),"write_callflow_html"=>serde_json::to_value(write_callflow_html(&serde_json::from_value(a(0).clone()).map_err(|e|e.to_string())?,r["now"].as_str())?).unwrap(),"main"=>serde_json::to_value(cli(&arr(0).iter().map(s).collect::<Vec<_>>())?).unwrap(),_=>return Err(format!("unknown callflow function: {name}").into())})
}
