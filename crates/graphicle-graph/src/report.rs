// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/report.py
//! Human-readable graph reports. Port of the pinned Graphify report module.
use crate::{Result, analyze, graph::Graph, path_syntax, python_compat, reflect};
use serde_json::{Map, Value, json};
use std::path::Path;

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
fn s(v: &Value) -> String {
    python_compat::string(v)
}
fn field(v: &Value, key: &str, default: &str) -> String {
    v.get(key).map(s).unwrap_or_else(|| default.into())
}
fn number(v: &Value) -> Result<f64> {
    v.as_f64()
        .or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
        .ok_or_else(|| "TypeError: expected number".into())
}
fn array(v: &Value) -> Result<&Vec<Value>> {
    v.as_array()
        .ok_or_else(|| "TypeError: expected sequence".into())
}
fn comma(v: &Value) -> Result<String> {
    if !v.is_number() {
        return Err("TypeError: expected numeric formatting input".into());
    }
    let text = s(v);
    let (whole, rest) = text
        .find(['.', 'e'])
        .map_or((text.as_str(), ""), |i| (&text[..i], &text[i..]));
    let (sign, digits) = whole.strip_prefix('-').map_or(("", whole), |d| ("-", d));
    let mut out = sign.to_string();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.push_str(rest);
    Ok(out)
}
pub fn portable_root_label(root: &str) -> String {
    let raw = root.replace('\\', "/");
    let parts = path_syntax::posix_parts(&raw);
    let name = path_syntax::posix_name(&parts);
    if !matches!(name, "" | "." | "..") {
        return name.into();
    }
    let resolved = crate::fsutil::resolve_missing(Path::new(&raw))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if resolved.is_empty() { raw } else { resolved }
}
pub fn safe_community_name(label: &str) -> String {
    let clean = label
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
        .chars()
        .filter(|c| !"\\/*?:\"<>|#^[]".contains(*c))
        .collect::<String>();
    let mut clean = clean
        .trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .to_string();
    let lower = clean.to_lowercase();
    for suffix in [".md", ".mdx", ".markdown"] {
        if lower.ends_with(suffix) {
            clean.truncate(clean.rfind('.').unwrap());
            break;
        }
    }
    if clean.is_empty() {
        "unnamed".into()
    } else {
        clean
    }
}
pub fn load_learning_for_report(path: &Path) -> Option<Value> {
    (|| -> Result<Option<Value>> {
        let overlay = reflect::load_learning_overlay(path)?;
        let mem = path.parent().unwrap_or(Path::new(".")).join("memory");
        let dead_ends = if mem.is_dir() {
            reflect::aggregate_lessons(
                &reflect::load_memory_docs(&mem)?,
                None,
                &crate::python_datetime::DateTime::now(),
                30.0,
                2,
                None,
            )?["dead_ends"]
                .clone()
        } else {
            json!([])
        };
        Ok(if !truthy(&overlay) && !truthy(&dead_ends) {
            None
        } else {
            Some(json!({"overlay": overlay, "dead_ends": dead_ends}))
        })
    })()
    .unwrap_or(None)
}
pub fn learning_section(lines: &mut Vec<String>, learning: &Value, top_n: i64) -> Result<()> {
    if !truthy(learning) {
        return Ok(());
    }
    let empty = Map::new();
    let overlay = learning
        .get("overlay")
        .filter(|v| truthy(v))
        .map(|v| v.as_object().ok_or("TypeError: expected overlay mapping"))
        .transpose()?
        .unwrap_or(&empty);
    let mut preferred = overlay
        .iter()
        .filter(|(_, e)| e.is_object() && e["status"] == "preferred")
        .map(|(id, e)| {
            let uses = number(e.get("uses").unwrap_or(&json!(0)))?;
            let raw = e
                .get("score")
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or(json!(0));
            let score = raw
                .as_str()
                .map(|s| {
                    s.parse::<f64>()
                        .map_err(|_| crate::Error::invalid("ValueError: score"))
                })
                .unwrap_or_else(|| number(&raw))?;
            Ok((id, e, uses, score))
        })
        .collect::<Result<Vec<_>>>()?;
    preferred.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| b.3.total_cmp(&a.3))
            .then_with(|| a.0.cmp(b.0))
    });
    let none = json!([]);
    let dead = array(
        learning
            .get("dead_ends")
            .filter(|v| truthy(v))
            .unwrap_or(&none),
    )?;
    if preferred.is_empty() && dead.is_empty() {
        return Ok(());
    }
    lines.extend(["".into(), "## Work-memory lessons".into()]);
    if !preferred.is_empty() {
        lines.extend([
            "".into(),
            "**Preferred sources** — corroborated by past sessions; start here.".into(),
        ]);
        let n = if top_n < 0 {
            preferred
                .len()
                .saturating_sub(top_n.unsigned_abs() as usize)
        } else {
            top_n as usize
        };
        for (id, e, _, _) in preferred.into_iter().take(n) {
            let label = e
                .get("label")
                .filter(|v| truthy(v))
                .map(s)
                .unwrap_or_else(|| id.clone());
            let stale = if truthy(&e["stale"]) {
                " _(code changed — re-verify)_"
            } else {
                ""
            };
            lines.push(format!(
                "- `{label}` ({}× useful, score={}){stale}",
                field(e, "uses", "0"),
                field(e, "score", "0")
            ));
        }
    }
    if !dead.is_empty() {
        lines.extend([
            "".into(),
            "**Known dead ends** — questions that led nowhere; don't re-derive.".into(),
        ]);
        for d in dead {
            let nodes = array(d.get("nodes").unwrap_or(&none))?
                .iter()
                .map(|v| format!("`{}`", s(v)))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "- \"{}\"{}",
                field(d, "question", ""),
                if nodes.is_empty() {
                    String::new()
                } else {
                    format!(" -> {nodes}")
                }
            ));
        }
    }
    Ok(())
}
pub fn generate(graph: &Graph, r: &Value) -> Result<String> {
    let empty = json!({});
    let list = json!([]);
    let communities = r["communities"].as_object().ok_or("Expected communities")?;
    let mut labels = Map::new();
    if let Some(m) = r.get("community_labels").filter(|v| truthy(v)) {
        for (k, v) in m.as_object().ok_or("Expected community labels")? {
            labels.insert(
                k.parse::<i64>()
                    .map_err(|_| "ValueError: community ID")?
                    .to_string(),
                v.clone(),
            );
        }
    }
    let scores = r.get("cohesion_scores").unwrap_or(&empty);
    let edges = graph.ordered_edges();
    let total = edges.len().max(1) as f64;
    let pct = |kind| {
        (edges
            .iter()
            .filter(|(_, _, e)| {
                e.attributes
                    .get("confidence")
                    .cloned()
                    .unwrap_or(json!("EXTRACTED"))
                    == kind
            })
            .count() as f64
            / total
            * 100.0)
            .round_ties_even() as i64
    };
    let (ext, inf, amb) = (pct("EXTRACTED"), pct("INFERRED"), pct("AMBIGUOUS"));
    let inferred = edges
        .iter()
        .filter(|(_, _, e)| e.attributes.get("confidence") == Some(&json!("INFERRED")))
        .collect::<Vec<_>>();
    let avg = if inferred.is_empty() {
        None
    } else {
        let sum = inferred
            .iter()
            .map(|(_, _, e)| number(e.attributes.get("confidence_score").unwrap_or(&json!(0.5))))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .sum::<f64>();
        Some(
            format!("{:.2}", sum / inferred.len() as f64)
                .parse::<f64>()
                .map_err(|_| "Invalid confidence")?,
        )
    };
    let today = r["today"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Local::now().date_naive().to_string());
    let root = field(r, "root", "");
    let mut lines = vec![
        format!("# Graph Report - {}  ({today})", portable_root_label(&root)),
        "".into(),
        "## Corpus Check".into(),
    ];
    let detection = &r["detection_result"];
    if truthy(&detection["warning"]) {
        lines.push(format!("- {}", s(&detection["warning"])));
    } else {
        lines.push(format!(
            "- {} files · ~{} words",
            s(detection
                .get("total_files")
                .ok_or("KeyError: total_files")?),
            comma(
                detection
                    .get("total_words")
                    .ok_or("KeyError: total_words")?
            )?
        ));
        lines.push("- Verdict: corpus is large enough that graph structure adds value.".into());
        if let Some(files) = detection.get("unclassified").filter(|v| truthy(v)) {
            let files = array(files)?;
            let mut counts: Vec<(String, usize)> = vec![];
            for file in files {
                let path = file.as_str().ok_or("Expected path")?;
                let parts = path_syntax::posix_parts(path);
                let ext = path_syntax::suffix(path_syntax::posix_name(&parts));
                let ext = if ext.is_empty() { "(none)" } else { ext };
                if let Some((_, n)) = counts.iter_mut().find(|(k, _)| k == ext) {
                    *n += 1;
                } else {
                    counts.push((ext.into(), 1));
                }
            }
            counts.sort_by_key(|item| std::cmp::Reverse(item.1));
            lines.push(format!(
                "- Unclassified: {} file(s) not represented in the graph (top: {})",
                files.len(),
                counts
                    .iter()
                    .take(3)
                    .map(|(e, n)| format!("{e} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let min = r["min_community_size"].as_i64().unwrap_or(3);
    // The one "real" (structural) node every figure below agrees on (#3148):
    // file nodes are scaffolding, concept nodes are semantic-layer entities
    // and rationale nodes are docstring fragments (0.9.72, #3794).
    let real_node = |n: usize| -> Result<bool> {
        Ok(!analyze::is_file_node(graph, n)?
            && !analyze::is_concept_node(graph, n)?
            && graph.nodes[n].attributes.get("file_type") != Some(&json!("rationale")))
    };
    let mut real = Vec::new();
    for (cid, members) in communities {
        let mut nodes = Vec::new();
        for id in array(members)? {
            let n = graph
                .node_index(id)
                .ok_or("KeyError: community node missing")?;
            if real_node(n)? {
                nodes.push(n);
            }
        }
        real.push((cid, nodes));
    }
    let thin = real.iter().filter(|(_, n)| (n.len() as i64) < min).count();
    let shown = real.iter().filter(|(_, n)| n.len() as i64 >= min).count();
    let tokens = r.get("token_cost").unwrap_or(&empty);
    lines.extend([
        "".into(),
        "## Summary".into(),
        format!(
            "- {} nodes · {} edges · {} communities{}",
            graph.nodes.len(),
            edges.len(),
            communities.len(),
            if thin > 0 {
                format!(" ({shown} shown, {thin} thin omitted)")
            } else {
                String::new()
            }
        ),
        format!(
            "- Extraction: {ext}% EXTRACTED · {inf}% INFERRED · {amb}% AMBIGUOUS{}",
            avg.map(|a| format!(
                " · INFERRED: {} edges (avg confidence: {})",
                inferred.len(),
                s(&json!(a))
            ))
            .unwrap_or_default()
        ),
        format!(
            "- Token cost: {} input · {} output",
            comma(tokens.get("input").unwrap_or(&json!(0)))?,
            comma(tokens.get("output").unwrap_or(&json!(0)))?
        ),
    ]);
    if let Some(commit) = r.get("built_at_commit").filter(|v| truthy(v)) {
        lines.extend([
            "".into(),
            "## Graph Freshness".into(),
            format!(
                "- Built from commit: `{}`",
                s(commit).chars().take(8).collect::<String>()
            ),
            "- Run `git rev-parse HEAD` and compare to check if the graph is stale.".into(),
            format!(
                "- Run `{} update .` after code changes (no API cost).",
                crate::cli::PROGRAM
            ),
        ]);
    }
    let label = |cid: &str| {
        labels
            .get(cid)
            .map(s)
            .unwrap_or_else(|| format!("Community {cid}"))
    };
    if real.iter().any(|(_, n)| !n.is_empty()) {
        lines.extend(["".into(), "## Community Hubs (Navigation)".into()]);
        for (cid, _) in real.iter().filter(|(_, n)| !n.is_empty()) {
            let l = label(cid);
            lines.push(if r["obsidian"].as_bool().unwrap_or(false) {
                format!("- [[_COMMUNITY_{}|{l}]]", safe_community_name(&l))
            } else {
                format!("- {l}")
            });
        }
    }
    lines.extend([
        "".into(),
        "## God Nodes (most connected - your core abstractions)".into(),
    ]);
    for (i, n) in array(r.get("god_node_list").unwrap_or(&list))?
        .iter()
        .enumerate()
    {
        lines.push(format!(
            "{}. `{}` - {} edges",
            i + 1,
            s(n.get("label").ok_or("KeyError: label")?),
            s(n.get("degree").ok_or("KeyError: degree")?)
        ));
    }
    lines.extend([
        "".into(),
        "## Surprising Connections (you probably didn't know these)".into(),
    ]);
    let surprises = array(r.get("surprise_list").unwrap_or(&list))?;
    if surprises.is_empty() {
        lines.push("- None detected - all connections are within the same source files.".into());
    }
    for item in surprises {
        let relation = field(item, "relation", "related_to");
        let conf = field(item, "confidence", "EXTRACTED");
        let tag = if conf == "INFERRED" && !item["confidence_score"].is_null() {
            format!("INFERRED {:.2}", number(&item["confidence_score"])?)
        } else {
            conf
        };
        let files = item.get("source_files").cloned().unwrap_or(json!(["", ""]));
        let files = array(&files)?;
        if files.len() < 2 {
            return Err("IndexError: source_files".into());
        }
        lines.push(format!(
            "- `{}` --{relation}--> `{}`  [{tag}]{}",
            s(item.get("source").ok_or("KeyError: source")?),
            s(item.get("target").ok_or("KeyError: target")?),
            if relation == "semantically_similar_to" {
                " [semantically similar]"
            } else {
                ""
            }
        ));
        lines.push(format!(
            "  {} → {}{}",
            s(&files[0]),
            s(&files[1]),
            if truthy(&item["note"]) {
                format!("  _{}_", s(&item["note"]))
            } else {
                String::new()
            }
        ));
    }
    let has_code = graph
        .nodes
        .iter()
        .any(|n| n.attributes.get("file_type") == Some(&json!("code")))
        || edges.iter().any(|(_, _, e)| {
            matches!(
                e.attributes.get("relation").and_then(Value::as_str),
                Some("imports" | "imports_from")
            )
        });
    if has_code {
        lines.extend(["".into(), "## Import Cycles".into()]);
        let cycles = analyze::find_import_cycles(graph, 5, 20)?;
        if cycles.is_empty() {
            lines.push("- None detected.".into());
        }
        for c in cycles {
            let mut ns = array(&c["cycle"])?.iter().map(s).collect::<Vec<_>>();
            if ns.is_empty() {
                continue;
            }
            ns.push(ns[0].clone());
            lines.push(format!(
                "- {}-file cycle: `{}`",
                s(&c["length"]),
                ns.join(" -> ")
            ));
        }
    }
    if let Some(hyper) = graph.attributes.get("hyperedges").filter(|v| truthy(v)) {
        lines.extend(["".into(), "## Hyperedges (group relationships)".into()]);
        for h in array(hyper)? {
            let nodes = array(h.get("nodes").unwrap_or(&list))?
                .iter()
                .map(|v| v.as_str().ok_or("TypeError: hyperedge node string"))
                .collect::<std::result::Result<Vec<_>, _>>()?
                .join(", ");
            let conf = field(h, "confidence", "INFERRED");
            let tag = if h["confidence_score"].is_null() {
                conf
            } else {
                format!("{conf} {:.2}", number(&h["confidence_score"])?)
            };
            lines.push(format!(
                "- **{}** — {nodes} [{tag}]",
                h.get("label")
                    .or_else(|| h.get("id"))
                    .map(s)
                    .unwrap_or_default()
            ));
        }
    }
    lines.extend([
        "".into(),
        format!(
            "## Communities ({} total, {thin} thin omitted)",
            communities.len()
        ),
    ]);
    let node_label = |i: usize| {
        graph.nodes[i]
            .attributes
            .get("label")
            .map(s)
            .unwrap_or_else(|| s(&graph.nodes[i].id))
    };
    for (cid, ns) in &real {
        if ns.is_empty() || (ns.len() as i64) < min {
            continue;
        }
        lines.extend([
            "".into(),
            format!("### Community {cid} - \"{}\"", label(cid)),
            format!(
                "Cohesion: {:.2}",
                number(scores.get(*cid).unwrap_or(&json!(0.0)))?
            ),
            format!(
                "Nodes ({}): {}{}",
                ns.len(),
                ns.iter()
                    .take(8)
                    .map(|n| node_label(*n))
                    .collect::<Vec<_>>()
                    .join(", "),
                if ns.len() > 8 {
                    format!(" (+{} more)", ns.len() - 8)
                } else {
                    String::new()
                }
            ),
        ]);
    }
    let ambiguous = edges
        .iter()
        .filter(|(_, _, e)| e.attributes.get("confidence") == Some(&json!("AMBIGUOUS")))
        .collect::<Vec<_>>();
    if !ambiguous.is_empty() {
        lines.extend(["".into(), "## Ambiguous Edges - Review These".into()]);
        for (u, v, e) in ambiguous {
            lines.extend([
                format!("- `{}` → `{}`  [AMBIGUOUS]", node_label(*u), node_label(*v)),
                format!(
                    "  {} · relation: {}",
                    e.attributes.get("source_file").map(s).unwrap_or_default(),
                    e.attributes
                        .get("relation")
                        .map(s)
                        .unwrap_or("unknown".into())
                ),
            ]);
        }
    }
    let mut isolated = Vec::new();
    let mut raw_isolated = 0;
    for (i, n) in graph.nodes.iter().enumerate() {
        if analyze::degree(graph, i) <= 1 {
            raw_isolated += 1;
            if !analyze::is_file_node(graph, i)?
                && !analyze::is_concept_node(graph, i)?
                && n.attributes.get("file_type") != Some(&json!("rationale"))
                && !analyze::is_contains_only_ast_node(graph, i)
            {
                isolated.push(i);
            }
        }
    }
    if !isolated.is_empty() || thin > 0 || amb > 20 {
        lines.extend(["".into(), "## Knowledge Gaps".into()]);
        if !isolated.is_empty() {
            lines.push(format!(
                "- **{} isolated node(s):** {}{}",
                isolated.len(),
                isolated
                    .iter()
                    .take(5)
                    .map(|i| format!("`{}`", node_label(*i)))
                    .collect::<Vec<_>>()
                    .join(", "),
                if isolated.len() > 5 {
                    format!(" (+{} more)", isolated.len() - 5)
                } else {
                    String::new()
                }
            ));
            // "Undocumented components" only applies when an LLM semantic
            // layer (document/paper/image nodes) exists (#3801).
            let has_semantic_layer = graph.nodes.iter().any(|n| {
                matches!(
                    n.attributes.get("file_type").and_then(Value::as_str),
                    Some("document" | "paper" | "image")
                )
            });
            let reason = if has_semantic_layer {
                "possible missing edges or undocumented components"
            } else {
                "possible missing edges"
            };
            lines.push(format!("  These have ≤1 connection - {reason}. (Counts symbols only; {raw_isolated} node(s) total have ≤1 connection when file, concept and rationale nodes are included.)"));
        }
        if thin > 0 {
            lines.push(format!("- **{thin} thin communities (<{min} nodes) omitted from report** — run `{} query` to explore isolated nodes.", crate::cli::PROGRAM));
        }
        if amb > 20 {
            lines.push(format!("- **High ambiguity: {amb}% of edges are AMBIGUOUS.** Review the Ambiguous Edges section above."));
        }
    }
    learning_section(&mut lines, &r["learning"], 10)?;
    if let Some(questions) = r.get("suggested_questions").filter(|v| truthy(v)) {
        let questions = array(questions)?;
        lines.extend(["".into(), "## Suggested Questions".into()]);
        if questions.len() == 1 && questions[0]["type"] == "no_signal" {
            lines.push(format!(
                "_{}_",
                s(questions[0].get("why").ok_or("KeyError: why")?)
            ));
        } else {
            lines.extend([
                "_Questions this graph is uniquely positioned to answer:_".into(),
                "".into(),
            ]);
            // #4199: AMBIGUOUS-tagged questions get their own list, so
            // low-confidence hints do not crowd EXTRACTED/INFERRED bridges.
            let (low, high): (Vec<_>, Vec<_>) = questions
                .iter()
                .partition(|q| q.get("low_confidence").is_some_and(truthy));
            fn list(lines: &mut Vec<String>, rows: &[&Value]) -> Result<()> {
                for q in rows {
                    if q.get("question").is_some_and(truthy) {
                        lines.extend([
                            format!("- **{}**", s(&q["question"])),
                            format!("  _{}_", s(q.get("why").ok_or("KeyError: why")?)),
                        ]);
                    }
                }
                Ok(())
            }
            list(&mut lines, &high)?;
            if !low.is_empty() {
                lines.extend([
                    "".into(),
                    "### Low-confidence Hints".into(),
                    "_AMBIGUOUS edges — the extractor was unsure. Verify before acting on these._"
                        .into(),
                    "".into(),
                ]);
                list(&mut lines, &low)?;
            }
        }
    }
    Ok(lines.join("\n"))
}
pub fn dispatch(r: &Value) -> Result<Value> {
    match r["operation"].as_str().unwrap_or("") {
        "report.root" => Ok(json!(portable_root_label(
            r["root"].as_str().ok_or("Expected root")?
        ))),
        "report.safe_name" => Ok(json!(safe_community_name(
            r["label"].as_str().ok_or("Expected label")?
        ))),
        "report.load_learning" => Ok(json!(load_learning_for_report(Path::new(
            r["graph_path"].as_str().ok_or("Expected graph_path")?
        )))),
        "report.learning" => {
            let mut lines: Vec<String> =
                serde_json::from_value(r["lines"].clone()).map_err(|_| "Expected lines")?;
            learning_section(
                &mut lines,
                &r["learning"],
                r["top_n"].as_i64().unwrap_or(10),
            )?;
            Ok(json!(lines))
        }
        "report.generate" => {
            let mut g = Graph::from_node_link(&r["graph"], "links")?;
            if let Some(order) = r.get("neighbor_order") {
                g.restore_neighbor_order(order)?;
            }
            Ok(json!(generate(&g, r)?))
        }
        _ => Err("Unknown report operation".into()),
    }
}
