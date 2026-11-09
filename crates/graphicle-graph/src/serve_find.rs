// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
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
/// Owner to member relations a `Type.Member` query walks: `method` for
/// methods, `defines` for fields and in-class declarations, `contains` for
/// nested types (Graphify 0.9.99, #4884).
const TYPE_MEMBER_RELATIONS: [&str; 3] = ["method", "defines", "contains"];
/// `Type.Member`, `Outer.Inner.Member` or `Type.Member()`.
static QUALIFIED_MEMBER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\.([A-Za-z_]\w*)(?:\(\))?$").unwrap()
});
/// `_symbol_name`: a node's label folded to its bare name, so the member
/// `.Build()` reads `build`.
fn symbol_name(g: &Graph, i: usize) -> String {
    let nl = norm_label(g, i);
    let nl = nl.trim();
    let nl = nl.strip_prefix('.').unwrap_or(nl);
    nl.strip_suffix("()").unwrap_or(nl).to_owned()
}
/// `_split_qualified_member`: the folded `(owner chain, member)` of a
/// `Type.Member` query, or `None` for another form.
fn split_qualified_member(label: &str) -> Option<(Vec<String>, String)> {
    let qualified = QUALIFIED_MEMBER.captures(label.trim())?;
    let owners = qualified[1]
        .split('.')
        .map(|p| fold(p).to_lowercase())
        .collect();
    Some((owners, fold(&qualified[2]).to_lowercase()))
}
fn named_nodes(g: &Graph, index: &SearchIndex, name: &str) -> Vec<usize> {
    index
        .candidates(&[name.to_owned()], 0.1)
        .unwrap_or_else(|| (0..g.nodes.len()).collect())
        .into_iter()
        .filter(|&i| symbol_name(g, i) == name)
        .collect()
}
/// `_membership_neighbors`: the members of `node` (`members`) or the nodes
/// that own it. Direction comes from the edge's `_src`/`_tgt` markers, else
/// the stored arc of a directed graph; an undirected edge without markers
/// says nothing about which end owns the other and is skipped.
fn membership_neighbors(g: &Graph, node: usize, members: bool) -> Vec<usize> {
    let mut found: Vec<usize> = vec![];
    let mut visit = |u: usize, v: usize, e: &crate::graph::Edge| {
        let relation = e.attributes.get("relation").and_then(Value::as_str);
        if !relation.is_some_and(|r| TYPE_MEMBER_RELATIONS.contains(&r)) {
            return;
        }
        let (owner, member) = match (e.attributes.get("_src"), e.attributes.get("_tgt")) {
            (Some(a), Some(b)) => (g.node_index(a), g.node_index(b)),
            _ if g.is_directed() => (Some(u), Some(v)),
            _ => return,
        };
        let (Some(owner), Some(member)) = (owner, member) else {
            return;
        };
        if (if members { owner } else { member }) != node {
            return;
        }
        let other = if members { member } else { owner };
        if other != node && !found.contains(&other) {
            found.push(other);
        }
    };
    let me = &g.nodes[node].id;
    let mut outs: Vec<usize> = g.neighbor_indices(node, false).collect();
    outs.dedup();
    for w in outs {
        for e in g.edges_between(me, &g.nodes[w].id) {
            visit(node, w, e);
        }
    }
    if g.is_directed() {
        let mut ins: Vec<usize> = g.neighbor_indices(node, true).collect();
        ins.dedup();
        for w in ins {
            for e in g.edges_between(&g.nodes[w].id, me) {
                visit(w, node, e);
            }
        }
    }
    found
}
/// `_enclosed_by`: is `node` nested in types named by `outer`, outermost
/// first?
fn enclosed_by(g: &Graph, node: usize, outer: &[String]) -> bool {
    let Some((last, rest)) = outer.split_last() else {
        return true;
    };
    membership_neighbors(g, node, false)
        .into_iter()
        .any(|parent| symbol_name(g, parent) == *last && enclosed_by(g, parent, rest))
}
/// `_qualified_member_owners`: nodes named `qualifiers[-1]` that own
/// members, nested as the rest of the chain says.
fn qualified_member_owners(g: &Graph, index: &SearchIndex, qualifiers: &[String]) -> Vec<usize> {
    let Some((last, outer)) = qualifiers.split_last() else {
        return vec![];
    };
    named_nodes(g, index, last)
        .into_iter()
        .filter(|&owner| {
            !membership_neighbors(g, owner, true).is_empty() && enclosed_by(g, owner, outer)
        })
        .collect()
}
fn members_named(g: &Graph, owners: &[usize], member: &str) -> Vec<usize> {
    let mut hits: Vec<usize> = vec![];
    for &owner in owners {
        for i in membership_neighbors(g, owner, true) {
            if symbol_name(g, i) == member && !hits.contains(&i) {
                hits.push(i);
            }
        }
    }
    hits
}
/// `_names_source_file`: is `label` a file name (`utils.py`) or path some
/// node was extracted from? Case-insensitive, like the source-path tier.
fn names_source_file(g: &Graph, label: &str) -> bool {
    let fold_path = |t: &str| fold(&t.trim().replace('\\', "/")).to_lowercase();
    let query = fold_path(label);
    let suffix = format!("/{query}");
    (0..g.nodes.len()).any(|i| {
        let source = fold_path(&text(field(g, i, "source_file")));
        !source.is_empty() && (source == query || source.ends_with(&suffix))
    })
}
/// `_resolve_qualified_member` (Graphify 0.9.99, #4884): member nodes named
/// by `Type.Member` through their declaring type. A file name such as
/// `config.json` has the same shape and names the file, so it resolves
/// nothing; neither does a namespace qualifier, since the file, not the
/// namespace node, owns the type.
pub fn resolve_qualified_member(g: &Graph, index: &SearchIndex, label: &str) -> Vec<usize> {
    let Some((qualifiers, member)) = split_qualified_member(label) else {
        return vec![];
    };
    if names_source_file(g, label) {
        return vec![];
    }
    members_named(g, &qualified_member_owners(g, index, &qualifiers), &member)
}
/// `_no_member_match_message` (Graphify 0.9.99, #4884): the refusal for a
/// `Type.Member` endpoint whose type declares no such member, since scoring
/// it would land on the type. `None` when the query is a node's own id or
/// label, a file name, or no type of that name owns members. Same-named
/// members of a type with that bare name are listed for a retry.
pub fn no_member_match_message(g: &Graph, index: &SearchIndex, label: &str) -> Option<String> {
    let (qualifiers, member) = split_qualified_member(label)?;
    let term = tokens(label).join(" ");
    let nq = fold(label).to_lowercase().trim().to_string();
    if literal_exact(g, index, &term, &nq) || names_source_file(g, label) {
        return None;
    }
    let bare_owners = qualified_member_owners(g, index, &qualifiers[qualifiers.len() - 1..]);
    if bare_owners.is_empty() {
        return None;
    }
    let trimmed = label.trim();
    let trimmed = trimmed.strip_suffix("()").unwrap_or(trimmed);
    let (owner_text, member_text) = trimmed.rsplit_once('.').unwrap_or(("", trimmed));
    let sanitize = |t: &str| crate::security::sanitize_label(&Value::String(t.to_owned()));
    let mut message = format!(
        "No node matching '{}' found; no type '{}' declares '{}'.",
        sanitize(label),
        sanitize(owner_text),
        sanitize(member_text)
    );
    let near = members_named(g, &bare_owners, &member);
    if !near.is_empty() {
        let listing = near
            .iter()
            .map(|&n| {
                let shown = [field(g, n, "source_file"), field(g, n, "label")]
                    .into_iter()
                    .find(|v| crate::build::truthy(v))
                    .map(string)
                    .unwrap_or_else(|| id(g, n));
                format!("  {shown}\n    id: {}", id(g, n))
            })
            .collect::<Vec<_>>()
            .join("\n");
        message.push_str(&format!("\n{listing}\nRetry with the full node id."));
    }
    Some(message)
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
    // `Type.Member` names a member through its declaring type (Graphify
    // 0.9.99, #4884). The member's own label is `.Build()`, so no tier above
    // matched it and scoring picked the type. Tried only once no node's own
    // id, label or source path matched exactly, so those keep precedence.
    // Returned as the exact tier, so several members under that name are
    // refused with their candidates rather than one of them guessed.
    if out[0].is_empty() && out[1].is_empty() {
        let members = resolve_qualified_member(g, index, query);
        if !members.is_empty() {
            return [vec![], members, vec![], vec![]];
        }
    }
    // The tiers fold case, so in a case-sensitive language `Run` and `run`
    // tie in the exact tier and every lookup is refused, even one spelled
    // exactly like one of them. When the tie is only case and exactly one
    // label is written as queried, it wins; the others stay reachable one
    // tier down (Graphify 0.9.98).
    if out[1].len() > 1 {
        let bare = |s: &str| {
            let s = s.trim().trim_start_matches('.');
            s.strip_suffix("()").unwrap_or(s).to_owned()
        };
        let wanted = bare(query);
        let labels: Vec<String> = out[1]
            .iter()
            .map(|&i| bare(&or_empty(field(g, i, "label"))))
            .collect();
        let same_case: Vec<usize> = out[1]
            .iter()
            .zip(&labels)
            .filter(|(_, l)| **l == wanted)
            .map(|(&i, _)| i)
            .collect();
        if same_case.len() == 1
            && labels
                .iter()
                .all(|l| l.to_lowercase() == wanted.to_lowercase())
        {
            let mut demoted: Vec<usize> = out[1]
                .iter()
                .copied()
                .filter(|&i| i != same_case[0])
                .collect();
            demoted.append(&mut out[2]);
            out[2] = demoted;
            out[1] = same_case;
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
/// `str(value or "")`: the text of a truthy value, else nothing.
fn or_empty(value: &Value) -> String {
    if crate::build::truthy(value) {
        string(value)
    } else {
        String::new()
    }
}
/// One node per distinct file in `tier`, or none for a single file. A file is
/// its repo plus its source_file: a merged graph keeps source_file relative to
/// each repo, so two repos that both have `dup.py` hold two files (Graphify
/// 0.9.93, #4512). A single-repo graph has no `repo` and groups by path alone.
fn rivals_across_files(g: &Graph, tier: &[usize]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let by_source = tier
        .iter()
        .copied()
        .filter(|i| {
            seen.insert((
                or_empty(field(g, *i, "repo")),
                or_empty(field(g, *i, "source_file")),
            ))
        })
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
///
/// A prefix match still resolves (the caller started typing the real name),
/// but a substring-only match is a search hit, not the symbol the caller named:
/// it comes back as a near miss listing the candidates (Graphify 0.9.94,
/// #3917).
pub fn resolve(g: &Graph, index: &SearchIndex, query: &str) -> std::result::Result<usize, String> {
    let [source_exact, exact, prefix, substring] = tiers(g, index, query);
    let file_query = !source_exact.is_empty() && !query.contains("::");
    let Some(tier) = [source_exact, exact, prefix]
        .into_iter()
        .find(|tier| !tier.is_empty())
    else {
        if substring.is_empty() {
            return Err(format!("No node matching '{query}' found."));
        }
        return Err(near_miss_message(g, query, &substring, 5));
    };
    if let Some(refusal) = tier_refusal(g, index, query, &tier, file_query) {
        return Err(refusal);
    }
    Ok(tier[0])
}
/// `_near_miss_message` (Graphify 0.9.94, #3917): `No exact match`, then the
/// substring matches that are the closest names, labelled as such, so the
/// caller retries with the real name instead of trusting a different node.
fn near_miss_message(g: &Graph, query: &str, hits: &[usize], limit: usize) -> String {
    let sanitize = |s: &str| crate::security::sanitize_label(&Value::String(s.into()));
    let name = |i: usize| {
        let label = or_empty(field(g, i, "label"));
        sanitize(&if label.is_empty() { id(g, i) } else { label })
    };
    let source = |i: usize| strip(&or_empty(field(g, i, "source_file"))).to_owned();
    let mut listing = hits
        .iter()
        .take(limit)
        .map(|&i| {
            let location = or_empty(field(g, i, "source_location"));
            let place = [source(i), strip(&location).to_owned()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if place.is_empty() {
                format!("  {}", name(i))
            } else {
                format!("  {}  {}", name(i), sanitize(&place))
            }
        })
        .collect::<Vec<_>>();
    if hits.len() > limit {
        listing.push(format!("  ... and {} more", hits.len() - limit));
    }
    let example = name(hits[0]);
    let example_source = source(hits[0]);
    let example_form = if example_source.is_empty() {
        example
    } else {
        format!("{}::{example}", sanitize(&example_source))
    };
    format!(
        "No exact match for '{}'. These names contain it, but none is called that:\n{}\nRetry with the exact label, a path::symbol form (e.g. {example_form}), or the full node id.",
        sanitize(query),
        listing.join("\n")
    )
}
/// `_query_name_refusal` (Graphify 0.9.93, #4511, #4517): refuse a `query` or
/// `--seed` value only when it is itself the exact name of several nodes, as
/// `explain` and `path` do. A bare keyword that only prefix- or
/// substring-matches labels is a search and still scores.
pub fn query_name_refusal(g: &Graph, index: &SearchIndex, query: &str) -> Option<String> {
    let [source_exact, exact, _, _] = tiers(g, index, query);
    let file_query = !source_exact.is_empty() && !query.contains("::");
    let tier = if source_exact.is_empty() {
        exact
    } else {
        source_exact
    };
    if tier.is_empty() {
        return None;
    }
    tier_refusal(g, index, query, &tier, file_query)
        .filter(|refusal| is_ambiguity_refusal(Some(refusal)))
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
    // The repo is named only when the candidates span several: then the bare
    // path cannot tell them apart (Graphify 0.9.93, #4512).
    let several_repos = rivals
        .iter()
        .map(|i| or_empty(field(g, *i, "repo")))
        .collect::<BTreeSet<_>>()
        .len()
        > 1;
    let listing = rivals
        .iter()
        .map(|i| {
            let sf = or_empty(field(g, *i, "source_file"));
            let repo = field(g, *i, "repo");
            let place = if several_repos && crate::build::truthy(repo) && !sf.is_empty() {
                format!("{}/{sf}", string(repo))
            } else {
                sf
            };
            format!(
                "  {}\n    id: {}",
                if place.is_empty() { id(g, *i) } else { place },
                id(g, *i)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The example names the matched node's own label: echoing the query would
    // nest a path-scoped one (`index.ts::foo()`) behind a second path.
    let own = rivals
        .first()
        .map(|i| or_empty(field(g, *i, "label")))
        .unwrap_or_default();
    let symbol = if own.is_empty() { query } else { own.as_str() };
    // The same path in two repos: `<path>::symbol` matches both, so only the
    // node id tells them apart.
    let sources = rivals
        .iter()
        .map(|i| or_empty(field(g, *i, "source_file")))
        .collect::<Vec<_>>();
    let shared_path = sources
        .iter()
        .any(|src| !src.is_empty() && sources.iter().filter(|s| *s == src).count() > 1);
    let retry = if shared_path {
        "Retry with the full node id: the same path exists in more than one repo, so a path cannot tell them apart.".to_owned()
    } else {
        format!(
            "Retry with path::symbol using one of the paths above (e.g. <path>::{symbol}) or the full node id."
        )
    };
    format!(
        "{AMBIGUOUS}'{query}' matches {} nodes in different files.\n{listing}\n{retry}",
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
    // Likewise a `Type.Member` endpoint whose type exists but declares no
    // such member: scoring resolves it to the type, and the route printed to
    // the type reads as a route to the member (#4884).
    if let Some(message) = no_member_match_message(g, index, query) {
        return (None, vec![], Some(message));
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
    if meeting.is_none() {
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
    }
    // Among every shortest route, the one whose weakest hop is weakest, so
    // the confidence never overstates the connection (Graphify 0.9.99,
    // #4881).
    let (path, routes, capped) = weakest_shortest_path(g, adj, src, tgt);
    let hops = path.len() - 1;
    let max = args["max_hops"].as_i64().unwrap_or(8);
    if hops as i64 > max {
        return Ok(PathAnswer::text(format!(
            "Path exceeds max_hops={max} ({hops} hops found)."
        )));
    }
    let mut segments = vec![label(g, src)];
    let mut hop_datas: Vec<Vec<&crate::record::Attributes>> = Vec::new();
    for pair in path.windows(2) {
        let (v, (forward, data)) = (pair[1], hop_edges(g, pair[0], pair[1]));
        hop_datas.push(data.iter().map(|e| &e.attributes).collect());
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
        // Every provenance record of the hop's edges, each with its score
        // (Graphify 0.9.98-0.9.99, #4881, #4882).
        let conf = data
            .iter()
            .flat_map(|e| crate::build::edge_evidence(&e.attributes))
            .filter(|e| e.get("confidence").is_some_and(crate::build::truthy))
            .map(crate::serve_search::confidence_tag)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("/");
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
    // The path's confidence is that of its weakest hop (Graphify 0.9.98,
    // #4885).
    let mut summary = path_confidence_line(&hop_datas);
    if !summary.is_empty() && routes > 1 {
        let routes = if capped {
            format!("{MAX_SHORTEST_PATHS}+")
        } else {
            routes.to_string()
        };
        summary.push_str(&format!(" (weakest of {routes} equal-length routes)"));
    }
    Ok(PathAnswer {
        text: format!(
            "{prefix}Shortest path ({hops} hops):\n  {}{}",
            segments.join(" "),
            if summary.is_empty() {
                String::new()
            } else {
                format!("\n{summary}")
            }
        ),
        refusal: None,
        hops: Some(hops),
    })
}
/// The rank of a hop in [`path_weakest`]: confidence tier, scoredness,
/// score and hop index.
type WeakestRank = (u8, bool, f64, usize);
/// [`path_weakest`]'s answer: `(rank, hop, relation, confidence or None for
/// no recognised tag, score)`.
type Weakest = (WeakestRank, usize, String, Option<String>, Option<f64>);
/// `_path_weakest` (Graphify 0.9.98-0.9.99, #4885): the weakest hop of a
/// path, ranked by the weakest evidence it carries (confidence tier, then the
/// lowest score; a missing score never counts as weaker than a present one,
/// and EXTRACTED's score is neither ranked nor printed). Returns `(hop,
/// relation, confidence or None for no recognised tag, score)`.
fn path_weakest(hop_datas: &[Vec<&crate::record::Attributes>]) -> Option<Weakest> {
    let empty = crate::record::Attributes::new();
    let mut weakest: Option<Weakest> = None;
    for (index, datas) in hop_datas.iter().enumerate() {
        let hop = index + 1;
        let datas: Vec<&crate::record::Attributes> = if datas.is_empty() {
            vec![&empty]
        } else {
            datas.clone()
        };
        for d in datas {
            for ev in crate::build::edge_evidence(d) {
                let conf = ev.get("confidence");
                let score = match ev.get("confidence_score") {
                    Some(Value::Number(n)) if conf.and_then(Value::as_str) != Some("EXTRACTED") => {
                        n.as_f64().filter(|s| s.is_finite())
                    }
                    _ => None,
                };
                let rank: u8 = match conf.map(string).as_deref() {
                    Some("EXTRACTED") => 3,
                    Some("INFERRED") => 2,
                    Some("AMBIGUOUS") => 1,
                    _ => 0,
                };
                let key = (rank, score.is_none(), score.unwrap_or(0.0), hop);
                let weaker = weakest
                    .as_ref()
                    .is_none_or(|(old, ..)| rank_cmp(&key, old).is_lt());
                if weaker {
                    let relation = d
                        .get("relation")
                        .filter(|v| crate::build::truthy(v))
                        .map(string)
                        .unwrap_or_else(|| "related".into());
                    let conf = (rank > 0).then(|| conf.map(string).unwrap_or_default());
                    weakest = Some((key, hop, relation, conf, score));
                }
            }
        }
    }
    weakest
}
fn rank_cmp(a: &WeakestRank, b: &WeakestRank) -> std::cmp::Ordering {
    a.0.cmp(&b.0)
        .then(a.1.cmp(&b.1))
        .then(a.2.total_cmp(&b.2))
        .then(a.3.cmp(&b.3))
}
/// `_path_confidence_key`: a path's weakest-first sort key; a path with no
/// confidence evidence at all (`None`) sorts below any tagged one.
fn path_confidence_key(hop_datas: &[Vec<&crate::record::Attributes>]) -> Option<WeakestRank> {
    path_weakest(hop_datas).map(|(key, ..)| key)
}
/// The edges of one hop `u -> v` and whether they run that way: the
/// `_src`-marked direction (#2309) as the path renderer reads it, else the
/// reverse edges (`_path_hop_datas`).
fn hop_edges(g: &Graph, u: usize, v: usize) -> (bool, Vec<&crate::graph::Edge>) {
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
    if fwd.is_empty() {
        (false, bwd)
    } else {
        (true, fwd)
    }
}
/// Enough equal-length routes to find the weakest among them without letting
/// a pathological graph enumerate exponentially many (#4881).
const MAX_SHORTEST_PATHS: usize = 256;
/// `_weakest_shortest_path`: NetworkX's `all_shortest_paths` over the search
/// adjacency (`predecessor`, then `_build_paths_from_predecessors`, in their
/// order), bounded at [`MAX_SHORTEST_PATHS`], and the first route with the
/// weakest [`path_confidence_key`]. Returns `(route, routes, capped)`; the
/// caller has already found that a route exists.
fn weakest_shortest_path(
    g: &Graph,
    adj: &[Vec<usize>],
    src: usize,
    tgt: usize,
) -> (Vec<usize>, usize, bool) {
    // nx.predecessor: breadth first, every same-level parent in visit order.
    let mut level_of: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::from([(src, 0)]);
    let mut pred: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::from([(src, vec![])]);
    let mut next_level = vec![src];
    let mut level = 0;
    while !next_level.is_empty() {
        level += 1;
        for v in std::mem::take(&mut next_level) {
            for &w in &adj[v] {
                match level_of.get(&w) {
                    None => {
                        level_of.insert(w, level);
                        pred.insert(w, vec![v]);
                        next_level.push(w);
                    }
                    Some(&l) if l == level => pred.entry(w).or_default().push(v),
                    Some(_) => {}
                }
            }
        }
    }
    // _build_paths_from_predecessors({src}, tgt, pred), stopping one past
    // the cap as upstream does.
    let mut paths: Vec<Vec<usize>> = vec![];
    let mut seen = std::collections::HashSet::from([tgt]);
    let mut stack: Vec<(usize, usize)> = vec![(tgt, 0)];
    let mut top: isize = 0;
    while top >= 0 {
        let t = top as usize;
        let (node, i) = stack[t];
        if node == src {
            paths.push(stack[..=t].iter().rev().map(|(n, _)| *n).collect());
            if paths.len() > MAX_SHORTEST_PATHS {
                break;
            }
        }
        let preds = pred.get(&node).map_or(&[][..], Vec::as_slice);
        if preds.len() > i {
            stack[t].1 = i + 1;
            let next = preds[i];
            if !seen.insert(next) {
                continue;
            }
            top += 1;
            let t = top as usize;
            if t == stack.len() {
                stack.push((next, 0));
            } else {
                stack[t] = (next, 0);
            }
        } else {
            seen.remove(&node);
            top -= 1;
        }
    }
    let capped = paths.len() > MAX_SHORTEST_PATHS;
    paths.truncate(MAX_SHORTEST_PATHS);
    let routes = paths.len();
    let mut best: Option<(Option<WeakestRank>, usize)> = None;
    for (i, path) in paths.iter().enumerate() {
        let hop_datas = path
            .windows(2)
            .map(|pair| {
                hop_edges(g, pair[0], pair[1])
                    .1
                    .into_iter()
                    .map(|e| &e.attributes)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let key = path_confidence_key(&hop_datas);
        let weaker = best.as_ref().is_none_or(|(old, _)| match (&key, old) {
            (None, None) => false,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => rank_cmp(a, b).is_lt(),
        });
        if weaker {
            best = Some((key, i));
        }
    }
    let chosen = best.map_or(0, |(_, i)| i);
    (paths.swap_remove(chosen), routes, capped)
}
/// `_path_confidence_line`: `Path confidence: <tier> (weakest hop N: rel,
/// score S)`, or `unknown` when the weakest hop has no recognised tag.
fn path_confidence_line(hop_datas: &[Vec<&crate::record::Attributes>]) -> String {
    let Some((_, hop, relation, conf, score)) = path_weakest(hop_datas) else {
        return String::new();
    };
    match conf {
        None => format!("Path confidence: unknown (hop {hop}: {relation} has no confidence tag)"),
        Some(conf) => format!(
            "Path confidence: {conf} (weakest hop {hop}: {relation}{})",
            score
                .map(|s| format!(", score {}", format_g(s)))
                .unwrap_or_default()
        ),
    }
}
/// Python's `format(x, "g")`: six significant digits, trailing zeros
/// dropped, scientific notation below 1e-4 and from 1e6.
fn format_g(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{x:.5e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trim = |s: &str| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if !(-4..6).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exponent.abs())
    } else {
        trim(&format!("{:.*}", (5 - exponent) as usize, x))
    }
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
