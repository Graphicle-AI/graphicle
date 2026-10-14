// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/export.py
use crate::{Result, build::Diagnostics, export::*, exporters::common::*, graph::Graph};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |k: &str| r[k].as_str().unwrap_or("");
    let mut d = Diagnostics::default();
    let limit = r["limit"].as_i64().unwrap_or(200);
    let value = match s("function") {
        "_obsidian_tag" => json!(obsidian_tag(s("name"))),
        "_strip_diacritics" => json!(strip_diacritics(&r["text"])),
        "_yaml_str" => json!(yaml_str(&r["s"])),
        "_cypher_escape" => json!(cypher_escape(s("s"))),
        "_cypher_label" => json!(cypher_label(s("raw"), s("fallback"))),
        "_strip_xml_illegal" => json!(strip_xml_illegal(s("s"))),
        "_cap_filename" => json!(cap_filename(s("s"), limit)),
        "_obsidian_safe_stem" => json!(obsidian_safe_stem(s("label"), limit)),
        "_is_generated_note" => json!(is_generated_note(Path::new(s("path")))),
        "_adopt_pre_manifest_notes" => json!(adopt_pre_manifest_notes(Path::new(s("out")))),
        "_git_head" => json!(git_head(r["cwd"].as_str().map(Path::new))),
        "backup_if_protected" => json!(backup_if_protected(Path::new(s("out_dir")), &mut d)),
        "existing_graph_node_count" => match existing_graph_node_count(Path::new(s("path"))) {
            ExistingGraph::Nothing => Value::Null,
            ExistingGraph::Count(n) => json!(n),
            ExistingGraph::Malformed => json!({"malformed":true}),
        },
        "prune_dangling_edges" => {
            let mut data = r["graph_data"].clone();
            let count = prune_dangling_edges(&mut data)?;
            json!([data, count])
        }
        function => {
            let mut graph = Graph::from_node_link(&r["G"], "edges")?;
            if let Some(order) = r.get("neighbor_order") {
                graph.restore_neighbor_order(order)?;
            }
            let comm = communities(r)?;
            let path = Path::new(s("output_path"));
            let labels = labels(&r["community_labels"])?;
            match function {
                "attach_hyperedges" => {
                    attach_hyperedges(
                        &mut graph,
                        r["hyperedges"].as_array().ok_or("Expected hyperedges")?,
                    )?;
                    graph.to_node_link("edges")
                }
                "_dedup_node_filenames" => {
                    let names = dedup_node_filenames(&graph, |s| obsidian_safe_stem(s, limit))?;
                    json!(
                        graph
                            .nodes
                            .iter()
                            .zip(names)
                            .map(|(n, s)| (string(&n.id), s))
                            .collect::<BTreeMap<_, _>>()
                    )
                }
                "to_json" => json!(to_json(
                    &graph,
                    &comm,
                    path,
                    &JsonOptions {
                        force: r["force"].as_bool().unwrap_or(false),
                        built_at_commit: r["built_at_commit"].as_str().map(str::to_owned),
                        community_labels: labels,
                        original_links: r["original_links"].as_array().cloned(),
                    },
                    &mut d
                )?),
                "to_cypher" => {
                    to_cypher(&graph, path)?;
                    Value::Null
                }
                "to_graphml" => {
                    to_graphml(&graph, &comm, path)?;
                    Value::Null
                }
                "to_svg" => {
                    to_svg(
                        &graph,
                        &comm,
                        path,
                        &labels,
                        (
                            r["figsize"][0].as_f64().unwrap_or(20.0),
                            r["figsize"][1].as_f64().unwrap_or(14.0),
                        ),
                    )?;
                    Value::Null
                }
                "spring_layout" => json!(crate::export_formats::spring_layout(&graph)?),
                "to_obsidian" => json!(to_obsidian(
                    &graph,
                    &comm,
                    Path::new(s("output_dir")),
                    &VaultOptions {
                        community_labels: labels,
                        cohesion: r["cohesion"]
                            .as_object()
                            .map(|v| v
                                .iter()
                                .map(|(k, v)| Ok((
                                    k.parse::<i64>().map_err(|_| "Invalid community ID")?,
                                    v.as_f64().ok_or("Expected cohesion number")?
                                )))
                                .collect::<Result<_>>())
                            .transpose()?
                            .unwrap_or_default(),
                        stem_limit: r["stem_limit"].as_i64(),
                        community_stem_limit: r["community_stem_limit"].as_i64()
                    },
                    &mut d
                )?),
                "to_canvas" => {
                    let names = r["node_filenames"]
                        .as_object()
                        .map(|v| {
                            v.iter()
                                .map(|(k, v)| {
                                    Ok((k.clone(), v.as_str().ok_or("Expected filename")?.into()))
                                })
                                .collect::<Result<BTreeMap<_, _>>>()
                        })
                        .transpose()?;
                    to_canvas(
                        &graph,
                        &comm,
                        path,
                        &labels,
                        names.as_ref(),
                        r["stem_limit"].as_i64(),
                    )?;
                    Value::Null
                }
                _ => return Err("Unknown graph export operation".into()),
            }
        }
    };
    Ok(json!({"value":value,"stdout":d.stdout,"stderr":d.stderr}))
}
