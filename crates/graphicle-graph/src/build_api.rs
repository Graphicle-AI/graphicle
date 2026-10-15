// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py
use crate::{Result, build::*, graph::Graph, hyperedge_members, paths};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
fn graph(request: &Value) -> Result<Graph> {
    Graph::from_node_link(&request["G"], "edges")
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |k: &str| r[k].as_str().unwrap_or("");
    let root = r["root"].as_str().map(Path::new);
    let path = Path::new(s("graph_path"));
    let array = |k: &str| r[k].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let strings = |k: &str| -> Vec<String> {
        array(k)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let mut messages = Diagnostics::default();
    let result = match s("function") {
        "_is_ast_tier" => Ok(json!(is_ast_tier(&r["item"]))),
        "_is_external_stub" => Ok(json!(is_external_stub(&r["item"]))),
        "_backfill_origin" => {
            let mut item = r["item"].clone();
            backfill_origin(&mut item);
            Ok(item)
        }
        "_execution_wins" => {
            let (mut a, mut b) = (r["a"].clone(), r["b"].clone());
            execution_wins(&mut a, &mut b);
            Ok(json!([a, b]))
        }
        "finalize_raw_graph_endpoints" => {
            let mut data = r["data"].clone();
            finalize_raw_graph_endpoints(&mut data);
            Ok(data)
        }
        "_lost_saved_semantic_nodes" => Ok(json!(lost_saved_semantic_nodes(
            array("saved_nodes"),
            array("new_nodes"),
            &strings("deleted_sources")
        ))),
        "mint_external_stubs_in_data" => {
            let mut data = r["data"].clone();
            mint_external_stubs_in_data(&mut data);
            Ok(data)
        }
        "_mint_external_stub" => {
            let mut g = graph(r)?;
            mint_external_stub(&mut g, s("nid"))?;
            Ok(g.to_node_link("edges"))
        }
        "_coerce_hyperedge_member_refs" => {
            let mut he = r["he"].clone();
            he["nodes"] = r["members"].clone();
            messages
                .stderr
                .extend(hyperedge_members::normalize(&mut he));
            Ok(he["nodes"].clone())
        }
        "_normalize_hyperedge_members" => {
            let mut he = r["he"].clone();
            messages
                .stderr
                .extend(hyperedge_members::normalize(&mut he));
            Ok(he)
        }
        "_fold_node_aliases" => {
            let mut node = r["node"].clone();
            fold_node_aliases(&mut node);
            Ok(node)
        }
        "_fold_edge_aliases" => {
            let mut edge = r["edge"].clone();
            fold_edge_aliases(&mut edge);
            Ok(edge)
        }
        "_coerce_id" => Ok(coerce_id(&r["value"])),
        "_hashable" => Ok(json!(
            crate::python_compat::scalar_key(&r["value"]).is_some()
        )),
        "_coerce_non_string_ids" => {
            let mut data = r["extraction"].clone();
            coerce_non_string_ids(&mut data);
            Ok(data)
        }
        "_norm_source_file" => norm_source_file(&r["p"], root),
        "_abs_identity" => abs_identity(&r["p"], root).map(|v| json!(v)),
        "_is_file_node_label" => Ok(json!(is_file_node_label(&r["label"], &r["source_file"]))),
        "_shortest_unique_suffix" => Ok(json!(shortest_unique_suffix(
            s("sf"),
            &strings("all_sfs").into_iter().collect()
        ))),
        "_file_label_reassignments" => {
            let items = array("items")
                .iter()
                .map(|v| (v[0].clone(), v[1].clone(), v[2].clone()))
                .collect::<Vec<_>>();
            Ok(json!(file_label_reassignments(&items)))
        }
        "_disambiguate_file_node_labels" => {
            let mut g = graph(r)?;
            disambiguate_file_node_labels(&mut g);
            Ok(g.to_node_link("edges"))
        }
        "disambiguate_file_labels_in_nodes" => {
            let mut nodes = array("nodes").to_vec();
            disambiguate_file_labels_in_nodes(&mut nodes);
            Ok(json!(nodes))
        }
        "_infer_merge_root" => {
            let output = r["output"].as_str().unwrap_or(&paths::OUT_DIR);
            let name = Path::new(output)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            Ok(json!(infer_merge_root(path, &name)))
        }
        "_build_prune_sets" => build_prune_sets(
            &strings("prune_sources"),
            r["eff_root"].as_str().map(Path::new),
            &strings("new_sources").into_iter().collect(),
        )
        .map(|v| json!(v)),
        "_derive_prune_root" => Ok(json!(derive_prune_root(
            &strings("prune_sources"),
            &strings("stored_sfs").into_iter().collect()
        ))),
        "edge_data" => Ok(json!(edge_data(&graph(r)?, &r["u"], &r["v"])?)),
        "edge_datas" => Ok(json!(edge_datas(&graph(r)?, &r["u"], &r["v"])?)),
        "dedupe_nodes" => dedupe_nodes(array("nodes")).map(|v| json!(v)),
        "dedupe_edges" => dedupe_edges(array("edges")).map(|v| json!(v)),
        "_old_file_stems" => Ok(json!(old_file_stems(Path::new(s("rel"))))),
        "_semantic_id_remap" => Ok(json!(semantic_id_remap(array("nodes"), root))),
        "_has_global_id" => Ok(json!(has_global_id(&r["node"]))),
        "graph_has_legacy_ids" => Ok(json!(graph_has_legacy_ids(
            array("nodes"),
            root,
            r["sample"].as_i64().unwrap_or(300)
        ))),
        "_doc_twin_remap" => Ok(json!(doc_twin_remap(array("nodes")))),
        "build_from_json" => {
            let mut data = r["extraction"].clone();
            let g = build_from_json_dedup(
                &mut data,
                r["directed"].as_bool().unwrap_or(false),
                false,
                r["dedup"].as_bool().unwrap_or(true),
                root,
                &mut messages,
            )?;
            Ok(json!({"graph":g.to_node_link("edges"),"extraction":data}))
        }
        "build" => {
            let options = BuildOptions {
                directed: r["directed"].as_bool().unwrap_or(false),
                dedup: r["dedup"].as_bool().unwrap_or(true),
                dedup_llm_backend: r["dedup_llm_backend"].as_str().map(str::to_owned),
                root: root.map(Path::to_path_buf),
                protected_ids: strings("protected_ids")
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                multigraph: false,
            };
            build(array("extractions"), &options, &mut messages, None)
                .map(|g| g.to_node_link("edges"))
        }
        "_norm_label" => Ok(json!(crate::dedup::norm(&r["label"]))),
        "deduplicate_by_label" => {
            let (nodes, edges, notes) = deduplicate_by_label(array("nodes"), array("edges"))?;
            messages.stderr.extend(notes);
            Ok(json!([nodes, edges]))
        }
        "_load_existing_graph" => load_existing_graph(path).map(|v| {
            v.map_or(Value::Null, |g| {
                json!([g.nodes, g.edges, g.hyperedges, g.directed])
            })
        }),
        "_tier_replacement_sources" => {
            let ast = r
                .get("ast_sources")
                .filter(|v| !v.is_null())
                .map(|_| strings("ast_sources"));
            Ok(json!(tier_replacement_sources(
                array("chunks"),
                root,
                ast.as_deref()
            )))
        }
        "merge_raw_extraction" => {
            let mut data = r["new"].clone();
            let ast = r
                .get("ast_sources")
                .filter(|v| !v.is_null())
                .map(|_| strings("ast_sources"));
            merge_raw_extraction(
                &mut data,
                path,
                &strings("prune_sources"),
                root,
                ast.as_deref(),
            )?;
            Ok(data)
        }
        "build_merge" => {
            let options = MergeOptions {
                graph_path: r["graph_path"].as_str().map(PathBuf::from),
                prune_sources: strings("prune_sources"),
                directed: r["directed"].as_bool(),
                dedup: r["dedup"].as_bool().unwrap_or(true),
                dedup_llm_backend: r["dedup_llm_backend"].as_str().map(str::to_owned),
                root: root.map(Path::to_path_buf),
                ast_sources: r
                    .get("ast_sources")
                    .filter(|v| !v.is_null())
                    .map(|_| strings("ast_sources")),
            };
            build_merge(array("new_chunks"), &options, &mut messages, None)
                .map(|g| g.to_node_link("edges"))
        }
        "prefix_graph_for_global" => prefix_graph_for_global(
            &graph(r)?,
            s("repo_tag"),
            r["community_offset"].as_i64().unwrap_or(0),
        )
        .map(|g| g.to_node_link("edges")),
        "distinct_repo_tags" => Ok(json!(distinct_repo_tags(&strings("graph_paths")))),
        "prune_repo_from_graph" => {
            let mut g = graph(r)?;
            let count = prune_repo_from_graph(&mut g, s("repo_tag"))?;
            Ok(json!({"graph":g.to_node_link("edges"),"count":count}))
        }
        _ => Err("Unknown graph construction function".into()),
    }?;
    Ok(json!({"value":result,"stdout":messages.stdout,"stderr":messages.stderr}))
}
