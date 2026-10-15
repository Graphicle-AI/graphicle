// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Graph traversal, precise symbol lookup, and budgeted text context.

#[derive(Clone, Copy)]
pub struct QueryOptions<'a> {
    pub mode: &'a str,
    pub depth: i64,
    pub budget: i64,
    pub filters: &'a [String],
    pub path: Option<&'a str>,
    /// Explicit start nodes (`query --seed`, Graphify 0.9.88, #4293) instead
    /// of the question's keyword matches; `None` or empty for keyword seeds.
    pub seeds: Option<&'a [String]>,
    /// The root an absolute `--seed` file path is made relative to.
    pub seed_root: Option<&'a std::path::Path>,
}
/// The answer of a query whose question matched no node.
pub const QUERY_NO_MATCH: &str = "No matching nodes found.";
use crate::{Result, graph::Graph, security, serve_search::*};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
pub fn sanitize(s: &str) -> String {
    security::sanitize_label(&json!(s))
}
pub fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
pub fn traversal_view(g: &Graph) -> Result<Graph> {
    if !g.directed {
        return Ok(g.clone());
    }
    let mut h = Graph::new(false, g.multigraph);
    h.attributes = g.attributes.clone();
    for n in &g.nodes {
        h.add_node(n.id.clone(), n.attributes.clone())?;
    }
    for (u, v, e) in g.iter_edges() {
        let mut a = e.attributes.clone();
        a.entry("_src".to_string()).or_insert(g.nodes[u].id.clone());
        a.entry("_tgt".to_string()).or_insert(g.nodes[v].id.clone());
        h.add_edge(g.nodes[u].id.clone(), g.nodes[v].id.clone(), None, a)?;
    }
    Ok(h)
}
pub fn filter_contexts(g: &Graph, filters: &[String]) -> Result<Graph> {
    filtered_contexts(g, filters).map(std::borrow::Cow::into_owned)
}
/// `filter_contexts` without the copy when no context filter applies.
pub fn filtered_contexts<'a>(
    g: &'a Graph,
    filters: &[String],
) -> Result<std::borrow::Cow<'a, Graph>> {
    let filters = normalize_contexts(filters);
    if filters.is_empty() {
        return Ok(std::borrow::Cow::Borrowed(g));
    }
    let mut h = Graph::new(g.directed, g.multigraph);
    for n in &g.nodes {
        h.add_node(n.id.clone(), n.attributes.clone())?;
    }
    for (u, v, e) in g.iter_edges() {
        if filters.contains(&text(e.attributes.get("context").unwrap_or(&Value::Null))) {
            h.add_edge(
                g.nodes[u].id.clone(),
                g.nodes[v].id.clone(),
                g.multigraph.then(|| e.key.to_value()),
                e.attributes.clone(),
            )?;
        }
    }
    Ok(std::borrow::Cow::Owned(h))
}
pub fn complete_induced(g: &Graph, visited: &BTreeSet<usize>, edges: &mut Vec<(usize, usize)>) {
    let key = |u, v| if g.directed || u < v { (u, v) } else { (v, u) };
    let mut seen = edges
        .iter()
        .map(|(u, v)| key(*u, *v))
        .collect::<BTreeSet<_>>();
    let mut ns = visited.iter().copied().collect::<Vec<_>>();
    ns.sort_by_key(|i| id(g, *i));
    let mut examined = BTreeSet::new();
    for u in ns {
        for (_, v, _) in g.out_edges(u) {
            if !g.directed && examined.contains(&v) {
                continue;
            }
            if u != v && visited.contains(&v) && seen.insert(key(u, v)) {
                edges.push((u, v));
            }
        }
        examined.insert(u);
    }
}
pub fn traverse(
    g: &Graph,
    seeds: &[usize],
    depth: i64,
    dfs: bool,
) -> (BTreeSet<usize>, Vec<(usize, usize)>) {
    let ds = degrees(g);
    let mut sorted = ds.clone();
    sorted.sort();
    let threshold = sorted
        .get((sorted.len() as f64 * 0.99) as usize)
        .copied()
        .unwrap_or(50)
        .max(50);
    let seedset = seeds.iter().copied().collect::<BTreeSet<_>>();
    let mut visited = BTreeSet::new();
    let mut edges = vec![];
    if dfs {
        let mut stack = seeds.iter().rev().map(|i| (*i, 0)).collect::<Vec<_>>();
        while let Some((i, d)) = stack.pop() {
            if visited.contains(&i) || d > depth {
                continue;
            }
            visited.insert(i);
            if !seedset.contains(&i) && ds[i] >= threshold {
                continue;
            }
            for nb in neighbors(g, i, false) {
                if !visited.contains(&nb) {
                    stack.push((nb, d + 1));
                    edges.push((i, nb));
                }
            }
        }
    } else {
        visited = seedset.clone();
        let mut frontier = seedset.clone();
        for _ in 0..depth.max(0) {
            let mut next = BTreeSet::new();
            for i in frontier {
                if !seedset.contains(&i) && ds[i] >= threshold {
                    continue;
                }
                for nb in neighbors(g, i, false) {
                    if !visited.contains(&nb) {
                        next.insert(nb);
                        edges.push((i, nb));
                    }
                }
            }
            visited.extend(&next);
            frontier = next;
        }
    }
    complete_induced(g, &visited, &mut edges);
    (visited, edges)
}
fn attrs_summary(attrs: &Value) -> String {
    let Some(attrs) = attrs.as_object().filter(|a| !a.is_empty()) else {
        return String::new();
    };
    let mut keys = attrs.keys().collect::<Vec<_>>();
    keys.sort();
    let mut pairs = vec![];
    for k in keys {
        let v = &attrs[k];
        let raw = match v {
            Value::Object(_) | Value::Array(_) => {
                json_text(&crate::extract_normalize::canonical(v), None, true)
            }
            Value::String(s) => format!("\"{s}\""),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            _ => string(v),
        };
        let value = if raw.chars().count() > 60 {
            format!("{}...", cut(&raw, 57))
        } else {
            raw
        };
        pairs.push(format!("{k}={value}"));
        if pairs.len() >= 10 {
            pairs.push("...".into());
            break;
        }
    }
    let mut summary = pairs.join(", ");
    if summary.chars().count() > 200 {
        summary = format!("{}...", cut(&summary, 197));
    }
    format!(" attrs={{{}}}", sanitize(&summary))
}
pub fn subgraph_text(
    g: &Graph,
    nodes: &BTreeSet<usize>,
    edges: &[(usize, usize)],
    budget: i64,
    seeds: &[usize],
) -> String {
    let char_budget = (budget.max(0) as usize).saturating_mul(3);
    let ds = degrees(g);
    let seedset = seeds.iter().copied().collect::<BTreeSet<_>>();
    let hits = seeds
        .iter()
        .filter(|i| nodes.contains(i))
        .copied()
        .collect::<Vec<_>>();
    let mut dist = hits
        .iter()
        .map(|i| (*i, 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut queue = VecDeque::from(hits.clone());
    while let Some(i) = queue.pop_front() {
        let d = dist[&i];
        for nb in neighbors(g, i, false).into_iter().chain(if g.directed {
            neighbors(g, i, true)
        } else {
            vec![]
        }) {
            if nodes.contains(&nb) && !dist.contains_key(&nb) {
                dist.insert(nb, d + 1);
                queue.push_back(nb);
            }
        }
    }
    let mut rest = nodes.difference(&seedset).copied().collect::<Vec<_>>();
    rest.sort_by(|a, b| {
        dist.get(a)
            .unwrap_or(&(1 << 30))
            .cmp(dist.get(b).unwrap_or(&(1 << 30)))
            .then_with(|| ds[*b].cmp(&ds[*a]))
            .then_with(|| id(g, *a).cmp(&id(g, *b)))
    });
    let ordered = hits.iter().copied().chain(rest);
    let mut lines = vec![];
    for i in ordered {
        let get = |k| g.nodes[i].attributes.get(k).map(string).unwrap_or_default();
        let overlay = &g.attributes["_learning_overlay"][id(g, i)];
        let mut suffix = String::new();
        if crate::build::truthy(overlay) {
            let status = sanitize(&text(&overlay["status"]));
            if !status.is_empty() {
                suffix = format!(
                    " learning={status}{}",
                    if crate::build::truthy(&overlay["stale"]) {
                        ":stale"
                    } else {
                        ""
                    }
                );
            }
        }
        let community = if crate::build::truthy(field(g, i, "community_name")) {
            get("community_name")
        } else {
            get("community")
        };
        let attrs = attrs_summary(field(g, i, "attributes"));
        lines.push(format!(
            "NODE {} [src={} loc={} community={}{suffix}{attrs}]",
            label_text(g, i),
            sanitize(&get("source_file")),
            sanitize(&get("source_location")),
            sanitize(&community)
        ));
    }
    for (u, v) in edges {
        if !nodes.contains(u) || !nodes.contains(v) {
            continue;
        }
        let Some(e) = g
            .edges_between(&g.nodes[*u].id, &g.nodes[*v].id)
            .first()
            .copied()
        else {
            continue;
        };
        let attr = |k| e.attributes.get(k).map(string).unwrap_or_default();
        let a = e
            .attributes
            .get("_src")
            .and_then(|id| g.node_index(id))
            .unwrap_or(*u);
        let b = e
            .attributes
            .get("_tgt")
            .and_then(|id| g.node_index(id))
            .unwrap_or(*v);
        let (a, b) = if BTreeSet::from([a, b]) == BTreeSet::from([*u, *v]) {
            (a, b)
        } else {
            (*u, *v)
        };
        let context = e
            .attributes
            .get("context")
            .filter(|v| crate::build::truthy(v))
            .map(|v| format!(" context={}", sanitize(&string(v))))
            .unwrap_or_default();
        let location = e
            .attributes
            .get("source_location")
            .filter(|v| crate::build::truthy(v))
            .map(string)
            .unwrap_or_default();
        let at = if location.is_empty() {
            String::new()
        } else {
            format!(
                " at={}:{}",
                sanitize(&text(
                    e.attributes.get("source_file").unwrap_or(&Value::Null)
                )),
                sanitize(&location)
            )
        };
        lines.push(format!(
            "EDGE {} --{} [{}{context}]--> {}{at}",
            label_text(g, a),
            sanitize(&attr("relation")),
            sanitize(&attr("confidence")),
            label_text(g, b)
        ));
    }
    let output = lines.join("\n");
    if output.chars().count() <= char_budget {
        return output;
    }
    let prefix = cut(&output, char_budget);
    let cut_at = prefix
        .rfind('\n')
        .map(|i| prefix[..i].chars().count())
        .filter(|n| *n > 0)
        .unwrap_or(char_budget);
    let seed_end = hits
        .len()
        .checked_sub(1)
        .map(|_| {
            lines
                .iter()
                .take(hits.len())
                .map(|s| s.chars().count() + 1)
                .sum::<usize>()
                - 1
        })
        .unwrap_or(0);
    let cut_at = cut_at.max(seed_end.min(output.chars().count()));
    let kept = cut(&output, cut_at);
    let total_nodes = lines.iter().filter(|s| s.starts_with("NODE ")).count();
    let shown = kept.matches("\nNODE ").count() + usize::from(output.starts_with("NODE "));
    let cut_count = total_nodes.saturating_sub(shown);
    if cut_count == 0 {
        let edges = lines.iter().filter(|s| s.starts_with("EDGE ")).count();
        let estimate = output.chars().count() / 3;
        format!(
            "[i] Complete answer over budget: all {total_nodes} nodes and {edges} edges shown (~{estimate} tokens vs the requested ~{budget}-token budget). Edges are never dropped once every node fits, so this is already the full answer — raising --budget further will not shrink it. Narrow with context_filter=['call'] or use get_node for a specific symbol to reduce size instead.\n\n{output}"
        )
    } else {
        // The smallest budget whose cut keeps every NODE line; past that,
        // edges are never dropped (Graphify 0.9.86, #4323).
        let node_chars = lines
            .iter()
            .filter(|s| s.starts_with("NODE "))
            .map(|s| s.chars().count() + 1)
            .sum::<usize>();
        let full = node_chars.min(output.chars().count()).div_ceil(3);
        format!(
            "[!] TRUNCATED: showing {shown} of {total_nodes} nodes (~{budget}-token budget). The answer may be among the {cut_count} cut nodes — raise the token budget to {full} to get all {total_nodes} (CLI: --budget {full}, MCP: token_budget={full}) or narrow the query (e.g. context_filter=['call'], or get_node for a specific symbol).\n\n{kept}\n... (truncated — {cut_count} more nodes cut by ~{budget}-token budget. Narrow with context_filter=['call'] or use get_node for a specific symbol)"
        )
    }
}
pub fn cut_lines(lines: &[String], budget: i64, hint: &str) -> String {
    let output = lines.join("\n");
    let limit = (budget.max(0) as usize).saturating_mul(3);
    if output.chars().count() <= limit {
        return output;
    }
    let prefix = cut(&output, limit);
    let kept = prefix
        .rfind('\n')
        .filter(|i| *i > 0)
        .map(|i| prefix[..i].to_string())
        .unwrap_or(prefix);
    let shown = kept.matches('\n').count() + 1;
    let left = lines.len().saturating_sub(shown);
    // The budget that returns every line (Graphify 0.9.86, #4323).
    let full = output.chars().count().div_ceil(3);
    let total = lines.len();
    format!(
        "[!] TRUNCATED: showing {shown} of {total} lines (~{budget}-token budget). Raise the token budget to {full} to get all {total} (token_budget={full}). {hint}\n\n{kept}\n... (truncated — {left} more lines cut by ~{budget}-token budget. {hint})"
    )
}
pub fn query_text(
    g: &Graph,
    index: &SearchIndex,
    q: &str,
    context: QueryOptions<'_>,
) -> Result<String> {
    // Explicit start nodes: an external ranker (embeddings, a learned
    // retriever, grep) can choose where the traversal begins. Each value
    // resolves as `affected` resolves its seed (node id, unique label, or a
    // repo-relative or absolute file path), and one that does not resolve to
    // exactly one node is reported and skipped, never guessed (#4293).
    if let Some(values) = context.seeds.filter(|s| !s.is_empty()) {
        let mut unresolved: Vec<&str> = vec![];
        let mut start = vec![];
        for value in values {
            let found = crate::affected::resolve_seed(g, value, context.seed_root)?
                .and_then(|id| g.node_index(&json!(id)));
            match found {
                None if !unresolved.contains(&value.as_str()) => unresolved.push(value.as_str()),
                Some(i) if !start.contains(&i) => start.push(i),
                _ => {}
            }
        }
        if start.is_empty() {
            return Ok(format!(
                "No unique node match for --seed: {}",
                values.join(", ")
            ));
        }
        return render_query(g, index, q, &start, context, true, &unresolved);
    }
    let terms = crate::query_terms::query_terms(q);
    let mut scored = score_query(g, index, &terms, true);
    let intent = CONSTANTS["_RELATIONAL_INTENT_TERMS"].as_array().unwrap();
    if terms.iter().any(|t| !intent.contains(&json!(t))) {
        scored
            .best_seed_by_term
            .retain(|t, _| !intent.contains(&json!(t)));
    }
    let seeds = pick_seeds_in_order(
        Some(g),
        &scored.ranked,
        3,
        0.2,
        &scored.best_seed_by_term,
        &scored.term_order,
    );
    if seeds.is_empty() {
        return Ok(QUERY_NO_MATCH.into());
    }
    render_query(g, index, q, &seeds, context, false, &[])
}
/// The traversal from `seeds` and its text, under the query header;
/// `given` marks explicit `--seed` start nodes, and `unresolved` lists the
/// seed values that were skipped.
fn render_query(
    g: &Graph,
    index: &SearchIndex,
    q: &str,
    seeds: &[usize],
    context: QueryOptions<'_>,
    given: bool,
    unresolved: &[&str],
) -> Result<String> {
    let QueryOptions {
        mode,
        depth,
        budget,
        filters,
        path,
        ..
    } = context;
    let (filters, source) = resolve_contexts(q, filters);
    let view = index.traversal_view(g)?;
    let traversal = filtered_contexts(&view, &filters)?;
    let (nodes, edges) = traverse(&traversal, seeds, depth, mode == "dfs");
    let start = crate::python_compat::repr(&json!(
        seeds.iter().map(|i| label(g, *i)).collect::<Vec<_>>()
    ));
    let mut head = vec![
        format!("Traversal: {} depth={depth}", mode.to_uppercase()),
        format!("Start{}: {start}", if given { " (given)" } else { "" }),
    ];
    if !unresolved.is_empty() {
        head.push(format!(
            "Unresolved seeds (skipped): {}",
            crate::python_compat::repr(&json!(unresolved))
        ));
    }
    if let Some(path) = path {
        head.insert(
            0,
            format!(
                "Graph: {} ({} nodes)",
                display_graph_path(path),
                g.nodes.len()
            ),
        );
    }
    if !filters.is_empty() {
        head.push(format!(
            "Context: {} ({})",
            filters.join(", "),
            source.unwrap_or("")
        ));
    }
    head.push(format!("{} nodes found", nodes.len()));
    Ok(format!(
        "{}\n\n{}",
        head.join(" | "),
        subgraph_text(&traversal, &nodes, &edges, budget, seeds)
    ))
}
pub fn display_graph_path(path: &str) -> String {
    let p = crate::extractors::resolution::resolved(std::path::Path::new(path));
    let cwd = crate::extractors::resolution::resolved(std::path::Path::new("."));
    let shown = p
        .strip_prefix(cwd)
        .unwrap_or(&p)
        .to_string_lossy()
        .replace('\\', "/");
    if shown.is_empty() { ".".into() } else { shown }
}

pub fn json_text(v: &Value, indent: Option<usize>, ascii: bool) -> String {
    let mut buf = vec![];
    let _ = crate::python_json::write(&mut buf, v, indent, ascii);
    String::from_utf8_lossy(&buf).into_owned()
}
