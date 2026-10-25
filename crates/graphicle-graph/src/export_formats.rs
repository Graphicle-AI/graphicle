// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/export.py
//! GraphML and SVG formats from graphify/export.py. SVG uses native XML,
//! NetworkX's force/energy objectives and Rust numeric routines.
use crate::{
    Result, analyze,
    cluster::Communities,
    export::{COMMUNITY_COLORS, sorted_value, strip_xml_illegal},
    exporters::common::*,
    graph::Graph,
    minhash::Mt19937,
    paths,
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::Path};

fn scalar(value: &Value) -> Result<Value> {
    Ok(match value {
        Value::Null => json!(""),
        Value::Bool(_) | Value::Number(_) => value.clone(),
        Value::String(s) => json!(strip_xml_illegal(s)),
        _ => json!(strip_xml_illegal(&json_text(
            &sorted_value(value),
            None,
            true
        )?)),
    })
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
}
fn xml_attr(value: &str) -> String {
    xml(value).replace('\n', "&#10;").replace('\t', "&#9;")
}
fn xml_type(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.to_string().contains(['.', 'e', 'E']) {
                "double"
            } else {
                "long"
            }
        }
        _ => "string",
    }
}
pub fn to_graphml(graph: &Graph, communities: &Communities, output: &Path) -> Result<()> {
    let mut graph = graph.clone();
    let assign = assignments(&graph, communities);
    for (i, n) in graph.nodes.iter_mut().enumerate() {
        n.attributes
            .insert("community".into(), json!(assign[i].unwrap_or(-1)));
        n.attributes.retain(|k, _| !k.starts_with('_'));
    }
    let mappings = graph
        .nodes
        .iter()
        .filter_map(|n| {
            n.id.as_str()
                .map(|s| (n.id.clone(), json!(strip_xml_illegal(s))))
        })
        .filter(|(a, b)| a != b)
        .collect::<Vec<_>>();
    if !mappings.is_empty() {
        graph = graph.relabel_copy(&mappings)?
    }
    let graph_attrs = graph
        .attributes
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| Ok((k, scalar(&v)?)))
        .collect::<Result<Map<_, _>>>()?;
    let nodes = graph
        .nodes
        .iter()
        .map(|n| {
            n.attributes
                .iter()
                .map(|(k, v)| Ok((k.to_string(), scalar(v)?)))
                .collect::<Result<Map<_, _>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    let edges = graph
        .ordered_edges()
        .iter()
        .map(|(_, _, e)| {
            e.attributes
                .iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| Ok((k.to_string(), scalar(v)?)))
                .collect::<Result<Map<_, _>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    let mut keys: Vec<(String, &str, &str)> = vec![];
    for (scope, attrs) in std::iter::once(("graph", &graph_attrs))
        .chain(nodes.iter().map(|a| ("node", a)))
        .chain(edges.iter().map(|a| ("edge", a)))
    {
        for (k, v) in attrs {
            if scope == "graph" && ["node_default", "edge_default", "id"].contains(&k.as_str()) {
                continue;
            }
            let key = (k.clone(), xml_type(v), scope);
            if !keys.contains(&key) {
                keys.push(key)
            }
        }
    }
    let mut text="<?xml version='1.0' encoding='utf-8'?>\n<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://graphml.graphdrawing.org/xmlns http://graphml.graphdrawing.org/xmlns/1.0/graphml.xsd\">\n".to_owned();
    for (i, (name, t, scope)) in keys.iter().enumerate().rev() {
        text.push_str(&format!(
            "  <key id=\"d{i}\" for=\"{scope}\" attr.name=\"{}\" attr.type=\"{t}\"/>\n",
            xml_attr(name)
        ));
    }
    let id = graph_attrs
        .get("id")
        .map(|v| format!(" id=\"{}\"", xml_attr(&string(v))))
        .unwrap_or_default();
    text.push_str(&format!(
        "  <graph edgedefault=\"{}\"{id}>\n",
        if graph.directed {
            "directed"
        } else {
            "undirected"
        }
    ));
    let data = |scope: &str, attrs: &Map<String, Value>, indent: &str| -> String {
        let mut out = String::new();
        for (k, v) in attrs {
            if scope == "graph" && ["node_default", "edge_default", "id"].contains(&k.as_str()) {
                continue;
            }
            let key = keys
                .iter()
                .position(|(name, t, s)| name == k && *t == xml_type(v) && *s == scope)
                .unwrap();
            out.push_str(&format!(
                "{indent}<data key=\"d{key}\">{}</data>\n",
                xml(&string(v))
            ))
        }
        out
    };
    text.push_str(&data("graph", &graph_attrs, "    "));
    for (i, n) in graph.nodes.iter().enumerate() {
        text.push_str(&format!("    <node id=\"{}\">\n", xml_attr(&string(&n.id))));
        text.push_str(&data("node", &nodes[i], "      "));
        text.push_str("    </node>\n")
    }
    for (i, (u, v, e)) in graph.ordered_edges().iter().enumerate() {
        let key = if graph.multigraph {
            format!(" id=\"{}\"", xml_attr(&string(&e.key)))
        } else {
            String::new()
        };
        text.push_str(&format!(
            "    <edge source=\"{}\" target=\"{}\"{key}>\n",
            xml_attr(&string(&graph.nodes[*u].id)),
            xml_attr(&string(&graph.nodes[*v].id))
        ));
        text.push_str(&data("edge", &edges[i], "      "));
        text.push_str("    </edge>\n")
    }
    text.push_str("  </graph>\n</graphml>\n");
    // Renamed over the output itself, as upstream writes a temporary file
    // and `os_replace_with_fallback`s it into place (#1831): a symlink at the
    // output is replaced, never written through.
    let name = output
        .file_name()
        .map_or_else(|| "graph.graphml".into(), |n| n.to_string_lossy());
    paths::replace_file(
        output,
        &format!(".{name}."),
        text.as_bytes(),
        paths::NewMode::Default,
    )
    .map_err(crate::Error::from)
}
fn random_position(rng: &mut Mt19937) -> f64 {
    let a = rng.next_u32() >> 5;
    let b = rng.next_u32() >> 6;
    ((a as u64) * 67_108_864 + b as u64) as f64 / 9_007_199_254_740_992.0
}
fn weights(graph: &Graph) -> Result<Vec<BTreeMap<usize, f64>>> {
    let mut out = vec![BTreeMap::new(); graph.nodes.len()];
    for (u, v, e) in graph.iter_edges() {
        let weight = e
            .attributes
            .get("weight")
            .map(|v| v.as_f64().ok_or("SVG edge weight must be numeric"))
            .transpose()?
            .unwrap_or(1.0);
        if !weight.is_finite() {
            return Err("SVG edge weight must be finite".into());
        }
        *out[u].entry(v).or_default() += weight;
        if !graph.directed && u != v {
            *out[v].entry(u).or_default() += weight
        }
    }
    Ok(out)
}
pub fn spring_layout(graph: &Graph) -> Result<Vec<[f64; 2]>> {
    let n = graph.nodes.len();
    if n == 0 {
        return Ok(vec![]);
    }
    if n == 1 {
        return Ok(vec![[0.0, 0.0]]);
    }
    let mut rng = Mt19937::new(42);
    let mut pos = (0..n)
        .map(|_| [random_position(&mut rng), random_position(&mut rng)])
        .collect::<Vec<_>>();
    let k = 2.0 / ((n as f64).sqrt() + 1.0);
    let w = weights(graph)?;
    if n < 500 {
        let max = |d: usize| pos.iter().map(|p| p[d]).fold(f64::NEG_INFINITY, f64::max);
        let min = |d: usize| pos.iter().map(|p| p[d]).fold(f64::INFINITY, f64::min);
        let mut t = (max(0) - min(0)).max(max(1) - min(1)) * 0.1;
        let dt = t / 51.0;
        for _ in 0..50 {
            let mut moves = vec![[0.0, 0.0]; n];
            for i in 0..n {
                let mut displacement = [0.0, 0.0];
                for j in 0..n {
                    let dx = pos[i][0] - pos[j][0];
                    let dy = pos[i][1] - pos[j][1];
                    let distance = (dx * dx + dy * dy).sqrt().max(0.01);
                    let f = k * k / (distance * distance)
                        - w[i].get(&j).copied().unwrap_or(0.0) * distance / k;
                    displacement[0] += dx * f;
                    displacement[1] += dy * f
                }
                let length = (displacement[0] * displacement[0]
                    + displacement[1] * displacement[1])
                    .sqrt()
                    .max(0.01);
                moves[i] = [displacement[0] * t / length, displacement[1] * t / length]
            }
            let mut norm = 0.0;
            for (i, d) in moves.iter().enumerate() {
                pos[i][0] += d[0];
                pos[i][1] += d[1];
                norm += d[0] * d[0] + d[1] * d[1]
            }
            t -= dt;
            if norm.sqrt() / (n as f64) < 1e-4 {
                break;
            }
        }
    } else {
        let symmetric = energy_weights(&w);
        let components = components(&symmetric);
        let mut x = pos
            .iter()
            .flat_map(|p| p.iter().map(|v| *v as f32 as f64))
            .collect::<Vec<_>>();
        let mut solver = lbfgsb_rs_pure::LBFGSB::new(10)
            .with_max_iter(50)
            .with_pgtol(1e-4);
        let lower = vec![f64::NEG_INFINITY; x.len()];
        let upper = vec![f64::INFINITY; x.len()];
        let result = solver
            .minimize(&mut x, &lower, &upper, &mut |x| {
                energy_cost(x, &symmetric, &components, k)
            })
            .map_err(str::to_owned)?;
        if !result.f.is_finite() || result.x.iter().any(|x| !x.is_finite()) {
            return Err("SVG energy layout failed to produce finite positions".into());
        }
        pos = result.x.chunks_exact(2).map(|p| [p[0], p[1]]).collect();
    }
    for d in 0..2 {
        let mean = pos.iter().map(|p| p[d]).sum::<f64>() / n as f64;
        for p in &mut pos {
            p[d] -= mean
        }
    }
    let scale = pos
        .iter()
        .flat_map(|p| p.iter())
        .map(|v| v.abs())
        .fold(0.0, f64::max);
    if scale > 0.0 {
        for p in &mut pos {
            p[0] /= scale;
            p[1] /= scale
        }
    }
    Ok(pos)
}
fn energy_weights(w: &[BTreeMap<usize, f64>]) -> Vec<BTreeMap<usize, f64>> {
    let mut sym = vec![BTreeMap::new(); w.len()];
    for (i, row) in w.iter().enumerate() {
        for (j, v) in row {
            let v = (*v as f32).abs() as f64 / 2.0;
            *sym[i].entry(*j).or_default() += v;
            *sym[*j].entry(i).or_default() += v
        }
    }
    sym
}
fn components(w: &[BTreeMap<usize, f64>]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; w.len()];
    let mut out = vec![];
    for i in 0..w.len() {
        if seen[i] {
            continue;
        }
        let mut stack = vec![i];
        let mut members = vec![];
        seen[i] = true;
        while let Some(j) = stack.pop() {
            members.push(j);
            for (k, v) in &w[j] {
                if *v != 0.0 && !seen[*k] {
                    seen[*k] = true;
                    stack.push(*k)
                }
            }
        }
        out.push(members)
    }
    out
}
fn energy_cost(
    x: &[f64],
    w: &[BTreeMap<usize, f64>],
    components: &[Vec<usize>],
    k: f64,
) -> (f64, Vec<f64>) {
    let n = w.len();
    let mut cost = 0.0;
    let mut grad = vec![0.0; 2 * n];
    for i in 0..n {
        for j in 0..n {
            let dx = x[2 * i] - x[2 * j];
            let dy = x[2 * i + 1] - x[2 * j + 1];
            let d2 = (dx * dx + dy * dy).max(1e-10);
            let d = d2.sqrt();
            cost -= k * k * d.ln();
            let factor = -2.0 * k * k / d2;
            grad[2 * i] += factor * dx;
            grad[2 * i + 1] += factor * dy;
        }
        for (j, weight) in &w[i] {
            let dx = x[2 * i] - x[2 * j];
            let dy = x[2 * i + 1] - x[2 * j + 1];
            let d2 = (dx * dx + dy * dy).max(1e-10);
            let ad = weight * d2.sqrt();
            cost += ad * d2 / (3.0 * k);
            grad[2 * i] += 2.0 * ad / k * dx;
            grad[2 * i + 1] += 2.0 * ad / k * dy;
        }
    }
    for comp in components {
        let mut center = [0.0, 0.0];
        for i in comp {
            center[0] += x[2 * i];
            center[1] += x[2 * i + 1]
        }
        let delta = [
            center[0] / comp.len() as f64 - 0.5,
            center[1] / comp.len() as f64 - 0.5,
        ];
        for i in comp {
            grad[2 * i] += delta[0];
            grad[2 * i + 1] += delta[1]
        }
        cost += 0.5 * comp.len() as f64 * (delta[0] * delta[0] + delta[1] * delta[1]);
    }
    (cost, grad)
}
pub fn to_svg(
    graph: &Graph,
    communities: &Communities,
    output: &Path,
    labels: &BTreeMap<i64, Value>,
    figsize: (f64, f64),
) -> Result<()> {
    if !figsize.0.is_finite() || !figsize.1.is_finite() || figsize.0 <= 0.0 || figsize.1 <= 0.0 {
        return Err("Figure size must be finite and positive".into());
    }
    let pos = spring_layout(graph)?;
    let assign = assignments(graph, communities);
    let degree = (0..graph.nodes.len())
        .map(|i| analyze::degree(graph, i))
        .collect::<Vec<_>>();
    let max = degree.iter().copied().max().unwrap_or(1).max(1) as f64;
    let width = figsize.0 * 72.0;
    let height = figsize.1 * 72.0;
    let margin = width.min(height) * 0.04;
    // Leave room for labels on the boundary, not only for node circles. Labels
    // are drawn without XML-illegal control characters (#4244).
    let max_label = graph
        .nodes
        .iter()
        .map(|n| {
            strip_xml_illegal(&string(n.attributes.get("label").unwrap_or(&n.id)))
                .split('\n')
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let horizontal_margin = margin.max(max_label as f64 * 7.0 * 0.65 / 2.0 + 8.0);
    let drawing_width = width.max(2.0 * horizontal_margin + 1.0);
    let point = |i: usize| {
        [
            (pos[i][0] + 1.0) * 0.5 * (drawing_width - 2.0 * horizontal_margin) + horizontal_margin,
            (1.0 - pos[i][1]) * 0.5 * (height - 2.0 * margin) + margin,
        ]
    };
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{drawing_width}pt\" height=\"{height}pt\" viewBox=\"0 0 {drawing_width} {height}\">\n<rect x=\"0\" y=\"0\" width=\"{drawing_width}\" height=\"{height}\" fill=\"#1a1a2e\"/>\n<g id=\"edges\">\n"
    );
    for (u, v, e) in graph.iter_edges() {
        let a = point(u);
        let b = point(v);
        let extracted = e
            .attributes
            .get("confidence")
            .map(|v| v == "EXTRACTED")
            .unwrap_or(true);
        let dash = if extracted {
            ""
        } else {
            " stroke-dasharray=\"3 2\""
        };
        let opacity = if extracted { 0.6 } else { 0.3 };
        svg.push_str(&format!("<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#aaaaaa\" stroke-width=\"0.8\" opacity=\"{opacity}\"{dash}/>\n",a[0],a[1],b[0],b[1]));
    }
    svg.push_str("</g>\n<g id=\"nodes\">\n");
    for (i, n) in graph.nodes.iter().enumerate() {
        let p = point(i);
        let size = 300.0 + 1200.0 * degree[i] as f64 / max;
        let color = COMMUNITY_COLORS[assign[i].unwrap_or(0).rem_euclid(10) as usize];
        let label = strip_xml_illegal(&string(n.attributes.get("label").unwrap_or(&n.id)));
        svg.push_str(&format!("<g data-node-id=\"{}\"><title>{}</title><circle cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{color}\" opacity=\"0.9\"/></g>\n",xml_attr(&strip_xml_illegal(&string(&n.id))),xml(&label),p[0],p[1],size.sqrt()*0.5));
    }
    svg.push_str("</g>\n<g id=\"labels\" font-family=\"DejaVu Sans, sans-serif\" font-size=\"7\" fill=\"white\" text-anchor=\"middle\" dominant-baseline=\"central\">\n");
    for (i, n) in graph.nodes.iter().enumerate() {
        let p = point(i);
        let label = strip_xml_illegal(&string(n.attributes.get("label").unwrap_or(&n.id)));
        let lines = label.split('\n').collect::<Vec<_>>();
        for (j, line) in lines.iter().enumerate() {
            let y = p[1] + (j as f64 - (lines.len() - 1) as f64 / 2.0) * 8.4;
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{y}\">{}</text>\n",
                p[0],
                xml(line)
            ));
        }
    }
    svg.push_str("</g>\n");
    if !labels.is_empty() {
        let entries = labels
            .iter()
            .map(|(cid, label)| {
                (
                    *cid,
                    format!(
                        "{} ({})",
                        string(label),
                        communities
                            .iter()
                            .find(|(c, _)| c == cid)
                            .map(|(_, m)| m.len())
                            .unwrap_or(0)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let w = entries
            .iter()
            .map(|(_, s)| s.chars().count())
            .max()
            .unwrap_or(0) as f64
            * 4.8
            + 30.0;
        svg.push_str(&format!("<g id=\"legend\" font-family=\"DejaVu Sans, sans-serif\" font-size=\"8\" fill=\"white\"><rect x=\"12\" y=\"12\" width=\"{w}\" height=\"{}\" rx=\"3\" fill=\"#2a2a4e\" opacity=\"0.7\"/>\n",entries.len()*14+12));
        for (i, (cid, label)) in entries.iter().enumerate() {
            let y = 24 + i * 14;
            svg.push_str(&format!("<rect x=\"18\" y=\"{}\" width=\"10\" height=\"8\" fill=\"{}\"/><text x=\"34\" y=\"{y}\">{}</text>\n",y-7,COMMUNITY_COLORS[cid.rem_euclid(10) as usize],xml(&strip_xml_illegal(label))));
        }
        svg.push_str("</g>\n")
    }
    svg.push_str("</svg>\n");
    paths::write_text_atomic(output, &svg).map_err(crate::Error::from)
}
