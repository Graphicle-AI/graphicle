// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/exporters/html.py
//! Interactive graph view. Browser HTML/JS/CSS remains in its native format;
//! graph assembly, aggregation, annotation and output are implemented in Rust.
use super::{base::COMMUNITY_COLORS, common::*};
use crate::{
    Result, analyze, build::Diagnostics, cluster::Communities, fsutil, graph::Graph, path_syntax,
    paths, reflect, security,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
pub const MAX_NODES_FOR_VIZ: i64 = 5000;
pub const HTML_STALE_MARKER: &str = ".graph.html.stale";
pub fn viz_node_limit() -> i64 {
    bounded_integer(&viz_node_limit_value())
}
fn bounded_integer(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| {
        if v.to_string().starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}
fn viz_node_limit_value() -> Value {
    let Some(raw) = std::env::var("GRAPHICLE_VIZ_NODE_LIMIT").ok() else {
        return json!(MAX_NODES_FOR_VIZ);
    };
    let raw = raw.trim_matches(crate::dedup_strings::whitespace);
    let (negative, body) = if let Some(s) = raw.strip_prefix('-') {
        (true, s)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(raw))
    };
    let mut ascii = String::new();
    let mut digit = false;
    for c in body.chars() {
        if c == '_' && digit {
            ascii.push(c);
            digit = false;
            continue;
        }
        let cp = c as u32;
        let Some((start, _)) = crate::decimal_data::DECIMAL
            .iter()
            .find(|(a, b)| *a <= cp && cp <= *b)
        else {
            return json!(MAX_NODES_FOR_VIZ);
        };
        ascii.push(char::from(b'0' + ((cp - start) % 10) as u8));
        digit = true;
    }
    if !digit {
        return json!(MAX_NODES_FOR_VIZ);
    }
    let digits = ascii.replace('_', "");
    let digits = digits.trim_start_matches('0');
    let n = if digits.is_empty() {
        "0".into()
    } else if negative {
        format!("-{digits}")
    } else {
        digits.to_owned()
    };
    serde_json::from_str(&n).unwrap_or(json!(MAX_NODES_FOR_VIZ))
}
pub fn html_styles() -> &'static str {
    include_str!("../assets/export_html/styles.html")
}
pub fn hyperedge_script(json: &str) -> String {
    include_str!("../assets/export_html/hyperedges.html.template").replace("{{HYPEREDGES}}", json)
}
pub fn html_script(nodes: &str, edges: &str, legend: &str) -> String {
    // Single pass keeps placeholder-looking source strings literal.
    render(
        include_str!("../assets/export_html/script.html.template"),
        &[
            ("{{NODES}}", nodes),
            ("{{EDGES}}", edges),
            ("{{LEGEND}}", legend),
        ],
    )
}
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = template;
    loop {
        let next = values
            .iter()
            .filter_map(|(key, v)| rest.find(key).map(|at| (at, *key, *v)))
            .min_by_key(|(at, _, _)| *at);
        let Some((at, key, value)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        out.push_str(value);
        rest = &rest[at + key.len()..];
    }
    out
}
pub fn html_document_title(output: &str) -> String {
    let mut raw = output.replace('\\', "/");
    if raw.as_bytes().get(1) == Some(&b':')
        && raw.as_bytes().get(2) == Some(&b'/')
        && raw.as_bytes()[0].is_ascii_alphabetic()
    {
        raw = raw[2..].into()
    }
    let parts = path_syntax::posix_parts(&raw);
    // Upstream's prefix test ("graphify-out") cannot match its configuration
    // folder; the configuration folder's name starts with the output folder's
    // here, so that one name is excluded.
    let output_folder = |part: &str| {
        part == *paths::OUT_DIR_NAME
            || (part.starts_with(paths::DEFAULT_OUT_DIR) && part != paths::CONFIG_DIR)
    };
    for (i, part) in parts.iter().enumerate() {
        if output_folder(part.as_str()) {
            return parts[i..].join("/");
        }
    }
    if let (Ok(target), Ok(cwd)) = (
        fsutil::resolve_missing(Path::new(&raw)),
        std::env::current_dir().and_then(|p| fsutil::resolve_missing(&p)),
    ) && let Ok(rel) = target.strip_prefix(cwd)
    {
        let s = rel.to_string_lossy().replace('\\', "/");
        if !s.is_empty() && s != "." {
            return s;
        }
    }
    let name = path_syntax::posix_name(&parts);
    if name.is_empty() {
        "graph.html".into()
    } else {
        name.into()
    }
}
#[derive(Default)]
pub struct HtmlOptions {
    pub community_labels: BTreeMap<i64, Value>,
    pub member_counts: BTreeMap<i64, f64>,
    pub node_limit: Option<i64>,
    pub learning_overlay: Option<Value>,
}
pub fn to_html(
    graph: &Graph,
    communities: &Communities,
    output: &Path,
    options: &HtmlOptions,
    diagnostics: &mut Diagnostics,
) -> Result<bool> {
    let limit = options.node_limit.unwrap_or_else(viz_node_limit);
    let assign = assignments(graph, communities);
    let labels = &options.community_labels;
    if graph.nodes.len() as i64 > limit {
        if options.node_limit.is_none() {
            return Err(format!(
                "Graph has {} nodes - too large for HTML viz (limit: {limit}). Use --no-viz, raise GRAPHICLE_VIZ_NODE_LIMIT, or reduce input size.",
                graph.nodes.len()
            ).into());
        }
        diagnostics.stdout.push(format!(
            "Graph has {} nodes (above {limit} limit). Building aggregated community view...",
            graph.nodes.len()
        ));
        let mut meta = Graph::new(false, false);
        for (cid, _) in communities {
            meta.add_node(
                json!(cid.to_string()),
                json!({"label":community_name(Some(*cid),labels)})
                    .as_object()
                    .unwrap()
                    .clone(),
            )?;
        }
        // In the order each community pair first appears, as upstream's
        // Counter keeps it (an index keeps the count lookup constant-time).
        let mut counts: Vec<((i64, i64), usize)> = vec![];
        let mut slots = rustc_hash::FxHashMap::<(i64, i64), usize>::default();
        for (u, v, _) in graph.iter_edges() {
            if let (Some(cu), Some(cv)) = (assign[u], assign[v])
                && cu != cv
            {
                let key = (cu.min(cv), cu.max(cv));
                let slot = *slots.entry(key).or_insert_with(|| {
                    counts.push((key, 0));
                    counts.len() - 1
                });
                counts[slot].1 += 1;
            }
        }
        for ((u, v), n) in counts {
            meta.add_edge(json!(u.to_string()),json!(v.to_string()),None,json!({"weight":n,"relation":format!("{n} cross-community edges"),"confidence":"AGGREGATED"}).as_object().unwrap().clone())?;
        }
        if meta.nodes.len() <= 1 {
            diagnostics
                .stdout
                .push("Single community - aggregated view not useful. Skipping graph.html.".into());
            return Ok(false);
        }
        let meta_communities = communities
            .iter()
            .map(|(c, _)| (*c, vec![json!(c.to_string())]))
            .collect::<Communities>();
        if let Some(hypers) = graph.attributes["hyperedges"]
            .as_array()
            .filter(|v| !v.is_empty())
        {
            let pairs = analyze::node_community_map(communities);
            // The first pair with each key, as the linear search it replaces
            // found it.
            let mut community_of = std::collections::BTreeMap::new();
            for (n, c) in &pairs {
                community_of
                    .entry(crate::python_compat::scalar_key(n))
                    .or_insert(*c);
            }
            let mut remapped = vec![];
            for he in hypers {
                let mut ids = vec![];
                let mut seen = BTreeSet::new();
                if let Some(members) = he["nodes"].as_array() {
                    for id in members {
                        if let Some(c) = community_of.get(&crate::python_compat::scalar_key(id)) {
                            let c = c.to_string();
                            if seen.insert(c.clone()) {
                                ids.push(c)
                            }
                        }
                    }
                }
                if ids.len() >= 2 {
                    let label = he
                        .get("label")
                        .filter(|v| truth(v))
                        .cloned()
                        .unwrap_or_else(|| {
                            json!(he["relation"].as_str().unwrap_or("").replace('_', " "))
                        });
                    remapped.push(
                        json!({"id":he.get("id").unwrap_or(&json!("")),"label":label,"nodes":ids}),
                    );
                }
            }
            meta.attributes["hyperedges"] = json!(remapped);
        }
        let opts = HtmlOptions {
            community_labels: labels.clone(),
            member_counts: communities
                .iter()
                .map(|(c, m)| (*c, m.len() as f64))
                .collect(),
            ..Default::default()
        };
        if !to_html(&meta, &meta_communities, output, &opts, diagnostics)? {
            return Ok(false);
        }
        diagnostics.stdout.push(format!(
            "graph.html written (aggregated: {} community nodes, {} cross-community edges)",
            meta.nodes.len(),
            meta.edge_count()
        ));
        diagnostics
            .stdout
            .push("Tip: run with --obsidian for full node-level detail.".into());
        return Ok(true);
    }
    let degree = (0..graph.nodes.len())
        .map(|i| analyze::degree(graph, i))
        .collect::<Vec<_>>();
    let max_deg = degree.iter().copied().max().unwrap_or(1).max(1) as f64;
    let mut max_mc = options
        .member_counts
        .values()
        .copied()
        .reduce(f64::max)
        .unwrap_or(1.0);
    if max_mc == 0.0 {
        max_mc = 1.0
    }
    let overlay = options
        .learning_overlay
        .clone()
        .unwrap_or_else(|| reflect::load_learning_overlay(output).unwrap_or_else(|_| json!({})));
    let mut vis_nodes = vec![];
    for (i, n) in graph.nodes.iter().enumerate() {
        let cid = assign[i].unwrap_or(0);
        let color = COMMUNITY_COLORS[cid.rem_euclid(COMMUNITY_COLORS.len() as i64) as usize];
        let label = security::sanitize_label(n.attributes.get("label").unwrap_or(&n.id));
        let deg = degree[i];
        let (size, font_size) = if !options.member_counts.is_empty() {
            (
                10.0 + 30.0 * options.member_counts.get(&cid).unwrap_or(&1.0) / max_mc,
                12,
            )
        } else {
            (
                10.0 + 30.0 * (deg as f64 / max_deg),
                if deg as f64 >= max_deg * 0.15 { 12 } else { 0 },
            )
        };
        let size = format!("{size:.1}")
            .parse::<f64>()
            .map_err(crate::Error::from)?;
        let source = n
            .attributes
            .get("source_file")
            .filter(|v| truth(v))
            .map(string)
            .unwrap_or_default();
        let mut node = json!({"id":n.id,"label":label,"color":{"background":color,"border":color,"highlight":{"background":"#ffffff","border":color}},"size":size,"font":{"size":font_size,"color":"#ffffff"},"title":label,"community":cid,"community_name":security::sanitize_label(&community_name(Some(cid),labels)),"source_file":security::sanitize_label(&json!(source)),"file_type":n.attributes.get("file_type").unwrap_or(&json!("")),"degree":deg});
        // Aggregated view: the Node Info panel shows a meta node's member count.
        if !options.member_counts.is_empty() {
            node["member_count"] = options
                .member_counts
                .get(&cid)
                .map(|v| json!(*v as i64))
                .unwrap_or_else(|| {
                    json!(
                        communities
                            .iter()
                            .find(|(c, _)| *c == cid)
                            .map_or(0, |(_, m)| m.len())
                    )
                });
        }
        if let Some(entry) = overlay.get(string(&n.id)).filter(|v| truth(v)) {
            let status =
                security::sanitize_label(&json!(string(entry.get("status").unwrap_or(&json!("")))));
            let stale = truth(&entry["stale"]);
            node["learning_status"] = json!(status);
            node["learning_stale"] = json!(stale);
            let ring = match status.as_str() {
                "preferred" => Some("#22c55e"),
                "contested" => Some("#f59e0b"),
                _ => None,
            };
            if let Some(mut ring) = ring {
                if stale {
                    ring = "#9ca3af";
                    node["shapeProperties"] = json!({"borderDashes":[4,4]});
                }
                node["borderWidth"] = json!(3);
                node["color"] = json!({"background":color,"border":ring,"highlight":{"background":"#ffffff","border":ring}});
            }
            let uses = string(entry.get("uses").unwrap_or(&json!(0)));
            let mut lesson = match status.as_str() {
                "contested" => format!(
                    "Lesson: contested (useful {uses} / dead-end {})",
                    string(entry.get("neg").unwrap_or(&json!(0)))
                ),
                "preferred" => format!(
                    "Lesson: preferred source ({uses} useful, score={})",
                    string(entry.get("score").unwrap_or(&json!(0)))
                ),
                _ => format!("Lesson: {status} ({uses} useful)"),
            };
            if stale {
                lesson.push_str(" [code changed — re-verify]")
            };
            node["title"] = json!(format!(
                "{label}\n{}",
                security::sanitize_label(&json!(lesson))
            ));
        }
        vis_nodes.push(node)
    }
    let mut vis_edges = vec![];
    for (u, v, e) in graph.iter_edges() {
        let d = &e.attributes;
        let confidence = d.get("confidence").cloned().unwrap_or(json!("EXTRACTED"));
        let relation = d.get("relation").cloned().unwrap_or(json!(""));
        let extracted = confidence == "EXTRACTED";
        vis_edges.push(json!({"from":d.get("_src").unwrap_or(&graph.nodes[u].id),"to":d.get("_tgt").unwrap_or(&graph.nodes[v].id),"label":relation,"title":security::sanitize_label(&json!(format!("{} [{}]",string(&relation),string(&confidence)))),"dashes":!extracted,"width":if extracted{2}else{1},"color":{"opacity":if extracted{0.7}else{0.35}},"confidence":confidence}));
    }
    let legend=labels.keys().map(|cid|{
        let count=communities.iter().find(|(c,_)|c==cid).map(|(_,m)|m.len()).unwrap_or(0);
        let n=options.member_counts.get(cid).map(|v|json!(*v as i64)).unwrap_or(json!(count));
        json!({"cid":cid,"color":COMMUNITY_COLORS[cid.rem_euclid(10) as usize],"label":escape(&security::sanitize_label(&community_name(Some(*cid),labels))),"count":n})
    }).collect::<Vec<_>>();
    let nodes = js_safe(&json!(vis_nodes))?;
    let edges = js_safe(&json!(vis_edges))?;
    let legends = js_safe(&json!(legend))?;
    let hypers = js_safe(graph.attributes.get("hyperedges").unwrap_or(&json!([])))?;
    let title = escape(&security::sanitize_label(&json!(html_document_title(
        &output.to_string_lossy()
    ))));
    let stats = format!(
        "{} nodes &middot; {} edges &middot; {} communities",
        graph.nodes.len(),
        graph.edge_count(),
        communities.len()
    );
    let script = html_script(&nodes, &edges, &legends);
    let hyper_script = hyperedge_script(&hypers);
    let page = render(
        include_str!("../assets/export_html/document.html.template"),
        &[
            ("{{TITLE}}", &title),
            ("{{STATS}}", &stats),
            ("{{STYLES}}", html_styles()),
            ("{{SCRIPT}}", &script),
            ("{{HYPEREDGE_SCRIPT}}", &hyper_script),
        ],
    );
    paths::write_text_atomic(output, &page).map_err(crate::Error::from)?;
    Ok(true)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |k: &str| r[k].as_str().unwrap_or("");
    let mut d = Diagnostics::default();
    let value = match s("function") {
        "_viz_node_limit" => viz_node_limit_value(),
        "_html_styles" => json!(html_styles()),
        "_html_document_title" => json!(html_document_title(s("output_path"))),
        "_hyperedge_script" => json!(hyperedge_script(s("hyperedges_json"))),
        "_html_script" => json!(html_script(
            s("nodes_json"),
            s("edges_json"),
            s("legend_json")
        )),
        "to_html" => {
            let graph = Graph::from_node_link(&r["G"], "edges")?;
            let options = HtmlOptions {
                community_labels: labels(&r["community_labels"])?,
                member_counts: labels(&r["member_counts"])?
                    .into_iter()
                    .map(|(c, n)| (c, n.as_f64().unwrap_or(1.0)))
                    .collect(),
                node_limit: r
                    .get("node_limit")
                    .filter(|v| !v.is_null())
                    .map(bounded_integer),
                learning_overlay: r.get("learning_overlay").filter(|v| !v.is_null()).cloned(),
            };
            json!(to_html(
                &graph,
                &communities(r)?,
                Path::new(s("output_path")),
                &options,
                &mut d
            )?)
        }
        _ => return Err("Unknown HTML export operation".into()),
    };
    Ok(json!({"value":value,"stdout":d.stdout,"stderr":d.stderr}))
}
#[cfg(test)]
mod tests {
    use super::html_document_title;

    /// The title keeps the output folder and what follows it. The
    /// configuration folder (`~/.graphicle-graph`) is not an output folder,
    /// although its name starts with the default output folder's; another
    /// folder named like the output folder still is, as upstream's prefix
    /// test matches `graphify-out-old`.
    #[test]
    fn a_path_under_the_configuration_folder_keeps_its_title() {
        assert_eq!(
            html_document_title(
                "/home/user/.graphicle-graph/repos/owner/repo/.graphicle/graph.html"
            ),
            ".graphicle/graph.html"
        );
        assert_eq!(
            html_document_title("/home/user/.graphicle-graph/global/graph.html"),
            "graph.html"
        );
        assert_eq!(
            html_document_title("/home/user/project/.graphicle-old/graph.html"),
            ".graphicle-old/graph.html"
        );
    }
}
