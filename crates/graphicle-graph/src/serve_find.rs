// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Symbol disambiguation and shortest paths in stored edge direction.
use crate::{Result, graph::Graph, serve_search::*};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};
/// The prefix of a refusal that lists candidates (see [`is_ambiguity_refusal`]).
pub const AMBIGUOUS: &str = "Ambiguous: ";
/// `pkg.mod.func`, `pkg.mod.func()` or `pkg.mod:func` (Graphify 0.9.88, #4366).
static PY_QUALIFIED: LazyLock<regex::Regex> = LazyLock::new(|| {
    crate::regex_compat::compile(r"^([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)[.:]([A-Za-z_]\w*)(?:\(\))?$")
});
/// `str.strip()`: Python's whitespace on both ends.
fn strip(s: &str) -> &str {
    s.trim_matches(crate::dedup_strings::whitespace)
}
fn exact(g: &Graph, i: usize, term: &str, nq: &str) -> bool {
    let nl = norm_label(g, i);
    let bare = nl.trim_end_matches(['(', ')']);
    let lt = tokens(&text(field(g, i, "label"))).join(" ");
    let nid = id(g, i).to_lowercase();
    let folded = if nid.is_ascii() {
        nid.clone()
    } else {
        fold(&nid).to_lowercase()
    };
    [nl.as_str(), bare, &lt, &nid].contains(&term) || [nl.as_str(), bare, &folded].contains(&nq)
}
pub fn scoped(g: &Graph, index: &SearchIndex, path: &str, symbol: &str) -> Vec<usize> {
    let pt = tokens(path).join(" ");
    let pq = fold(path).to_lowercase().trim().to_string();
    let st = tokens(symbol).join(" ");
    let sq = fold(symbol).to_lowercase().trim().to_string();
    if pt.is_empty() && pq.is_empty() || st.is_empty() && sq.is_empty() {
        return vec![];
    }
    index
        .candidates(&[st.clone(), sq.clone()], 0.1)
        .unwrap_or_else(|| (0..g.nodes.len()).collect())
        .into_iter()
        .filter(|i| {
            let sf = text(field(g, *i, "source_file"));
            let nt = tokens(&sf).join(" ");
            let nf = fold(&sf).to_lowercase().trim().to_string();
            let file = nt == pt
                || nf == pq
                || !pq.is_empty() && nf.ends_with(&format!("/{pq}"))
                || !pt.is_empty() && nt.ends_with(&format!(" {pt}"));
            if !file {
                return false;
            }
            let nl = norm_label(g, *i);
            let bare = nl.trim_end_matches(['(', ')']);
            let lt = tokens(&text(field(g, *i, "label"))).join(" ");
            let nid = id(g, *i).to_lowercase();
            [nl.as_str(), bare, &lt, &nid].contains(&st.as_str())
                || [nl.as_str(), bare].contains(&sq.as_str())
        })
        .collect()
}
pub fn literal_exact(g: &Graph, index: &SearchIndex, term: &str, nq: &str) -> bool {
    index
        .candidates(&[term.into(), nq.into()], 0.1)
        .unwrap_or_else(|| (0..g.nodes.len()).collect())
        .iter()
        .any(|i| exact(g, *i, term, nq))
}
/// Nodes named by a dotted Python symbol such as `pkg.mod.func` (#4366): the
/// `path::symbol` lookup of `pkg/mod.py`, else of `pkg/mod/__init__.py`.
pub fn python_qualified(g: &Graph, index: &SearchIndex, label: &str) -> Vec<usize> {
    let Some(qualified) = PY_QUALIFIED.captures(strip(label)) else {
        return vec![];
    };
    let module = qualified[1].replace('.', "/");
    for path in [format!("{module}.py"), format!("{module}/__init__.py")] {
        let found = scoped(g, index, &path, &qualified[2]);
        if !found.is_empty() {
            return found;
        }
    }
    vec![]
}
pub fn tiers(g: &Graph, index: &SearchIndex, query: &str) -> [Vec<usize>; 4] {
    let term = tokens(query).join(" ");
    let nq = fold(query).to_lowercase().trim().to_string();
    if let Some((path, sym)) = query.split_once("::")
        && !literal_exact(g, index, &term, &nq)
        && !path.trim().is_empty()
        && !sym.trim().is_empty()
    {
        let scoped = scoped(g, index, path.trim(), sym.trim());
        if !scoped.is_empty() {
            return [scoped, vec![], vec![], vec![]];
        }
    }
    // A dotted Python symbol (`pkg.mod.func`, or the `pkg.mod:func` entry-point
    // form) names a module path plus a symbol; tokenized as-is it matches no
    // label, so it maps onto the `path::symbol` lookup above, suffix-matched so
    // a `src/` layout still resolves (#4366). No hit falls through.
    if PY_QUALIFIED.is_match(strip(query)) && !literal_exact(g, index, &term, &nq) {
        let scoped = python_qualified(g, index, query);
        if !scoped.is_empty() {
            return [scoped, vec![], vec![], vec![]];
        }
    }
    let mut out = [vec![], vec![], vec![], vec![]];
    if term.is_empty() {
        return out;
    }
    for i in index
        .candidates(&[term.clone(), nq.clone()], 0.1)
        .unwrap_or_else(|| (0..g.nodes.len()).collect())
    {
        let nl = norm_label(g, i);
        let bare = nl.trim_end_matches(['(', ')']);
        let lt = tokens(&text(field(g, i, "label"))).join(" ");
        let sf = tokens(&text(field(g, i, "source_file"))).join(" ");
        let nid = id(g, i).to_lowercase();
        if term == sf {
            out[0].push(i);
        } else if exact(g, i, &term, &nq) {
            out[1].push(i);
        } else if [nl.as_str(), bare, &lt, &nid]
            .iter()
            .any(|s| s.starts_with(&term))
            || [nl.as_str(), bare].iter().any(|s| s.starts_with(&nq))
        {
            out[2].push(i);
        } else if nl.contains(&term) || lt.contains(&term) || nl.contains(&nq) {
            out[3].push(i);
        }
    }
    if !out[0].is_empty() {
        let base = fold(
            std::path::Path::new(query)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
        )
        .to_lowercase();
        let preferred = out[0]
            .iter()
            .copied()
            .filter(|i| {
                if string(field(g, *i, "source_location")) != "L1" {
                    return false;
                }
                let label = fold(&text(field(g, *i, "label"))).to_lowercase();
                label == base || label.ends_with(&format!("/{base}"))
            })
            .collect::<Vec<_>>();
        if preferred.len() == 1 {
            out[0].retain(|i| *i != preferred[0]);
            out[0].insert(0, preferred[0]);
        }
    }
    out
}
pub fn ambiguity(g: &Graph, index: &SearchIndex, query: &str) -> Vec<usize> {
    for tier in tiers(g, index, query) {
        if !tier.is_empty() {
            return rivals_across_files(g, &tier);
        }
    }
    vec![]
}
/// One node per distinct source file in `tier`, or none for a single file.
fn rivals_across_files(g: &Graph, tier: &[usize]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let by_source = tier
        .iter()
        .copied()
        .filter(|i| seen.insert(text(field(g, *i, "source_file"))))
        .collect::<Vec<_>>();
    if by_source.len() > 1 {
        by_source
    } else {
        vec![]
    }
}
/// Shared node resolution for `explain` and the get_node / get_neighbors
/// tools: the single winner of the first non-empty tier, or the refusal when
/// there is no match or that tier holds several nodes, in different files or
/// in one, as the `path` endpoints refuse them (Graphify 0.9.86, #4320).
pub fn resolve(g: &Graph, index: &SearchIndex, query: &str) -> std::result::Result<usize, String> {
    let [source_exact, exact, prefix, substring] = tiers(g, index, query);
    let file_query = !source_exact.is_empty() && !query.contains("::");
    let tier = [source_exact, exact, prefix, substring]
        .into_iter()
        .find(|tier| !tier.is_empty())
        .ok_or_else(|| format!("No node matching '{query}' found."))?;
    if let Some(refusal) = tier_refusal(g, index, query, &tier, file_query) {
        return Err(refusal);
    }
    Ok(tier[0])
}
/// The refusal when the winning `tier` names more than one node, else `None`.
/// Shared by `explain`, get_node / get_neighbors and the `path` endpoints. A
/// file-path query is the one exception: its tier is the file node followed by
/// its members, which is precedence, not a tie.
pub fn tier_refusal(
    g: &Graph,
    index: &SearchIndex,
    label: &str,
    tier: &[usize],
    file_query: bool,
) -> Option<String> {
    let rivals = rivals_across_files(g, tier);
    if !rivals.is_empty() {
        return Some(ambiguity_message(g, label, &rivals));
    }
    // A dotted Python symbol's tier is symbols, not a file followed by its
    // members, so a function and a same-named method in one module tie (#4366).
    let file_query =
        file_query && !(tier.len() > 1 && tier == python_qualified(g, index, label).as_slice());
    if tier.len() > 1 && !file_query {
        return Some(same_file_ambiguity_message(g, label, tier));
    }
    None
}
/// True for a refusal that lists candidates, false for a miss (no node
/// matched) or no refusal.
pub fn is_ambiguity_refusal(message: Option<&str>) -> bool {
    message.is_some_and(|m| m.starts_with(AMBIGUOUS))
}
pub fn ambiguity_message(g: &Graph, query: &str, rivals: &[usize]) -> String {
    let listing = rivals
        .iter()
        .map(|i| {
            let sf = text(field(g, *i, "source_file"));
            format!(
                "  {}\n    id: {}",
                if sf.is_empty() { id(g, *i) } else { sf },
                id(g, *i)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The example names the matched node's own label: echoing the query would
    // nest a path-scoped one (`index.ts::foo()`) behind a second path.
    let own = rivals
        .first()
        .map(|i| text(field(g, *i, "label")))
        .unwrap_or_default();
    let symbol = if own.is_empty() { query } else { own.as_str() };
    format!(
        "{AMBIGUOUS}'{query}' matches {} nodes in different files.\n{listing}\nRetry with path::symbol using one of the paths above (e.g. <path>::{symbol}) or the full node id.",
        rivals.len()
    )
}
fn same_file_ambiguity_message(g: &Graph, query: &str, hits: &[usize]) -> String {
    let source = text(field(g, hits[0], "source_file"));
    let place = if source.is_empty() {
        "with no source file".to_string()
    } else {
        format!("in {source}")
    };
    let listing = hits
        .iter()
        .map(|h| {
            let location = text(field(g, *h, "source_location"));
            format!(
                "  {}\n    id: {}",
                if location.is_empty() {
                    label(g, *h)
                } else {
                    location
                },
                id(g, *h)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{AMBIGUOUS}'{query}' matches {} nodes {place}.\n{listing}\nRetry with the full node id.",
        hits.len()
    )
}
/// The refusal for a `file::symbol` endpoint whose file defines no such symbol
/// (Graphify 0.9.84, #4264): fail closed like `explain` instead of scoring the
/// bare name across other files, and name the files that do define it.
pub fn no_scoped_match_message(g: &Graph, path_part: &str, symbol_part: &str) -> String {
    let st = tokens(symbol_part).join(" ");
    let sq = fold(symbol_part).to_lowercase().trim().to_string();
    let mut others = BTreeSet::new();
    for i in 0..g.nodes.len() {
        let nl = norm_label(g, i);
        let bare = nl.trim_end_matches(['(', ')']);
        let lt = tokens(&text(field(g, i, "label"))).join(" ");
        if [nl.as_str(), bare, &lt].contains(&st.as_str())
            || [nl.as_str(), bare].contains(&sq.as_str())
        {
            let sf = text(field(g, i, "source_file"));
            if !sf.is_empty() {
                others.insert(sf);
            }
        }
    }
    let mut message = format!(
        "No node matching '{path_part}::{symbol_part}' found; {path_part} defines no '{symbol_part}'."
    );
    for source in others {
        message.push_str(&format!("\n  defined in {source}"));
    }
    message
}
/// Resolve one endpoint of the `path` CLI / `shortest_path` tool (#3913).
///
/// An exact-tier hit (`path::symbol`, a full node id, an exact label or source
/// path) names the node outright and is refused with the candidates when it
/// spans several files, as `explain` does; scoring alone tokenized those forms,
/// so the path half of `path::symbol` pulled the route to the file node. Several
/// hits in one file are refused too unless the query is a plain file path,
/// since only the node id can separate them. Only a query with no exact hit
/// falls through to scoring. Returns `(node, scored, None)`, where `scored` is
/// empty unless scoring picked the node; `(None, [], Some(message))` when
/// ambiguous; `(None, [], None)` when nothing matches.
pub fn resolve_path_endpoint(
    g: &Graph,
    index: &SearchIndex,
    query: &str,
) -> (Option<usize>, Vec<(f64, usize)>, Option<String>) {
    let [source_exact, exact, _, _] = tiers(g, index, query);
    // A `path::symbol` hit also lands in the source tier; only a plain path
    // query makes that tier a file lookup.
    let file_query = !source_exact.is_empty() && !query.contains("::");
    let hits = if source_exact.is_empty() {
        exact
    } else {
        source_exact
    };
    if !hits.is_empty() {
        if let Some(refusal) = tier_refusal(g, index, query, &hits, file_query) {
            return (None, vec![], Some(refusal));
        }
        return (Some(hits[0]), vec![], None);
    }
    // A deliberately file-scoped endpoint (`file::symbol`) that matched no
    // symbol in that file must not fall back to scoring the bare name across
    // other files (#4264). A native `::` label that matched resolved above.
    if let Some((path, symbol)) = query.split_once("::")
        && !strip(path).is_empty()
        && !strip(symbol).is_empty()
    {
        return (
            None,
            vec![],
            Some(no_scoped_match_message(g, strip(path), strip(symbol))),
        );
    }
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let scored = score_query(g, index, &terms, false).ranked;
    if scored.is_empty() {
        return (None, vec![], None);
    }
    (pick_endpoint(g, &scored, query), scored, None)
}
fn general(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    let exp = n.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format!("{n:.5e}");
        let (m, e) = s.split_once('e').unwrap();
        let e = e.parse::<i32>().unwrap_or(0);
        format!("{}e{e:+03}", m.trim_end_matches('0').trim_end_matches('.'))
    } else {
        let precision = (5 - exp).max(0) as usize;
        let s = format!("{n:.precision$}");
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').into()
        } else {
            s
        }
    }
}
/// Why a lookup gave no answer: a name named several nodes (or both `path`
/// endpoints one node), or no node matched. The CLI exits 3 or 4 for them
/// (Graphify 0.9.86, #4320).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Ambiguous,
    NotFound,
}
impl Refusal {
    /// The refusal a lookup's error message reports.
    pub fn of(message: &str) -> Self {
        if is_ambiguity_refusal(Some(message)) {
            Self::Ambiguous
        } else {
            Self::NotFound
        }
    }
}
/// A `path` answer: its text, the refusal when an endpoint did not resolve to
/// one node, and the hop count of a found path.
pub struct PathAnswer {
    pub text: String,
    pub refusal: Option<Refusal>,
    pub hops: Option<usize>,
}
impl PathAnswer {
    fn text(text: String) -> Self {
        Self {
            text,
            refusal: None,
            hops: None,
        }
    }
    fn refused(text: String, refusal: Refusal) -> Self {
        Self {
            text,
            refusal: Some(refusal),
            hops: None,
        }
    }
}
/// The sorted, materialized adjacency `shortest_path` searches (#2074): in
/// stored edge direction (`_src`/`_tgt`, #2309) plus the implied reverse hop of
/// each `contains` edge (#3878), or undirected.
fn path_adjacency(g: &Graph, undirected: bool) -> PathAdjacency {
    // Neighbour lists in NetworkX insertion order: the search below visits
    // them as stored, which decides between equally short routes.
    let mut adj = vec![Vec::new(); g.nodes.len()];
    let mut reverse = vec![Vec::new(); g.nodes.len()];
    if undirected {
        let mut sets = vec![BTreeSet::new(); g.nodes.len()];
        for (u, v, _) in g.iter_edges() {
            sets[u].insert(v);
            sets[v].insert(u);
        }
        for (u, set) in sets.into_iter().enumerate() {
            let mut neighbors = set.into_iter().collect::<Vec<_>>();
            neighbors.sort_by_key(|i| id(g, *i));
            reverse[u] = neighbors.clone();
            adj[u] = neighbors;
        }
    } else {
        let (mut stored, mut implied) = (vec![], vec![]);
        for (u, v, e) in g.iter_edges() {
            let a = e
                .attributes
                .get("_src")
                .and_then(|id| g.node_index(id))
                .unwrap_or(u);
            let b = e
                .attributes
                .get("_tgt")
                .and_then(|id| g.node_index(id))
                .unwrap_or(v);
            stored.push((a, b));
            // A `contains` edge only runs file -> symbol, so a route reaching
            // a symbol could never continue to its file (#3878). The implied
            // reverse hop is for traversal only; segments print the real edge.
            if e.attributes.get("relation").and_then(Value::as_str) == Some("contains") {
                implied.push((b, a));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for mut batch in [stored, implied] {
            batch.sort_by_cached_key(|(a, b)| (id(g, *a), id(g, *b)));
            for (a, b) in batch {
                if seen.insert((a, b)) {
                    adj[a].push(b);
                    reverse[b].push(a);
                }
            }
        }
    }
    (adj, reverse)
}
pub fn shortest_path(g: &Graph, index: &SearchIndex, args: &Value) -> Result<String> {
    Ok(shortest_path_with(g, index, args, false)?.text)
}
pub fn shortest_path_cli(g: &Graph, index: &SearchIndex, args: &Value) -> Result<PathAnswer> {
    shortest_path_with(g, index, args, true)
}
fn shortest_path_with(
    g: &Graph,
    index: &SearchIndex,
    args: &Value,
    cli: bool,
) -> Result<PathAnswer> {
    let source = args["source"].as_str().ok_or("source is required")?;
    let target = args["target"].as_str().ok_or("target is required")?;
    let (src, ss, err) = resolve_path_endpoint(g, index, source);
    if let Some(err) = err {
        let refusal = Refusal::of(&err);
        return Ok(PathAnswer::refused(err, refusal));
    }
    let Some(src) = src else {
        return Ok(PathAnswer::refused(
            format!("No node matching source '{source}' found."),
            Refusal::NotFound,
        ));
    };
    let (tgt, ts, err) = resolve_path_endpoint(g, index, target);
    if let Some(err) = err {
        let refusal = Refusal::of(&err);
        return Ok(PathAnswer::refused(err, refusal));
    }
    let Some(tgt) = tgt else {
        return Ok(PathAnswer::refused(
            format!("No node matching target '{target}' found."),
            Refusal::NotFound,
        ));
    };
    if src == tgt {
        return Ok(PathAnswer::refused(
            format!(
                "'{source}' and '{target}' both resolved to the same node '{}'. Use a more specific label or the exact node ID.",
                id(g, src)
            ),
            Refusal::Ambiguous,
        ));
    }
    let mut warnings = vec![];
    for (name, ranked, node) in [("source", &ss, src), ("target", &ts, tgt)] {
        if ranked.len() >= 2 && node == ranked[0].1 {
            let (top, runner) = (ranked[0].0, ranked[1].0);
            if top > 0.0 && (top - runner) / top < 0.1 {
                warnings.push(format!(
                    "warning: {name} match was ambiguous (top score {}, runner-up {})",
                    general(top),
                    general(runner)
                ));
            }
        }
    }
    let undirected = args["undirected"].as_bool().unwrap_or(false);
    let adjacency = index.path_adjacency(undirected, || path_adjacency(g, undirected));
    let (adj, reverse) = &*adjacency;
    let mut parents = BTreeMap::from([(src, src)]);
    let mut successors = BTreeMap::from([(tgt, tgt)]);
    let mut forward = vec![src];
    let mut backward = vec![tgt];
    let mut meeting = None;
    // Match NetworkX's unweighted bidirectional search, including its choice
    // between equally short routes through differently sized frontiers.
    'search: while !forward.is_empty() && !backward.is_empty() {
        let forwards = forward.len() <= backward.len();
        let level = std::mem::take(if forwards {
            &mut forward
        } else {
            &mut backward
        });
        for node in level {
            let neighbors = if forwards { &adj[node] } else { &reverse[node] };
            for &next in neighbors {
                if forwards {
                    if let std::collections::btree_map::Entry::Vacant(entry) = parents.entry(next) {
                        entry.insert(node);
                        forward.push(next);
                    }
                    if successors.contains_key(&next) {
                        meeting = Some(next);
                        break 'search;
                    }
                } else {
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        successors.entry(next)
                    {
                        entry.insert(node);
                        backward.push(next);
                    }
                    if parents.contains_key(&next) {
                        meeting = Some(next);
                        break 'search;
                    }
                }
            }
        }
    }
    let Some(meeting) = meeting else {
        let source_label = if cli {
            source.to_string()
        } else {
            label(g, src)
        };
        let target_label = if cli {
            target.to_string()
        } else {
            label(g, tgt)
        };
        let message = if undirected {
            format!(
                "No path found between '{}' and '{}'.",
                source_label, target_label
            )
        } else {
            format!(
                "No directed path found between '{}' and '{}'. Retry with undirected=true to search ignoring edge direction.",
                source_label, target_label
            )
        };
        return Ok(PathAnswer::text(if cli && !warnings.is_empty() {
            format!("{}\n{message}", warnings.join("\n"))
        } else {
            message
        }));
    };
    let mut path = vec![meeting];
    while *path.last().unwrap() != src {
        path.push(parents[path.last().unwrap()]);
    }
    path.reverse();
    let mut node = meeting;
    while node != tgt {
        node = successors[&node];
        path.push(node);
    }
    let hops = path.len() - 1;
    let max = args["max_hops"].as_i64().unwrap_or(8);
    if hops as i64 > max {
        return Ok(PathAnswer::text(format!(
            "Path exceeds max_hops={max} ({hops} hops found)."
        )));
    }
    let mut segments = vec![label(g, src)];
    for pair in path.windows(2) {
        let (u, v) = (pair[0], pair[1]);
        let (mut fwd, mut bwd) = (vec![], vec![]);
        for (a, b) in [(u, v), (v, u)] {
            for e in g.edges_between(&g.nodes[a].id, &g.nodes[b].id) {
                if e.attributes.get("_src").unwrap_or(&g.nodes[a].id) == &g.nodes[u].id {
                    fwd.push(e);
                } else {
                    bwd.push(e);
                }
            }
        }
        let forward = !fwd.is_empty();
        let data = if forward { fwd } else { bwd };
        let terms = |k| {
            data.iter()
                .filter_map(|e| e.attributes.get(k))
                .filter(|v| crate::build::truthy(v))
                .map(string)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join("/")
        };
        let rel = terms("relation");
        let rel = if rel.is_empty() {
            "related".into()
        } else {
            rel
        };
        let conf = terms("confidence");
        let conf = if conf.is_empty() {
            conf
        } else {
            format!(" [{conf}]")
        };
        segments.push(if forward {
            format!("--{rel}{conf}--> {}", label(g, v))
        } else {
            format!("<--{rel}{conf}-- {}", label(g, v))
        });
    }
    let prefix = if warnings.is_empty() {
        String::new()
    } else {
        format!("{}\n", warnings.join("\n"))
    };
    Ok(PathAnswer {
        text: format!(
            "{prefix}Shortest path ({hops} hops):\n  {}",
            segments.join(" ")
        ),
        refusal: None,
        hops: Some(hops),
    })
}
#[cfg(test)]
mod tests {
    use crate::{graph::Graph, serve_search::SearchIndex};
    use serde_json::json;

    /// Graphify 0.9.80 (#4192): the cached search adjacency answers as a
    /// freshly built one, in either order of direction modes.
    #[test]
    fn cached_path_adjacency_answers_like_a_fresh_one() {
        let g = Graph::from_node_link_owned(
            json!({"directed": true, "multigraph": false, "graph": {}, "nodes": [
                {"id": "f", "label": "f.py"}, {"id": "a", "label": "a()"}, {"id": "b", "label": "b()"},
                {"id": "c", "label": "c()"},
            ], "links": [
                {"source": "f", "target": "a", "relation": "contains"},
                {"source": "b", "target": "a", "relation": "calls"},
                {"source": "c", "target": "b", "relation": "calls"},
            ]}),
            "links",
        )
        .unwrap();
        let ask = |index: &SearchIndex, source: &str, target: &str, undirected: bool| {
            super::shortest_path(
                &g,
                index,
                &json!({"source": source, "target": target, "undirected": undirected}),
            )
            .unwrap()
        };
        let cached = SearchIndex::new(&g);
        for (source, target, undirected) in [
            ("c()", "f.py", false),
            ("f.py", "c()", false),
            ("f.py", "c()", true),
            ("c()", "f.py", false),
            ("a()", "c()", true),
        ] {
            let fresh = ask(&SearchIndex::new(&g), source, target, undirected);
            assert_eq!(ask(&cached, source, target, undirected), fresh);
        }
        assert!(ask(&cached, "c()", "f.py", false).contains("Shortest path (3 hops)"));
        assert!(ask(&cached, "f.py", "c()", false).starts_with("No directed path found"));
    }
}
