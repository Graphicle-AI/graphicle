// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/wiki.py
//! Graph wiki articles, source audits, portable links and complete exports.

#[derive(Clone, Copy)]
pub struct ArticleContext<'a> {
    pub labels: &'a Map<String, Value>,
    pub mapping: &'a [Option<String>],
    pub resolver: &'a Map<String, Value>,
}
use crate::{Result, analyze, graph::Graph, graph_algorithms, paths, python_compat};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::Path,
};
pub const SLUG_SUFFIX_RESERVE: i64 = 5;
type Communities = Vec<(String, Vec<usize>)>;
fn text(v: &Value) -> Result<&str> {
    v.as_str()
        .ok_or_else(|| "TypeError: expected string".into())
}
fn display(v: &Value) -> String {
    python_compat::string(v)
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn object(v: &Value) -> Result<&Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| "TypeError: expected mapping".into())
}
fn optional_object(v: Option<&Value>) -> Result<Map<String, Value>> {
    v.filter(|v| truthy(v))
        .map(|v| object(v).cloned())
        .transpose()
        .map(|m| m.unwrap_or_default())
}
fn label(g: &Graph, n: usize) -> String {
    g.nodes[n]
        .attributes
        .get("label")
        .map(display)
        .unwrap_or_else(|| display(&g.nodes[n].id))
}
fn community_label(labels: &Map<String, Value>, cid: &str) -> Result<String> {
    labels
        .get(cid)
        .map(|v| text(v).map(str::to_string))
        .unwrap_or_else(|| Ok(format!("Community {cid}")))
}
pub fn safe_filename(name: &str, limit: i64) -> String {
    let s = name
        .replace('/', "-")
        .replace(' ', "_")
        .replace(':', "-")
        .chars()
        .filter(|c| !matches!(c, '(' | ')'))
        .map(|c| {
            if "<>:\"/\\|?*#%".contains(c) || c <= '\x1f' || c == '\x7f' {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    let s = s.trim_matches(['.', ' ']);
    if s.is_empty() {
        return "unnamed".into();
    }
    let n = if limit < 0 {
        s.chars()
            .count()
            .saturating_sub(limit.unsigned_abs() as usize)
    } else {
        limit as usize
    };
    s.chars().take(n).collect()
}
pub fn md_link(label: &str, resolver: &Map<String, Value>) -> Result<String> {
    Ok(md_link_to(
        label,
        resolver
            .get(label)
            .filter(|v| !v.is_null())
            .map(display)
            .as_deref(),
    ))
}
/// `[label](slug.md)` for an already-known slug; plain text when there is no
/// article. The index links each row by the slug its article was written
/// under, not by a label lookup that collapses same-labelled articles onto
/// the first one registered (#3032/#3033, Graphify 0.9.88).
pub fn md_link_to(label: &str, slug: Option<&str>) -> String {
    let shown = label.replace('[', "\\[").replace(']', "\\]");
    match slug {
        Some(slug) => format!("[{shown}]({slug}.md)"),
        None => shown,
    }
}
fn node_mapping(g: &Graph, v: Option<&Value>) -> Result<Vec<Option<String>>> {
    let mut out = vec![None; g.nodes.len()];
    if let Some(v) = v.filter(|v| truthy(v)) {
        for (k, cid) in object(v)? {
            if let Some(i) = g.node_index(&json!(k))
                && !cid.is_null()
            {
                out[i] = Some(display(cid));
            }
        }
    }
    Ok(out)
}
pub fn cross_community_links(
    g: &Graph,
    nodes: &[usize],
    cid: &str,
    labels: &Map<String, Value>,
    mapping: &[Option<String>],
) -> Result<Vec<(String, usize)>> {
    let adj = graph_algorithms::neighbors(g);
    let mut counts: Vec<(String, usize)> = Vec::new();
    for &node in nodes {
        for &other in &adj[node] {
            if let Some(other_cid) = &mapping[other]
                && other_cid != cid
            {
                let lab = community_label(labels, other_cid)?;
                if let Some((_, n)) = counts.iter_mut().find(|(l, _)| *l == lab) {
                    *n += 1;
                } else {
                    counts.push((lab, 1));
                }
            }
        }
    }
    counts.sort_by_key(|item| std::cmp::Reverse(item.1));
    Ok(counts)
}
pub fn community_article(
    g: &Graph,
    cid: &str,
    nodes: &[usize],
    name: &str,
    context: ArticleContext<'_>,
    cohesion: Option<f64>,
) -> Result<String> {
    let ArticleContext {
        labels,
        mapping,
        resolver,
    } = context;

    let mut top = nodes.to_vec();
    top.sort_by_key(|n| std::cmp::Reverse(analyze::degree(g, *n)));
    top.truncate(25);
    let cross = cross_community_links(g, nodes, cid, labels, mapping)?;
    let members = nodes.iter().copied().collect::<HashSet<_>>();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (u, v, e) in g.iter_edges() {
        if members.contains(&u) || (!g.directed && members.contains(&v)) {
            let conf = e
                .attributes
                .get("confidence")
                .map(display)
                .unwrap_or("EXTRACTED".into());
            *counts.entry(conf).or_default() += 1;
        }
    }
    let total = counts.values().sum::<usize>().max(1);
    let mut sources = BTreeSet::new();
    for &n in nodes {
        if let Some(v) = g.nodes[n]
            .attributes
            .get("source_file")
            .filter(|v| truthy(v))
        {
            sources.insert(text(v)?.to_string());
        }
    }
    let mut lines = vec![
        format!("# {name}"),
        "".into(),
        format!(
            "> {} nodes{}",
            nodes.len(),
            cohesion
                .map(|c| format!(" · cohesion {c:.2}"))
                .unwrap_or_default()
        ),
        "".into(),
        "## Key Concepts".into(),
        "".into(),
    ];
    for &n in &top {
        let src = g.nodes[n].attributes.get("source_file");
        lines.push(format!(
            "- **{}** ({} connections){}",
            label(g, n),
            analyze::degree(g, n),
            src.filter(|v| truthy(v))
                .map(|v| format!(" — `{}`", display(v)))
                .unwrap_or_default()
        ));
    }
    if nodes.len() > top.len() {
        lines.push(format!(
            "- *... and {} more nodes in this community*",
            nodes.len() - top.len()
        ));
    }
    lines.extend(["".into(), "## Relationships".into(), "".into()]);
    if cross.is_empty() {
        lines.push("- No strong cross-community connections detected".into());
    } else {
        for (l, n) in cross.into_iter().take(12) {
            lines.push(format!(
                "- {} ({n} shared connections)",
                md_link(&l, resolver)?
            ));
        }
    }
    lines.push("".into());
    if !sources.is_empty() {
        lines.extend(["## Source Files".into(), "".into()]);
        lines.extend(sources.iter().take(20).map(|s| format!("- `{s}`")));
        lines.push("".into());
    }
    lines.extend(["## Audit Trail".into(), "".into()]);
    for conf in ["EXTRACTED", "INFERRED", "AMBIGUOUS"] {
        let n = counts.get(conf).copied().unwrap_or(0);
        let pct = (n as f64 / total as f64 * 100.0).round_ties_even() as i64;
        lines.push(format!("- {conf}: {n} ({pct}%)"));
    }
    lines.extend(["".into(), "---".into(), "".into(), footer()]);
    Ok(lines.join("\n"))
}
/// The navigation footer links the catalog by its fixed filename, not through
/// the label resolver, so an article whose own label is "index" resolves to its
/// own (deduplicated) file instead of the catalog (0.9.72, #3818).
fn footer() -> String {
    "*Part of the Graphicle knowledge wiki. See [index](index.md) to navigate.*".into()
}
pub fn god_node_article(
    g: &Graph,
    n: usize,
    labels: &Map<String, Value>,
    mapping: &[Option<String>],
    resolver: &Map<String, Value>,
) -> Result<String> {
    let src = g.nodes[n]
        .attributes
        .get("source_file")
        .map(display)
        .unwrap_or_default();
    let mut lines = vec![
        format!("# {}", label(g, n)),
        "".into(),
        format!(
            "> God node · {} connections · `{src}`",
            analyze::degree(g, n)
        ),
        "".into(),
    ];
    if let Some(cid) = &mapping[n] {
        let name = community_label(labels, cid)?;
        if !name.is_empty() {
            lines.extend([
                format!("**Community:** {}", md_link(&name, resolver)?),
                "".into(),
            ]);
        }
    }
    let mut by_relation: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut neighbors = graph_algorithms::neighbors(g)[n].clone();
    neighbors.sort_by_key(|n| std::cmp::Reverse(analyze::degree(g, *n)));
    for other in neighbors {
        let edges = g.edges_between(&g.nodes[n].id, &g.nodes[other].id);
        let edge = edges.first().ok_or("Missing neighbor edge")?;
        let relation = edge
            .attributes
            .get("relation")
            .map(display)
            .unwrap_or("related".into());
        let conf = edge.attributes.get("confidence");
        let suffix = conf
            .filter(|v| truthy(v))
            .map(|c| format!(" `{}`", display(c)))
            .unwrap_or_default();
        by_relation
            .entry(relation)
            .or_default()
            .push(format!("{}{suffix}", md_link(&label(g, other), resolver)?));
    }
    lines.extend(["## Connections by Relation".into(), "".into()]);
    for (rel, targets) in by_relation {
        lines.push(format!("### {rel}"));
        lines.extend(targets.iter().take(20).map(|t| format!("- {t}")));
        if targets.len() > 20 {
            lines.push(format!(
                "- *…and {} more `{rel}` connection(s) not listed (lowest-degree first to go)*",
                targets.len() - 20
            ));
        }
        lines.push("".into());
    }
    lines.extend(["---".into(), "".into(), footer()]);
    Ok(lines.join("\n"))
}
/// `community_slugs` is keyed by community id, `god_articles` holds `(node id, slug)`
/// in write order (a later duplicate node id wins, like Python's `dict(god_articles)`).
pub fn index_md(
    communities: &[(String, usize)],
    labels: &Map<String, Value>,
    gods: &[Value],
    total_nodes: usize,
    total_edges: usize,
    community_slugs: &Map<String, Value>,
    god_articles: &[(Value, String)],
) -> Result<String> {
    let god_slug = |id: Option<&Value>| -> Option<&str> {
        let id = id?;
        god_articles
            .iter()
            .rev()
            .find(|(nid, _)| nid == id)
            .map(|(_, slug)| slug.as_str())
    };
    let mut lines=vec!["# Knowledge Graph Index".into(),"".into(),"> Auto-generated by Graphicle. Start here — read community articles for context, then drill into god nodes for detail.".into(),"".into(),format!("**{total_nodes} nodes · {total_edges} edges · {} communities**",communities.len()),"".into(),"---".into(),"".into(),"## Communities".into(),"(sorted by size, largest first)".into(),"".into()];
    let mut ordered = communities.to_vec();
    ordered.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (cid, n) in ordered {
        let slug = community_slugs
            .get(&cid)
            .filter(|v| !v.is_null())
            .map(display);
        lines.push(format!(
            "- {} — {n} nodes",
            md_link_to(&community_label(labels, &cid)?, slug.as_deref())
        ));
    }
    lines.push("".into());
    if !gods.is_empty() {
        lines.extend([
            "## God Nodes".into(),
            "(most connected concepts — the load-bearing abstractions)".into(),
            "".into(),
        ]);
        for n in gods {
            lines.push(format!(
                "- {} — {} connections",
                md_link_to(
                    text(n.get("label").ok_or("KeyError: label")?)?,
                    god_slug(n.get("id"))
                ),
                display(n.get("degree").ok_or("KeyError: degree")?)
            ));
        }
        lines.push("".into());
    }
    lines.extend(["---".into(), "".into(), "*Generated by Graphicle*".into()]);
    Ok(lines.join("\n"))
}
pub fn to_wiki(
    g: &Graph,
    raw_communities: &Value,
    out: &Path,
    labels: &Map<String, Value>,
    cohesion: &Map<String, Value>,
    gods: &[Value],
) -> Result<(usize, Vec<String>)> {
    std::fs::create_dir_all(out).map_err(crate::Error::from)?;
    let raw = object(raw_communities)?;
    if raw.is_empty() {
        return Err(format!("communities dict is empty — refusing to clear wiki/. Run `{0} extract .` or `{0} cluster-only .` first.", crate::cli::program()).into());
    }
    let mut original = 0;
    let mut kept = 0;
    let mut communities: Communities = Vec::new();
    for (cid, ns) in raw {
        let ns = ns.as_array().ok_or("Expected community nodes")?;
        original += ns.len();
        let nodes = ns
            .iter()
            .filter_map(|n| g.node_index(n))
            .collect::<Vec<_>>();
        kept += nodes.len();
        if !nodes.is_empty() {
            communities.push((cid.clone(), nodes));
        }
    }
    let mut warnings = Vec::new();
    if kept < original {
        warnings.push(format!(
            "wiki: dropped {} stale node ID(s) not in graph ({} communities remaining)",
            original - kept,
            communities.len()
        ));
    }
    if communities.is_empty() {
        return Err(format!("all community node IDs are stale — none exist in the graph. Re-run `{} extract .` to regenerate .graphicle_analysis.json.", crate::cli::program()).into());
    }
    let mut mapping = vec![None; g.nodes.len()];
    for (cid, ns) in &communities {
        for &n in ns {
            mapping[n] = cid.clone().into();
        }
    }
    let limit = paths::stem_filename_budget(out, SLUG_SUFFIX_RESERVE, 200);
    // "index" is reserved for the catalog written below: an article slugged
    // "index" (a community named after an `index()` hub, an `Index` god node
    // on a case-insensitive filesystem) would otherwise be overwritten by it.
    let mut used = HashSet::from(["index".to_string()]);
    let mut unique = |base: String| {
        let mut slug = base.clone();
        let mut n = 2;
        while used.contains(&slug.to_lowercase()) {
            slug = format!("{base}_{n}");
            n += 1;
        }
        used.insert(slug.to_lowercase());
        slug
    };
    let mut resolver = Map::new();
    let mut community_slugs = Vec::new();
    for (cid, _) in &communities {
        let name = community_label(labels, cid)?;
        let slug = unique(safe_filename(&name, limit));
        resolver.entry(name).or_insert(json!(&slug));
        community_slugs.push(slug);
    }
    let mut god_articles = Vec::new();
    for data in gods {
        if truthy(&data["id"])
            && let Some(n) = g.node_index(&data["id"])
        {
            let name = text(data.get("label").ok_or("KeyError: label")?)?;
            let slug = unique(safe_filename(name, limit));
            resolver.entry(name).or_insert(json!(&slug));
            god_articles.push((n, slug));
        }
    }
    let mut produced = HashSet::new();
    let mut write = |name: &str, text: String| -> Result<()> {
        let name = format!("{name}.md");
        paths::write_text_atomic_if_changed(&out.join(&name), &text)?;
        produced.insert(name);
        Ok(())
    };
    for ((cid, ns), slug) in communities.iter().zip(&community_slugs) {
        let score = cohesion
            .get(cid)
            .filter(|v| !v.is_null())
            .map(|v| v.as_f64().ok_or("TypeError: cohesion"))
            .transpose()?;
        write(
            slug,
            community_article(
                g,
                cid,
                ns,
                &community_label(labels, cid)?,
                ArticleContext {
                    labels,
                    mapping: &mapping,
                    resolver: &resolver,
                },
                score,
            )?,
        )?;
    }
    for (n, slug) in &god_articles {
        write(slug, god_node_article(g, *n, labels, &mapping, &resolver)?)?;
    }
    write(
        "index",
        index_md(
            &communities
                .iter()
                .map(|(c, n)| (c.clone(), n.len()))
                .collect::<Vec<_>>(),
            labels,
            gods,
            g.nodes.len(),
            g.edge_count(),
            &communities
                .iter()
                .zip(&community_slugs)
                .map(|((cid, _), slug)| (cid.clone(), json!(slug)))
                .collect(),
            &god_articles
                .iter()
                .map(|(n, slug)| (g.nodes[*n].id.clone(), slug.clone()))
                .collect::<Vec<_>>(),
        )?,
    )?;
    // Publish every replacement before removing obsolete generated pages.
    for entry in std::fs::read_dir(out)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".md") && !produced.contains(name.as_ref()) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok((communities.len() + god_articles.len(), warnings))
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let op = r["operation"].as_str().unwrap_or("");
    if op == "wiki.safe_name" {
        return Ok(json!(safe_filename(
            text(&r["name"])?,
            r["limit"].as_i64().unwrap_or(200)
        )));
    }
    let resolver = optional_object(r.get("resolver"))?;
    if op == "wiki.link" {
        return Ok(json!(md_link(text(&r["label"])?, &resolver)?));
    }
    let labels = optional_object(r.get("labels").or_else(|| r.get("community_labels")))?;
    let empty = vec![];
    let gods = r
        .get("god_nodes_data")
        .filter(|v| truthy(v))
        .map(|v| v.as_array().ok_or("Expected gods"))
        .transpose()?
        .unwrap_or(&empty);
    if op == "wiki.index" {
        let communities = object(&r["communities"])?
            .iter()
            .map(|(cid, ns)| Ok((cid.clone(), ns.as_array().ok_or("Expected nodes")?.len())))
            .collect::<Result<Vec<_>>>()?;
        let god_articles = r
            .get("god_articles")
            .filter(|v| !v.is_null())
            .map(|v| v.as_array().ok_or("Expected god_articles"))
            .transpose()?
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        let pair = row.as_array().filter(|p| p.len() == 2);
                        let pair = pair.ok_or("Expected (node id, slug) pairs")?;
                        Ok((pair[0].clone(), text(&pair[1])?.to_owned()))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        return Ok(json!(index_md(
            &communities,
            &labels,
            gods,
            r["total_nodes"].as_u64().ok_or("Expected total_nodes")? as usize,
            r["total_edges"].as_u64().ok_or("Expected total_edges")? as usize,
            &optional_object(r.get("community_slugs"))?,
            &god_articles,
        )?));
    }
    let mut g = Graph::from_node_link(&r["graph"], "links")?;
    if let Some(order) = r.get("neighbor_order") {
        g.restore_neighbor_order(order)?;
    }
    let mapping = node_mapping(&g, r.get("node_community"))?;
    match op {
        "wiki.write" => {
            let (n, w) = to_wiki(
                &g,
                &r["communities"],
                Path::new(text(&r["output_dir"])?),
                &labels,
                &optional_object(r.get("cohesion"))?,
                gods,
            )?;
            Ok(json!({"count":n,"warnings":w}))
        }
        "wiki.god" => Ok(json!(god_node_article(
            &g,
            g.node_index(&r["nid"]).ok_or("KeyError: node")?,
            &labels,
            &mapping,
            &resolver
        )?)),
        "wiki.cross" | "wiki.community" => {
            let ns = r["nodes"]
                .as_array()
                .ok_or("Expected nodes")?
                .iter()
                .map(|n| {
                    g.node_index(n)
                        .ok_or_else(|| crate::Error::invalid("KeyError: node"))
                })
                .collect::<Result<Vec<_>>>()?;
            let cid = display(
                r.get("cid")
                    .or_else(|| r.get("own_cid"))
                    .ok_or("Expected cid")?,
            );
            if op == "wiki.cross" {
                Ok(json!(cross_community_links(
                    &g, &ns, &cid, &labels, &mapping
                )?))
            } else {
                Ok(json!(community_article(
                    &g,
                    &cid,
                    &ns,
                    text(&r["label"])?,
                    ArticleContext {
                        labels: &labels,
                        mapping: &mapping,
                        resolver: &resolver
                    },
                    r["cohesion"].as_f64()
                )?))
            }
        }
        _ => Err("Unknown wiki operation".into()),
    }
}
