// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Structured native entry points for embedded callers and parity qualification.
use crate::{Result, graph::Graph, serve, serve_find, serve_search::*, serve_text::*};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::Path};
pub fn dispatch(r: &Value) -> Result<Value> {
    let action = r["action"].as_str().unwrap_or("query_graph");
    let value = r.get("value").unwrap_or(&Value::Null);
    let st = |k: &str| r[k].as_str().unwrap_or("");
    let list = |k: &str| {
        r[k].as_array()
            .into_iter()
            .flatten()
            .map(string)
            .collect::<Vec<_>>()
    };
    match action {
        "load" => return Ok(serve::load_graph(Path::new(st("path")))?.to_node_link("links")),
        "tools" => return Ok(serve::tools_list()),
        "resources" => return Ok(serve::resources_list()),
        "max_contexts" => return Ok(json!(serve::max_contexts())),
        "strip_diacritics" => return Ok(json!(fold(&text(value)))),
        "search_tokens" => return Ok(json!(tokens(&string(value)))),
        "trigrams" => return Ok(json!(trigrams(&string(value)))),
        "rationale_text" => {
            return Ok(json!(rationale_text(
                value.as_object().ok_or("Expected node")?
            )));
        }
        "attributes_text" => {
            return Ok(json!(attributes_text(
                value.as_object().ok_or("Expected node")?
            )));
        }
        "flatten" => {
            let mut parts = list("parts");
            flatten(value, &mut parts);
            return Ok(json!(parts));
        }
        "is_ambiguity_refusal" => {
            return Ok(json!(serve_find::is_ambiguity_refusal(value.as_str())));
        }
        "normalize_contexts" => return Ok(json!(normalize_contexts(&list("filters")))),
        "infer_contexts" => return Ok(json!(infer_contexts(st("question")))),
        "resolve_contexts" => return Ok(json!(resolve_contexts(st("question"), &list("filters")))),
        "display_path" => return Ok(json!(display_graph_path(st("path")))),
        "community_header" => {
            return Ok(json!(serve::community_header(
                r["cid"].as_i64().unwrap_or(0),
                r.get("name")
            )));
        }
        "cut_lines" => {
            return Ok(json!(cut_lines(
                &list("lines"),
                r["token_budget"].as_i64().unwrap_or(2000),
                st("hint")
            )));
        }
        "rpc" => {
            return Ok(serve::Server::new(Path::new(st("path")))
                .rpc(&r["request"])
                .unwrap_or(Value::Null));
        }
        "resource" => {
            return Ok(json!(
                serve::Server::new(Path::new(st("path"))).resource(st("uri"))?
            ));
        }
        _ => {}
    }
    let mut data = r["graph"].clone();
    if data.is_null() {
        data = json!({"nodes":[],"links":[]});
    }
    if data.get("links").is_none() {
        data["links"] = data.get_mut("edges").map(Value::take).unwrap_or(json!([]));
    }
    let mut g = Graph::from_node_link_owned(data, "links")?;
    if let Some(order) = r.get("neighbor_order") {
        g.restore_neighbor_order(order)?;
    }
    if let Some(order) = r.get("predecessor_order") {
        g.restore_predecessor_order(order)?;
    }
    let ctx = serve::GraphContext {
        communities: serve::communities(&g).unwrap_or_default(),
        index: SearchIndex::new(&g),
        graph: g,
    };
    let g = &ctx.graph;
    let indices = |key: &str| {
        r[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| g.node_index(id))
            .collect::<Vec<_>>()
    };
    let edges = |key: &str| {
        r[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((g.node_index(&e[0])?, g.node_index(&e[1])?)))
            .collect::<Vec<_>>()
    };
    let index = &ctx.index;
    Ok(match action {
        "communities" => ctx
            .communities
            .iter()
            .map(|(cid, ns)| {
                (
                    cid.to_string(),
                    json!(ns.iter().map(|i| &g.nodes[*i].id).collect::<Vec<_>>()),
                )
            })
            .collect::<Map<_, _>>()
            .into(),
        "search_text" => json!(search_text(
            g,
            g.node_index(&r["nid"]).ok_or("Unknown node")?
        )),
        "idf" => json!(index.idf(g, &list("terms"))),
        "candidates" => json!(
            index
                .candidates(&list("needles"), r["guard_frac"].as_f64().unwrap_or(0.1))
                .map(|a| a
                    .into_iter()
                    .map(|i| g.nodes[i].id.clone())
                    .collect::<Vec<_>>())
        ),
        "index" => {
            json!({"ids":g.nodes.iter().map(|n|n.id.clone()).collect::<Vec<_>>(),"postings":index.postings()})
        }
        "score_query" => score_json(
            g,
            &score_query(
                g,
                index,
                &list("terms"),
                r["collect_per_term_seeds"] == true,
            ),
        ),
        "score_nodes" => {
            score_json(g, &score_query(g, index, &list("terms"), false))["ranked"].clone()
        }
        "pick_endpoint" => {
            let ranked = r["scored"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_f64()?, g.node_index(&p[1])?)))
                .collect::<Vec<_>>();
            json!(pick_endpoint(g, &ranked, st("query")).map(|i| g.nodes[i].id.clone()))
        }
        "pick_seeds" => {
            let ranked = r["scored"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_f64()?, g.node_index(&p[1])?)))
                .collect::<Vec<_>>();
            let best = r["best_seed_by_term"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(t, id)| Some((t.clone(), g.node_index(id)?)))
                .collect::<BTreeMap<_, _>>();
            json!(
                pick_seeds_in_order(
                    (r["without_graph"] != true).then_some(g),
                    &ranked,
                    r["max_k"].as_u64().unwrap_or(3) as usize,
                    r["gap_ratio"].as_f64().unwrap_or(0.2),
                    &best,
                    &list("term_order")
                )
                .iter()
                .map(|i| &g.nodes[*i].id)
                .collect::<Vec<_>>()
            )
        }
        "filter_contexts" => filter_contexts(g, &list("filters"))?.to_node_link("links"),
        "traversal_view" => traversal_view(g)?.to_node_link("links"),
        "bfs" | "dfs" => {
            let (ns, es) = traverse(
                g,
                &indices("start_nodes"),
                r["depth"].as_i64().unwrap_or(3),
                action == "dfs",
            );
            json!([
                ns.iter().map(|i| &g.nodes[*i].id).collect::<Vec<_>>(),
                es.iter()
                    .map(|(a, b)| json!([g.nodes[*a].id, g.nodes[*b].id]))
                    .collect::<Vec<_>>()
            ])
        }
        "complete_induced" => {
            let mut es = edges("edges_seen");
            complete_induced(g, &indices("visited").into_iter().collect(), &mut es);
            json!(
                es.iter()
                    .map(|(a, b)| json!([g.nodes[*a].id, g.nodes[*b].id]))
                    .collect::<Vec<_>>()
            )
        }
        "neighbor_lines" => {
            let nid = r["nid"].clone();
            let i = g.node_index(&nid).ok_or("KeyError: node")?;
            json!(crate::serve::neighbor_lines(
                g,
                i,
                r["rel_filter"].as_str().unwrap_or("")
            ))
        }
        "subgraph_text" => json!(subgraph_text(
            g,
            &indices("nodes").into_iter().collect(),
            &edges("edges"),
            r["token_budget"].as_i64().unwrap_or(2000),
            &indices("seeds")
        )),
        "query_text" => json!(query_text(
            g,
            index,
            st("question"),
            QueryOptions {
                mode: r["mode"].as_str().unwrap_or("bfs"),
                depth: r["depth"].as_i64().unwrap_or(3),
                budget: r["token_budget"].as_i64().unwrap_or(2000),
                filters: &list("context_filters"),
                path: r["graph_path"].as_str(),
                seeds: r["seeds"].is_array().then_some(&list("seeds")[..]),
                seed_root: r["seed_root"].as_str().map(Path::new),
            }
        )?),
        "scoped" => json!(
            serve_find::scoped(g, index, st("path_part"), st("symbol_part"))
                .iter()
                .map(|i| &g.nodes[*i].id)
                .collect::<Vec<_>>()
        ),
        "literal_exact" => json!(serve_find::literal_exact(
            g,
            index,
            st("term"),
            st("norm_query")
        )),
        "find_tiers" => json!(
            serve_find::tiers(g, index, st("label"))
                .iter()
                .map(|a| a.iter().map(|i| &g.nodes[*i].id).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        ),
        "find_node" => json!(
            serve_find::tiers(g, index, st("label"))
                .iter()
                .flatten()
                .map(|i| &g.nodes[*i].id)
                .collect::<Vec<_>>()
        ),
        "ambiguity" => json!(
            serve_find::ambiguity(g, index, st("label"))
                .iter()
                .map(|i| &g.nodes[*i].id)
                .collect::<Vec<_>>()
        ),
        "resolve" => match serve_find::resolve(g, index, st("label")) {
            Ok(i) => json!([g.nodes[i].id, null]),
            Err(e) => json!([null, e]),
        },
        "tier_refusal" => json!(serve_find::tier_refusal(
            g,
            index,
            st("label"),
            &indices("tier"),
            r["file_query"] == true
        )),
        "python_qualified" => json!(
            serve_find::python_qualified(g, index, st("label"))
                .iter()
                .map(|i| &g.nodes[*i].id)
                .collect::<Vec<_>>()
        ),
        "no_scoped_match" => json!(serve_find::no_scoped_match_message(
            g,
            st("path_part"),
            st("symbol_part")
        )),
        "shortest_path_text" => json!(serve_find::shortest_path(g, index, &r["arguments"])?),
        "tool" => json!(serve::call_graph_tool(
            &ctx,
            st("name"),
            &r["arguments"],
            r["path"].as_str().map(Path::new)
        )?),
        name => json!(serve::call_graph_tool(
            &ctx,
            name,
            r,
            r["path"].as_str().map(Path::new)
        )?),
    })
}
