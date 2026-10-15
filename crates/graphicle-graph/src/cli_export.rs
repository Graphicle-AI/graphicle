// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
use crate::{
    Result, build,
    cli::{Args, Output, expand},
    cluster, export,
    graph::Graph,
    paths,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};
fn partition(v: &Value) -> cluster::Communities {
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_array()?.clone())))
        .collect()
}
pub fn run(a: &Args, out: &mut Output) -> Result<()> {
    let kind = a.require(0, "export <format> [--graph PATH]")?;
    if ![
        "html",
        "callflow-html",
        "obsidian",
        "wiki",
        "svg",
        "graphml",
        "neo4j",
        "falkordb",
    ]
    .contains(&kind)
    {
        return Err(format!("Unknown export format: {kind}").into());
    }
    let mut path = a.graph();
    if kind == "callflow-html"
        && a.get("graph").is_none()
        && let Some(p) = a.positional(1)
    {
        let p = expand(p);
        path = if p.extension().is_some_and(|s| s == "json") {
            p
        } else if p.join("graph.json").exists() {
            p.join("graph.json")
        } else {
            p.join(paths::OUT_DIR.as_str()).join("graph.json")
        };
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let labels_path = a.path("labels", dir.join(".graphicle_labels.json"));
    let report_path = a.path("report", dir.join("GRAPH_REPORT.md"));
    if kind == "callflow-html" {
        let r = crate::callflow_html::dispatch(
            &json!({"function":"write_callflow_html","args":[{"graph":path,"report":report_path,"labels":labels_path,"sections":a.get("sections").map(expand),"output":a.get("output").map(expand),"lang":a.value("lang","auto"),"max_sections":a.number("max-sections",15)?,"diagram_scale":a.float("diagram-scale",1.0)?,"max_diagram_nodes":a.number("max-diagram-nodes",18)?,"max_diagram_edges":a.number("max-diagram-edges",24)?,"verbose":true}]}),
        )?;
        out.value(&r);
        return Ok(());
    }
    let over = crate::security::check_graph_file_size_cap(&path).err();
    let above_cap = over.is_some();
    if let Some(e) = over {
        if kind != "html" {
            return Err(e);
        }
        out.warn(format!(
            "warning: {e}; continuing with community aggregation"
        ));
    }
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).map_err(crate::Error::from)?)
        .map_err(crate::Error::from)?;
    let edges_key = if raw.get("links").is_some() {
        "links"
    } else {
        "edges"
    };
    let hyperedges = raw
        .get_mut("hyperedges")
        .filter(|v| v.is_array())
        .map(Value::take);
    // Every export reads direction from the restored markers (0.9.77, #4066).
    let mut g = Graph::from_node_link_owned_with_arcs(raw, edges_key)?;
    if let Some(h) = hyperedges {
        g.attributes["hyperedges"] = h;
    }
    let analysis = crate::install_fs::json_like(&a.path(
        "analysis",
        Path::new(paths::OUT_DIR.as_str()).join(".graphicle_analysis.json"),
    ));
    let mut comm = partition(&analysis["communities"]);
    let mut cohesion = analysis["cohesion"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut gods = analysis["gods"].as_array().cloned().unwrap_or_default();
    let mut fresh = Vec::<(i64, Vec<Value>)>::new();
    for n in &g.nodes {
        let cid = n
            .attributes
            .get("community")
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
        if let Some(cid) = cid {
            if let Some((_, ns)) = fresh.iter_mut().find(|(c, _)| *c == cid) {
                ns.push(n.id.clone());
            } else {
                fresh.push((cid, vec![n.id.clone()]));
            }
        }
    }
    let equivalent = |cs: &cluster::Communities| {
        let mut sets = cs
            .iter()
            .map(|(_, ns)| {
                let mut ns = ns.iter().map(Value::to_string).collect::<Vec<_>>();
                ns.sort();
                ns
            })
            .collect::<Vec<_>>();
        sets.sort();
        sets
    };
    if comm.is_empty() {
        comm = fresh;
    } else if !fresh.is_empty() && equivalent(&comm) != equivalent(&fresh) {
        out.warn("warning: analysis sidecar is stale; reconstructing communities from graph.json");
        comm = fresh;
        cohesion = cluster::score_all(&g, &comm)
            .as_object()
            .cloned()
            .unwrap_or_default();
        gods = crate::analyze::god_nodes(&g, 10, None)?;
    }
    let labels = crate::install_fs::json_like(&labels_path)
        .as_object()
        .cloned()
        .unwrap_or_default();
    let map: BTreeMap<i64, Value> = labels
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
        .collect();
    let mut d = build::Diagnostics::default();
    match kind {
        "html" => {
            let target = a.path("output", dir.join("graph.html"));
            if a.flag("no-viz") {
                crate::install_fs::remove(&target)?;
                out.line("--no-viz: skipped graph.html");
            } else {
                let limit = if above_cap {
                    5000
                } else {
                    a.number("node-limit", 5000)?
                };
                if crate::exporters::html::to_html(
                    &g,
                    &comm,
                    &target,
                    &crate::exporters::html::HtmlOptions {
                        community_labels: map,
                        node_limit: Some(limit),
                        ..Default::default()
                    },
                    &mut d,
                )? {
                    out.line(format!("graph.html written: {}", target.display()));
                }
            }
        }
        "obsidian" => {
            let target = a.path("dir", Path::new(paths::OUT_DIR.as_str()).join("obsidian"));
            let n = export::to_obsidian(
                &g,
                &comm,
                &target,
                &export::VaultOptions {
                    community_labels: map.clone(),
                    cohesion: cohesion
                        .iter()
                        .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_f64()?)))
                        .collect(),
                    ..Default::default()
                },
                &mut d,
            )?;
            export::to_canvas(&g, &comm, &target.join("graph.canvas"), &map, None, None)?;
            out.line(format!(
                "Obsidian vault: {n} notes in {}\nCanvas: {}",
                target.display(),
                target.join("graph.canvas").display()
            ));
        }
        "wiki" => {
            if comm.is_empty() {
                return Err("community data is missing or empty — refusing to export wiki to prevent data loss".into());
            }
            if gods.is_empty() {
                gods = crate::analyze::god_nodes(&g, 10, None)?;
            }
            let target = a.path("output", dir.join("wiki"));
            let (n, messages) = crate::wiki::to_wiki(
                &g,
                &cluster::communities_to_json(&comm),
                &target,
                &labels,
                &cohesion,
                &gods,
            )?;
            for m in messages {
                out.warn(m);
            }
            out.line(format!(
                "Wiki: {n} articles written to {}",
                target.display()
            ));
        }
        "svg" => {
            let target = a.path("output", dir.join("graph.svg"));
            export::to_svg(&g, &comm, &target, &map, (20.0, 14.0))?;
            out.line(format!("graph.svg written: {}", target.display()));
        }
        "graphml" => {
            let target = a.path("output", dir.join("graph.graphml"));
            export::to_graphml(&g, &comm, &target)?;
            out.line(format!("graph.graphml written: {}", target.display()));
        }
        "neo4j" | "falkordb" => {
            if let Some(uri) = a.get("push") {
                let password = a.get("password").map(str::to_string).or_else(|| {
                    std::env::var(if kind == "neo4j" {
                        "NEO4J_PASSWORD"
                    } else {
                        "FALKORDB_PASSWORD"
                    })
                    .ok()
                });
                let result = if kind == "neo4j" {
                    crate::exporters::graphdb::push_to_neo4j(
                        &g,
                        uri,
                        a.value("user", "neo4j"),
                        password
                            .as_deref()
                            .ok_or("--password or NEO4J_PASSWORD required for --push")?,
                        &comm,
                    )?
                } else {
                    crate::exporters::graphdb::push_to_falkordb(
                        &g,
                        uri,
                        a.get("user"),
                        password.as_deref(),
                        &comm,
                        a.value("graph-name", "graphicle"),
                    )?
                };
                out.value(&serde_json::to_value(result).map_err(crate::Error::from)?);
            } else {
                let target = a.path("output", dir.join("cypher.txt"));
                export::to_cypher(&g, &target)?;
                out.line(format!("cypher.txt written: {}", target.display()));
            }
        }
        _ => unreachable!(),
    }
    out.diagnostics(d);
    Ok(())
}
