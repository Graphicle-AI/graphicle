// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
use crate::{
    Result, build,
    cli::{Args, Output, expand, load},
    cli_state::{ar, read_graph},
    graph::Graph,
    paths,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};
pub fn compose(left: &mut Graph, right: &Graph) -> Result<()> {
    if left.directed != right.directed || left.multigraph != right.multigraph {
        return Err("All graphs must have the same graph type".into());
    }
    left.attributes
        .as_object_mut()
        .unwrap()
        .extend(right.attributes.as_object().cloned().unwrap_or_default());
    for n in &right.nodes {
        left.add_node(n.id.clone(), n.attributes.clone())?;
    }
    for (u, v, e) in right.iter_edges() {
        left.add_edge(
            right.nodes[u].id.clone(),
            right.nodes[v].id.clone(),
            right.multigraph.then(|| e.key.clone()),
            e.attributes.clone(),
        )?;
    }
    Ok(())
}
fn simple(g: &Graph) -> Result<Graph> {
    let mut out = Graph::new(false, false);
    out.attributes = g.attributes.clone();
    for n in &g.nodes {
        out.add_node(n.id.clone(), n.attributes.clone())?;
    }
    for (u, v, e) in g.iter_edges() {
        out.add_edge(
            g.nodes[u].id.clone(),
            g.nodes[v].id.clone(),
            None,
            e.attributes.clone(),
        )?;
    }
    Ok(out)
}
pub fn run(command: &str, a: &Args, out: &mut Output) -> Result<()> {
    match command {
        "merge-driver" => {
            a.require(2, "merge-driver <base> <current> <other>")?;
            let cur = expand(a.positional(1).unwrap());
            let other = expand(a.positional(2).unwrap());
            for p in [&cur, &other] {
                if p.metadata().map_err(crate::Error::from)?.len() > 50 * 1024 * 1024 {
                    return Err("merge graph exceeds 50 MiB cap".into());
                }
            }
            let mut g = paths::load_node_link_graph_owned(read_graph(&cur)?)?;
            let other = paths::load_node_link_graph_owned(read_graph(&other)?)?;
            compose(&mut g, &other)?;
            if g.nodes.len() > 100_000 {
                return Err("merged graph exceeds 100000-node cap".into());
            }
            let mut merged = g.to_node_link("links");
            // The loader stamped _src/_tgt and compose kept them; write the
            // direction back into arc order, as to_json does (#563, #4066).
            for link in merged["links"]
                .as_array_mut()
                .into_iter()
                .flatten()
                .filter_map(Value::as_object_mut)
            {
                let source = link.shift_remove("_src").filter(|v| !v.is_null());
                let target = link.shift_remove("_tgt").filter(|v| !v.is_null());
                if let (Some(source), Some(target)) = (source, target) {
                    link.insert("source".into(), source);
                    link.insert("target".into(), target);
                }
            }
            crate::cli_state::write_json_compact(&cur, &merged).map_err(crate::Error::from)?;
        }
        "merge-graphs" => {
            if a.pos.len() < 2 {
                return Err(format!(
                    "Usage: {} merge-graphs <graph1> <graph2> [...] --out PATH [--previous previous-merged-graph.json]",
                    crate::cli::program()
                )
                .into());
            }
            // Each input numbers its own communities, so the merged output
            // carries per-source ids that have no relation to the previous
            // MERGED clustering the labels file was written against (#3858).
            // --previous restores that clustering onto every surviving node.
            let previous = a.get("previous").map(expand);
            let mut previous_community =
                std::collections::BTreeMap::<crate::python_compat::ScalarKey, Value>::new();
            if let Some(prev) = &previous {
                if !prev.exists() {
                    return Err(
                        format!("--previous file not found: {}", prev.to_string_lossy()).into(),
                    );
                }
                let raw = std::fs::read(prev)
                    .map_err(|e| e.to_string())
                    .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
                    .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
                    .map_err(|e| {
                        format!(
                            "--previous file is not readable JSON: {} ({e})",
                            prev.to_string_lossy()
                        )
                    })?;
                for n in raw
                    .get("nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    // Python's isinstance(_, int) also admits bools.
                    let community = n
                        .get("community")
                        .filter(|c| c.is_i64() || c.is_u64() || c.is_boolean());
                    let id = n
                        .get("id")
                        .filter(|id| crate::build_normalize::truthy(id))
                        .and_then(crate::python_compat::scalar_key);
                    if let (Some(c), Some(id)) = (community, id) {
                        previous_community.insert(id, c.clone());
                    }
                }
            }
            let names = a
                .pos
                .iter()
                .map(|s| expand(s).to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let tags = build::distinct_repo_tags(&names);
            let mut g = Graph::new(false, false);
            let mut hs = vec![];
            let mut offset = 0;
            for (path, tag) in names.iter().zip(&tags) {
                let one = load(Path::new(path), None)?;
                let prefixed = simple(&build::prefix_graph_for_global(&one, tag, offset)?)?;
                hs.extend_from_slice(ar(prefixed
                    .attributes
                    .get("hyperedges")
                    .unwrap_or(&Value::Null)));
                for n in &prefixed.nodes {
                    if let Some(cid) = n.attributes.get("community").and_then(Value::as_i64) {
                        offset = offset.max(cid + 1);
                    }
                }
                compose(&mut g, &prefixed)?;
            }
            let shared = crate::cross_repo_types::link_shared_type_declarations(&mut g)?;
            let calls = crate::cross_repo_calls::link_cross_repo_member_calls(&mut g)?;
            if shared > 0 {
                out.line(format!(
                    "  linked {shared} type declaration(s) shared across repos"
                ));
            }
            if calls > 0 {
                out.line(format!("  resolved {calls} member call(s) across repos"));
            }
            if let Some(prev) = &previous {
                let mut restored = 0;
                for n in &mut g.nodes {
                    let key = crate::python_compat::scalar_key(&n.id);
                    match key.and_then(|k| previous_community.get(&k)) {
                        Some(c) => {
                            n.attributes.insert("community".into(), c.clone());
                            restored += 1;
                        }
                        None => {
                            n.attributes.shift_remove("community");
                        }
                    }
                }
                out.line(format!(
                    "  restored the previous merged community on {restored} node(s) from {}",
                    prev.to_string_lossy()
                ));
            }
            g.attributes
                .as_object_mut()
                .unwrap()
                .shift_remove("hyperedges");
            if !hs.is_empty() {
                crate::export::attach_hyperedges(&mut g, &hs)?;
            }
            let mut data = g.to_node_link("links");
            for link in data["links"].as_array_mut().unwrap() {
                let obj = link.as_object_mut().unwrap();
                let u = obj.shift_remove("_src");
                let v = obj.shift_remove("_tgt");
                if let (Some(u), Some(v)) = (u.filter(|v| !v.is_null()), v.filter(|v| !v.is_null()))
                {
                    obj.insert("source".into(), u);
                    obj.insert("target".into(), v);
                }
            }
            data["hyperedges"] = g.attributes.get("hyperedges").cloned().unwrap_or(json!([]));
            let target = a.path(
                "out",
                Path::new(paths::OUT_DIR.as_str()).join("merged-graph.json"),
            );
            crate::cli_state::write_json_compact(&target, &data).map_err(crate::Error::from)?;
            out.line(format!(
                "Merged {} graphs -> {} nodes, {} edges\nWritten to: {}",
                names.len(),
                g.nodes.len(),
                g.edge_count(),
                target.display()
            ));
        }
        "merge-chunks" | "merge-semantic" => {
            let target = expand(a.get("out").ok_or("--out <path> required")?);
            let chunks = command == "merge-chunks";
            let mut files = vec![];
            if chunks {
                for raw in &a.pos {
                    let mut expanded = glob::glob(raw)
                        .map_err(crate::Error::from)?
                        .filter_map(std::result::Result::ok)
                        .collect::<Vec<_>>();
                    expanded.sort();
                    if expanded.is_empty() {
                        files.push(expand(raw));
                    } else {
                        files.extend(expanded);
                    }
                }
            } else {
                for k in ["cached", "new"] {
                    if let Some(p) = a.get(k).map(expand).filter(|p| p.exists()) {
                        files.push(p);
                    }
                }
            }
            let total = files.len();
            let (mut ns, mut es, mut hs, mut seen, mut valid, mut input, mut output) =
                (vec![], vec![], vec![], BTreeSet::new(), 0, 0.0, 0.0);
            for path in &files {
                let v = if chunks {
                    let (v, errors) = crate::semantic_cleanup::load_validated_semantic_fragment(
                        path,
                        &Default::default(),
                    )?;
                    if !errors.errors.is_empty() {
                        out.warn(format!(
                            "[graphicle merge-chunks] warning: skipping invalid chunk {}: {}",
                            path.display(),
                            errors
                                .errors
                                .iter()
                                .take(3)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("; ")
                        ));
                        continue;
                    }
                    v.ok_or("missing validated fragment")?
                } else {
                    read_graph(path)?
                };
                valid += 1;
                for n in ar(&v["nodes"]) {
                    if seen.insert(n["id"].to_string()) {
                        ns.push(n.clone());
                    }
                }
                es.extend_from_slice(ar(&v["edges"]));
                hs.extend_from_slice(ar(&v["hyperedges"]));
                input += v["input_tokens"].as_f64().unwrap_or(0.0);
                output += v["output_tokens"].as_f64().unwrap_or(0.0);
            }
            if chunks && valid == 0 {
                return Err(format!(
                    "no valid chunks to merge; refusing to write {}",
                    target.display()
                )
                .into());
            }
            let mut data = json!({"nodes":ns,"edges":es,"hyperedges":hs});
            if chunks {
                data["input_tokens"] = json!(input);
                data["output_tokens"] = json!(output);
            }
            crate::install_fs::write(&target, &crate::serve_text::json_text(&data, None, false))?;
            out.line(if chunks{format!("Merged {valid} of {total} chunks: {} nodes, {} edges, {input} in / {output} out tokens",ar(&data["nodes"]).len(),ar(&data["edges"]).len())}else{format!("Merged: {} nodes, {} edges",ar(&data["nodes"]).len(),ar(&data["edges"]).len())});
        }
        _ => return Err("Unknown merge command".into()),
    }
    Ok(())
}
