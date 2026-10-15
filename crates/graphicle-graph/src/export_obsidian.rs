// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/export.py
//! Graphify's Obsidian vault and Canvas exports, including ownership manifests.
use crate::{
    Result, analyze,
    build::{self, Diagnostics},
    cluster::Communities,
    export::*,
    exporters::common::*,
    fsutil,
    graph::Graph,
    graph_algorithms, paths,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Default)]
pub struct VaultOptions {
    pub community_labels: BTreeMap<i64, Value>,
    pub cohesion: BTreeMap<i64, f64>,
    pub stem_limit: Option<i64>,
    pub community_stem_limit: Option<i64>,
}
struct OwnedFiles<'a> {
    out: &'a Path,
    owned: BTreeSet<String>,
    written: Vec<String>,
    skipped: Vec<String>,
}
impl<'a> OwnedFiles<'a> {
    fn write(&mut self, name: &str, content: &str) -> Result<bool> {
        let target = self.out.join(name);
        if target.exists() && !self.owned.contains(name) {
            self.skipped.push(name.into());
            return Ok(false);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(crate::Error::from)?
        }
        paths::write_text_atomic_if_changed(&target, content)?;
        self.written.push(name.into());
        Ok(true)
    }
}
fn node_label(graph: &Graph, i: usize) -> String {
    string(
        graph.nodes[i]
            .attributes
            .get("label")
            .unwrap_or(&graph.nodes[i].id),
    )
}
fn data_text(graph: &Graph, i: usize, key: &str) -> String {
    string(graph.nodes[i].attributes.get(key).unwrap_or(&json!("")))
}
fn dominant_confidence(graph: &Graph, i: usize) -> Value {
    let mut counts: Vec<(Value, usize)> = vec![];
    for (_, _, edge) in graph.out_edges(i) {
        let c = edge
            .attributes
            .get("confidence")
            .cloned()
            .unwrap_or(json!("EXTRACTED"));
        if let Some((_, n)) = counts.iter_mut().find(|(v, _)| *v == c) {
            *n += 1
        } else {
            counts.push((c, 1))
        }
    }
    counts.sort_by_key(|item| std::cmp::Reverse(item.1));
    counts
        .first()
        .map(|(v, _)| v.clone())
        .unwrap_or(json!("EXTRACTED"))
}
pub fn to_obsidian(
    graph: &Graph,
    communities: &Communities,
    out: &Path,
    options: &VaultOptions,
    d: &mut Diagnostics,
) -> Result<usize> {
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    let manifest = out.join(".graphicle_obsidian_manifest.json");
    let prior = fs::read_to_string(&manifest)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let owned = if let Some(prior) = prior {
        prior
            .get("files")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        adopt_pre_manifest_notes(out)
    };
    let mut files = OwnedFiles {
        out,
        owned,
        written: vec![],
        skipped: vec![],
    };
    let assign = assignments(graph, communities);
    let neighbors = graph_algorithms::neighbors(graph);
    let labels = &options.community_labels;
    let stem_limit = options
        .stem_limit
        .unwrap_or_else(|| paths::stem_filename_budget(out, DEDUP_SUFFIX_RESERVE, 200));
    let filenames = dedup_node_filenames(graph, |s| obsidian_safe_stem(s, stem_limit))?;
    let mut count = 0;
    for (i, n) in graph.nodes.iter().enumerate() {
        let label = node_label(graph, i);
        let community = community_name(assign[i], labels);
        let cname = community
            .as_str()
            .ok_or("Community name must be a string")?;
        let ftype = n.attributes.get("file_type").cloned().unwrap_or(json!(""));
        let ft = if truth(&ftype) {
            format!("graphicle/{}", string(&ftype))
        } else {
            "graphicle/document".into()
        };
        let tags = [
            ft,
            format!("graphicle/{}", string(&dominant_confidence(graph, i))),
            format!("community/{}", obsidian_tag(cname)),
        ];
        let mut lines = vec![
            "---".into(),
            format!(
                "source_file: \"{}\"",
                yaml_str(n.attributes.get("source_file").unwrap_or(&json!("")))
            ),
            format!("type: \"{}\"", yaml_str(&ftype)),
            format!("community: \"{}\"", yaml_str(&community)),
        ];
        if let Some(location) = n.attributes.get("source_location").filter(|v| truth(v)) {
            lines.push(format!(
                "location: \"{}\"",
                yaml_str(&json!(string(location)))
            ))
        }
        lines.push("tags:".into());
        lines.extend(tags.iter().map(|t| format!("  - {t}")));
        lines.extend([
            "---".into(),
            String::new(),
            format!("# {label}"),
            String::new(),
        ]);
        let mut adjacent = neighbors[i].clone();
        if !adjacent.is_empty() {
            lines.push("## Connections".into());
            adjacent.sort_by_cached_key(|j| node_label(graph, *j));
            for j in adjacent {
                let e = build::edge_data(graph, &n.id, &graph.nodes[j].id)?;
                let relation = e.get("relation").unwrap_or(&Value::Null);
                let relation = if e.get("relation").is_none() {
                    String::new()
                } else {
                    string(relation)
                };
                let confidence = e.get("confidence").cloned().unwrap_or(json!("EXTRACTED"));
                lines.push(format!(
                    "- [[{}]] - `{relation}` [{}]",
                    filenames[j],
                    string(&confidence)
                ));
            }
            lines.push(String::new());
        }
        lines.push(
            tags.iter()
                .map(|s| format!("#{s}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        if files.write(&format!("{}.md", filenames[i]), &lines.join("\n"))? {
            count += 1
        }
    }
    let mut inter: BTreeMap<i64, Vec<(i64, usize)>> =
        communities.iter().map(|(c, _)| (*c, vec![])).collect();
    for (u, v, _) in graph.iter_edges() {
        if let (Some(a), Some(b)) = (assign[u], assign[v])
            && a != b
        {
            for (x, y) in [(a, b), (b, a)] {
                let row = inter.entry(x).or_default();
                if let Some((_, n)) = row.iter_mut().find(|(c, _)| *c == y) {
                    *n += 1
                } else {
                    row.push((y, 1))
                }
            }
        }
    }
    let reach = |i: usize| {
        neighbors[i]
            .iter()
            .filter_map(|j| assign[*j].filter(|c| Some(*c) != assign[i]))
            .collect::<BTreeSet<_>>()
            .len()
    };
    let comm_limit = options.community_stem_limit.unwrap_or_else(|| {
        paths::stem_filename_budget(
            out,
            DEDUP_SUFFIX_RESERVE + COMMUNITY_PREFIX.len() as i64,
            200,
        )
    });
    let mut community_files = BTreeMap::new();
    let mut used = BTreeSet::new();
    for (cid, _) in communities {
        let name = community_name(Some(*cid), labels);
        let name = name.as_str().ok_or("Community name must be a string")?;
        let base = format!("{COMMUNITY_PREFIX}{}", obsidian_safe_stem(name, comm_limit));
        let mut candidate = base.clone();
        let mut n = 1;
        while used.contains(&candidate.to_lowercase()) {
            candidate = format!("{base}_{n}");
            n += 1
        }
        used.insert(candidate.to_lowercase());
        community_files.insert(*cid, candidate);
    }
    for (cid, all_members) in communities {
        let community = community_name(Some(*cid), labels);
        let cname = community
            .as_str()
            .ok_or("Community name must be a string")?;
        let members = all_members
            .iter()
            .filter_map(|id| graph.node_index(id))
            .collect::<Vec<_>>();
        let size = members.len();
        let coh = options.cohesion.get(cid).copied();
        let mut lines = vec!["---".into(), "type: community".into()];
        if let Some(c) = coh {
            lines.push(format!("cohesion: {c:.2}"))
        }
        lines.extend([
            format!("members: {size}"),
            "---".into(),
            String::new(),
            format!("# {cname}"),
            String::new(),
        ]);
        if let Some(c) = coh {
            let desc = if c >= 0.7 {
                "tightly connected"
            } else if c >= 0.4 {
                "moderately connected"
            } else {
                "loosely connected"
            };
            lines.push(format!("**Cohesion:** {c:.2} - {desc}"))
        }
        lines.extend([
            format!("**Members:** {size} nodes"),
            String::new(),
            "## Members".into(),
        ]);
        let mut sorted = members.clone();
        sorted.sort_by_cached_key(|i| node_label(graph, *i));
        for i in sorted {
            let mut entry = format!("- [[{}]]", filenames[i]);
            for key in ["file_type", "source_file"] {
                if graph.nodes[i].attributes.get(key).is_some_and(truth) {
                    entry.push_str(&format!(" - {}", data_text(graph, i, key)))
                }
            }
            lines.push(entry)
        }
        lines.extend([
            String::new(),
            "## Live Query (requires Dataview plugin)".into(),
            String::new(),
            "```dataview".into(),
            format!(
                "TABLE source_file, type FROM #community/{}",
                obsidian_tag(cname)
            ),
            "SORT file.name ASC".into(),
            "```".into(),
            String::new(),
        ]);
        if let Some(cross) = inter.get(cid).filter(|v| !v.is_empty()) {
            lines.push("## Connections to other communities".into());
            let mut cross = cross.clone();
            cross.sort_by_key(|item| std::cmp::Reverse(item.1));
            for (other, n) in cross {
                let otherfile = community_files.get(&other).cloned().unwrap_or_else(|| {
                    format!(
                        "{COMMUNITY_PREFIX}{}",
                        obsidian_safe_stem(
                            &string(&community_name(Some(other), labels)),
                            comm_limit
                        )
                    )
                });
                lines.push(format!(
                    "- {n} edge{} to [[{otherfile}]]",
                    if n == 1 { "" } else { "s" }
                ));
            }
            lines.push(String::new());
        }
        let mut bridges = members
            .iter()
            .filter_map(|i| {
                let r = reach(*i);
                (r > 0).then_some((*i, analyze::degree(graph, *i), r))
            })
            .collect::<Vec<_>>();
        bridges.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        bridges.truncate(5);
        if !bridges.is_empty() {
            lines.push("## Top bridge nodes".into());
            for (i, degree, reach) in bridges {
                lines.push(format!(
                    "- [[{}]] - degree {degree}, connects to {reach} {}",
                    filenames[i],
                    if reach == 1 {
                        "community"
                    } else {
                        "communities"
                    }
                ));
            }
        }
        if files.write(&format!("{}.md", community_files[cid]), &lines.join("\n"))? {
            count += 1
        }
    }
    let groups=labels.iter().map(|(c,label)|->Result<Value>{let label=label.as_str().ok_or("Community name must be a string")?;let color=COMMUNITY_COLORS[c.rem_euclid(10) as usize];Ok(json!({"query":format!("tag:#community/{}",obsidian_tag(label)),"color":{"a":1,"rgb":u32::from_str_radix(color.trim_start_matches('#'),16).unwrap()}}))}).collect::<Result<Vec<_>>>()?;
    files.write(
        ".obsidian/graph.json",
        &json_text(&json!({"colorGroups":groups}), Some(2), true)?,
    )?;
    let written = files.written.iter().cloned().collect::<BTreeSet<_>>();
    let skipped = files.skipped.iter().cloned().collect::<BTreeSet<_>>();
    let root = fsutil::resolve_missing(out).map_err(crate::Error::from)?;
    let mut pruned = 0;
    for name in files
        .owned
        .difference(&written)
        .filter(|n| !skipped.contains(*n))
    {
        let Ok(target) = fsutil::resolve_missing(&out.join(name)) else {
            continue;
        };
        if target == root || !target.starts_with(&root) {
            continue;
        }
        match fs::remove_file(target) {
            Ok(()) => pruned += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => pruned += 1,
            Err(_) => (),
        }
    }
    if pruned > 0 {
        d.stderr.push(format!(
            "[graphicle] pruned {pruned} note(s) for nodes no longer in the graph"
        ))
    }
    let _ = fs::write(
        &manifest,
        json_text(&json!({"files":written}), Some(2), true)?,
    );
    if !files.skipped.is_empty() {
        let mut shown = files
            .skipped
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if files.skipped.len() > 5 {
            shown.push_str(&format!(" (+{} more)", files.skipped.len() - 5))
        };
        d.stderr.push(format!("[graphicle] WARNING: skipped {} pre-existing file(s) graphicle did not create, to avoid overwriting your notes: {shown}. Export into an empty directory (or the default .graphicle/obsidian) to get the full vault.",files.skipped.len()));
    }
    Ok(count)
}
pub fn to_canvas(
    graph: &Graph,
    communities: &Communities,
    output: &Path,
    labels: &BTreeMap<i64, Value>,
    node_filenames: Option<&BTreeMap<String, String>>,
    stem_limit: Option<i64>,
) -> Result<()> {
    let limit = stem_limit.unwrap_or_else(|| {
        paths::stem_filename_budget(
            output.parent().unwrap_or(Path::new(".")),
            DEDUP_SUFFIX_RESERVE,
            200,
        )
    });
    let generated = if node_filenames.is_none() {
        Some(dedup_node_filenames(graph, |s| {
            obsidian_safe_stem(s, limit)
        })?)
    } else {
        None
    };
    let filenames = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let name = if let Some(names) = node_filenames {
                names.get(&string(&n.id)).cloned()
            } else {
                Some(generated.as_ref().unwrap()[i].clone())
            };
            name.map(|name| (i, name))
        })
        .collect::<BTreeMap<_, _>>();
    let fallback = vec![(
        0,
        graph.nodes.iter().map(|n| json!(string(&n.id))).collect(),
    )];
    let communities = if communities.is_empty() && !graph.nodes.is_empty() {
        &fallback
    } else {
        communities
    };
    let count = communities.len();
    let cols = if count == 0 {
        1
    } else {
        (count as f64).sqrt().ceil() as usize
    };
    let rows = count.div_ceil(cols).max(1);
    let mut sorted = communities.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|(c, _)| *c);
    let mut sizes = BTreeMap::new();
    let mut grids = BTreeMap::new();
    let mut members = BTreeMap::new();
    for (cid, ids) in &sorted {
        let row = ids
            .iter()
            .filter_map(|id| graph.node_index(id))
            .filter(|i| filenames.contains_key(i))
            .collect::<Vec<_>>();
        let n = row.len();
        let inner = (n as f64).sqrt().ceil().max(1.0) as usize;
        let w = (220 * inner).max(600);
        let h = (100 * n.div_ceil(inner) + 120).max(400);
        sizes.insert(*cid, (w, h));
        grids.insert(*cid, inner);
        members.insert(*cid, row);
    }
    let mut widths = vec![0; cols];
    let mut heights = vec![0; rows];
    for (idx, (cid, _)) in sorted.iter().enumerate() {
        let (w, h) = sizes[cid];
        widths[idx % cols] = widths[idx % cols].max(w);
        heights[idx / cols] = heights[idx / cols].max(h)
    }
    let mut nodes = vec![];
    let mut edges = vec![];
    for (idx, (cid, _)) in sorted.iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let gx = widths[..col].iter().sum::<usize>() + col * 80;
        let gy = heights[..row].iter().sum::<usize>() + row * 80;
        let (w, h) = sizes[cid];
        nodes.push(json!({"id":format!("g{cid}"),"type":"group","label":community_name(Some(*cid),labels),"x":gx,"y":gy,"width":w,"height":h,"color":(idx%6+1).to_string()}));
        let mut row = members[cid].clone();
        row.sort_by_cached_key(|i| node_label(graph, *i));
        for (j, i) in row.iter().enumerate() {
            let x = gx + 20 + (j % grids[cid]) * 200;
            let y = gy + 80 + (j / grids[cid]) * 80;
            nodes.push(json!({"id":format!("n_{}",string(&graph.nodes[*i].id)),"type":"file","file":format!("{}.md",filenames[i]),"x":x,"y":y,"width":180,"height":60}));
        }
    }
    let mut all = BTreeSet::new();
    for (_, ids) in communities {
        for id in ids {
            if let Some(i) = graph.node_index(id) {
                all.insert(i);
            }
        }
    }
    let mut weighted = vec![];
    for (u, v, e) in graph.iter_edges() {
        // JSON Canvas draws the arrowhead at toNode: use the stored direction.
        let end = |marker: &str, i: usize| match e.attributes.get(marker) {
            Some(id) => graph.node_index(id),
            None => Some(i),
        };
        let (Some(u), Some(v)) = (end("_src", u), end("_tgt", v)) else {
            continue;
        };
        if all.contains(&u) && all.contains(&v) {
            let w = e
                .attributes
                .get("weight")
                .map(|v| v.as_f64().ok_or("Canvas edge weight must be numeric"))
                .transpose()?
                .unwrap_or(1.0);
            let r = e.attributes.get("relation").cloned().unwrap_or(json!(""));
            let c = e
                .attributes
                .get("confidence")
                .cloned()
                .unwrap_or(json!("EXTRACTED"));
            let label = if truth(&r) {
                format!("{} [{}]", string(&r), string(&c))
            } else {
                format!("[{}]", string(&c))
            };
            weighted.push((w, u, v, label));
        }
    }
    weighted.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, u, v, label) in weighted.into_iter().take(200) {
        let u = string(&graph.nodes[u].id);
        let v = string(&graph.nodes[v].id);
        edges.push(json!({"id":format!("e_{u}_{v}"),"fromNode":format!("n_{u}"),"toNode":format!("n_{v}"),"label":label}));
    }
    paths::write_json_atomic(output, &json!({"nodes":nodes,"edges":edges}), Some(2), true)
        .map_err(crate::Error::from)
}
