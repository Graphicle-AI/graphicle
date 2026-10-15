// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
use crate::{
    Result, build,
    cli::{Args, Output, expand},
    cli_state::{StageTimer, read_graph},
    cluster, export, paths,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs};
pub fn run(command: &str, a: &Args, out: &mut Output) -> Result<()> {
    let root = expand(a.positional(0).unwrap_or("."));
    let graph = a.path(
        "graph",
        root.join(paths::OUT_DIR.as_str()).join("graph.json"),
    );
    let mut raw = read_graph(&graph)?;
    let directed = raw["directed"].as_bool().unwrap_or(false);
    let mut d = build::Diagnostics::default();
    let mut timer = StageTimer::new(a.flag("timing"));
    let truthy = |v: Option<&Value>| v.is_some_and(build::truthy);
    // Upstream writes back `_raw.get("links") or _raw.get("edges")`, whose
    // dicts the build rewrote in place (alias folds, id remaps). The build
    // here moves a links-only file's links to `edges` and remaps those.
    let links_moved = truthy(raw.get("links")) && raw.get("edges").is_none();
    let g = build::build_from_json(&mut raw, directed, None, &mut d)?;
    let original_links = if links_moved {
        raw.get("edges")
    } else if truthy(raw.get("links")) {
        raw.get("links")
    } else {
        raw.get("edges").filter(|v| build::truthy(v))
    }
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();
    out.line(format!(
        "Graph: {} nodes, {} edges",
        g.nodes.len(),
        g.edge_count()
    ));
    timer.mark("load", &mut d.stderr);
    let mut comm = cluster::cluster(
        &g,
        a.positive_float("resolution", 1.0)?,
        a.get("exclude-hubs")
            .map(|_| a.float("exclude-hubs", 0.0))
            .transpose()?,
        "auto",
    )?;
    let prev = crate::watch::node_communities(&raw);
    if !prev.is_empty() {
        comm = cluster::remap_communities_to_previous(
            &comm,
            &prev.iter().map(|(k, v)| (json!(k), *v)).collect(),
        )?;
    }
    timer.mark("cluster", &mut d.stderr);
    let cohesion = cluster::score_all(&g, &comm);
    let gods = crate::analyze::god_nodes(
        &g,
        10,
        a.get("exclude-hubs")
            .map(|_| a.float("exclude-hubs", 0.0))
            .transpose()?,
    )?;
    let surprises =
        crate::analyze::surprising_connections(&g, &comm, crate::analyze::SURPRISES_TOP_N)?;
    let dir = if a.get("graph").is_some()
        && graph.parent().is_some_and(|p| {
            p.file_name()
                .is_some_and(|n| n == paths::OUT_DIR_NAME.as_str())
        }) {
        graph.parent().unwrap().into()
    } else {
        root.join(paths::OUT_DIR.as_str())
    };
    fs::create_dir_all(&dir).map_err(crate::Error::from)?;
    let lp = dir.join(".graphicle_labels.json");
    let sigp = dir.join(".graphicle_labels.json.sig");
    let saved = crate::install_fs::json_like(&lp);
    let sigs = crate::install_fs::json_like(&sigp);
    let current = cluster::community_member_sigs(&comm);
    let hubs = cluster::label_communities_by_hub(&g, &comm);
    let mut labels = json!({});
    let mut usage = crate::llm::Usage::default();
    let placeholders = a.flag("no-label") && command != "label" && !lp.exists();
    if lp.exists() && command != "label" {
        let count_mismatch = saved.as_object().is_none_or(|m| m.len() != comm.len());
        for (cid, members) in &comm {
            let k = cid.to_string();
            let named = saved[&k]
                .as_str()
                .is_some_and(|s| s != format!("Community {cid}"));
            // Equal signature, or the shared members are a strict majority of
            // both the saved and the current set (#2298). Without signatures a
            // changed community count keeps a label only on that majority.
            let fresh = if sigs.as_object().is_some_and(|m| !m.is_empty()) {
                cluster::label_still_applies(
                    *cid,
                    members,
                    &prev,
                    sigs[&k].as_str(),
                    current[&k].as_str(),
                )
            } else if count_mismatch {
                cluster::label_still_applies(*cid, members, &prev, None, None)
            } else {
                true
            };
            labels[&k] = if named && fresh {
                saved[&k].clone()
            } else {
                hubs[&k].clone()
            };
        }
    } else if placeholders {
        for (cid, _) in &comm {
            labels[cid.to_string()] = json!(format!("Community {cid}"));
        }
    } else {
        labels = hubs.clone();
        let mut requested = comm.clone();
        if a.flag("missing-only") {
            requested.retain(|(cid, _)| {
                saved[cid.to_string()]
                    .as_str()
                    .is_none_or(|s| s == format!("Community {cid}"))
            });
            for (cid, _) in &comm {
                if let Some(v) = saved.get(cid.to_string()) {
                    labels[cid.to_string()] = v.clone();
                }
            }
        }
        let runtime = crate::llm::Runtime::from_environment(&mut d);
        let (generated, _) = runtime.generate_labels(
            &g,
            &requested,
            &crate::llm_labels::LabelOptions {
                backend: a.get("backend").map(str::to_string),
                model: a.get("model").map(str::to_string),
                gods: gods.clone(),
                max_concurrency: a.positive("max-concurrency", 4)?,
                batch_size: a.positive("batch-size", 100)?,
                ..Default::default()
            },
            &mut usage,
            &mut d,
        );
        for (cid, v) in generated {
            if v.as_str().is_some_and(|s| !s.is_empty())
                && v != format!("Community {cid}")
                && v != cid
            {
                labels[cid.to_string()] = json!(v);
            }
        }
    }
    timer.mark("label", &mut d.stderr);
    let questions = crate::analyze::suggest_questions(
        &g,
        &comm,
        labels.as_object().unwrap(),
        crate::analyze::QUESTIONS_TOP_N,
    )?;
    let commit = raw["built_at_commit"]
        .as_str()
        .map(str::to_string)
        .or_else(|| export::git_head(Some(&root)));
    let map: BTreeMap<i64, Value> = labels
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
        .collect();
    let target = dir.join("graph.json");
    export::backup_if_protected(&dir, &mut d);
    let stale = dir.join(crate::exporters::html::HTML_STALE_MARKER);
    let existed = stale.exists();
    crate::install_fs::write(&stale, "")?;
    if !export::to_json(
        &g,
        &comm,
        &target,
        &export::JsonOptions {
            community_labels: map.clone(),
            built_at_commit: commit.clone(),
            original_links: Some(original_links),
            ..Default::default()
        },
        &mut d,
    )? {
        if !existed {
            let _ = fs::remove_file(stale);
        }
        out.diagnostics(d);
        return Err(
            "graph.json NOT written: refusing to overwrite; report and labels left untouched."
                .into(),
        );
    }
    let report = crate::report::generate(
        &g,
        &json!({"communities":cluster::communities_to_json(&comm),"cohesion_scores":cohesion,"community_labels":labels,"god_node_list":gods,"surprise_list":surprises,"detection_result":{"warning":"cluster-only mode — file stats not available"},"token_cost":{"input":usage.input,"output":usage.output},"root":root,"suggested_questions":questions,"min_community_size":a.number("min-community-size",3)?,"built_at_commit":commit,"learning":crate::report::load_learning_for_report(&target)}),
    )?;
    crate::install_fs::write(&dir.join("GRAPH_REPORT.md"), &report)?;
    paths::write_json_atomic(&dir.join(".graphicle_analysis.json"),&json!({"communities":cluster::communities_to_json(&comm),"cohesion":cohesion,"gods":gods,"surprises":surprises,"questions":questions}),Some(2),false).map_err(|e|e.to_string())?;
    if !placeholders {
        paths::write_json_atomic(&lp, &labels, None, false).map_err(crate::Error::from)?;
        paths::write_json_atomic(&sigp, &current, None, true).map_err(crate::Error::from)?;
    }
    timer.mark("report", &mut d.stderr);
    let html = dir.join("graph.html");
    if a.flag("no-viz") || crate::exporters::html::viz_node_limit() <= 0 {
        let _ = fs::remove_file(&html);
        let _ = fs::remove_file(&stale);
    } else {
        match crate::exporters::html::to_html(
            &g,
            &comm,
            &html,
            &crate::exporters::html::HtmlOptions {
                community_labels: map,
                node_limit: Some(crate::exporters::html::viz_node_limit()),
                ..Default::default()
            },
            &mut d,
        ) {
            Ok(true) => {
                let _ = fs::remove_file(&stale);
            }
            Ok(false) => {
                out.warn("Skipped graph.html: no useful community aggregation could be generated")
            }
            Err(e) => out.warn(format!("Skipped graph.html: {e}")),
        }
    }
    timer.mark("export", &mut d.stderr);
    timer.total(&mut d.stderr);
    out.diagnostics(d);
    out.line(format!(
        "Done - {} communities. GRAPH_REPORT.md and graph.json updated.",
        comm.len()
    ));
    Ok(())
}
