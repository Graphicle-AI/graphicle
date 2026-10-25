// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/reflect.py
//! Deterministic work-memory aggregation from graphify/reflect.py. Derived
//! lessons and source fingerprints remain separate from the structural graph.

#[derive(Clone, Copy)]
pub struct ReflectOptions<'a> {
    pub graph: Option<&'a Path>,
    pub analysis: Option<&'a Path>,
    pub labels: Option<&'a Path>,
    pub now: &'a DateTime,
    pub half_life: f64,
    pub min_corroboration: i64,
}
use crate::{Result, decimal_data, fsutil, python_compat, python_datetime::DateTime, python_json};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const LEARNING_SIDECAR_NAME: &str = ".graphicle_learning.json";
pub const OUTCOMES: [&str; 3] = ["useful", "dead_end", "corrected"];
const UNCATEGORIZED: &str = "Uncategorized";
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| "AttributeError: expected mapping".into())
}
fn array(value: &Value) -> Result<&[Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "TypeError: expected list".into())
}
fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| "TypeError: expected string".into())
}
fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&Value::Null)
}
fn str_field(map: &Map<String, Value>, key: &str) -> Result<String> {
    match map.get(key) {
        None => Ok(String::new()),
        Some(v) => Ok(text(v)?.to_owned()),
    }
}
fn default_field(map: &Map<String, Value>, key: &str, default: Value) -> Value {
    map.get(key).cloned().unwrap_or(default)
}
fn display(value: &Value) -> String {
    python_compat::string(value)
}
fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn read_text(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path).map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
}
fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&read_text(path).ok()?).ok()
}
fn md_files(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".md"))
        })
        .collect();
    files.sort();
    files
}
fn hexadecimal(chars: &[char]) -> Option<u32> {
    let raw: String = chars
        .iter()
        .map(|ch| {
            decimal_data::DECIMAL
                .iter()
                .find(|(a, b)| (*a..=*b).contains(&(*ch as u32)))
                .map(|(a, _)| char::from_digit((*ch as u32 - a) % 10, 10).unwrap())
                .unwrap_or(*ch)
        })
        .collect();
    let raw = raw.trim_matches(char::is_whitespace);
    let negative = raw.starts_with('-');
    let raw = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let prefixed = raw.starts_with("0x") || raw.starts_with("0X");
    let raw = if prefixed {
        raw[2..].strip_prefix('_').unwrap_or(&raw[2..])
    } else {
        raw
    };
    let mut value = 0;
    for part in raw.split('_') {
        if part.is_empty() {
            return None;
        }
        for ch in part.chars() {
            value = value * 16 + ch.to_digit(16)?;
        }
    }
    (!negative || value == 0).then_some(value)
}
pub fn yaml_unescape(input: &str) -> Result<String> {
    let chars: Vec<_> = input.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let simple = match chars[i + 1] {
                'n' => Some('\n'),
                'r' => Some('\r'),
                't' => Some('\t'),
                '0' => Some('\0'),
                '"' => Some('"'),
                '\\' => Some('\\'),
                'L' => Some('\u{2028}'),
                'P' => Some('\u{2029}'),
                _ => None,
            };
            if let Some(ch) = simple {
                result.push(ch);
                i += 2;
                continue;
            }
            let length = match chars[i + 1] {
                'x' => 2,
                'u' => 4,
                _ => 0,
            };
            if length > 0
                && i + length + 2 <= chars.len()
                && let Some(value) = hexadecimal(&chars[i + 2..i + 2 + length])
            {
                let ch = char::from_u32(value)
                    .ok_or("UnicodeError: unpaired surrogate outside the native UTF-8 interface")?;
                result.push(ch);
                i += length + 2;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    Ok(result)
}
fn quoted_items(input: &str) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let chars: Vec<_> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < chars.len() {
            if chars[end] == '\\' {
                if end + 1 == chars.len() {
                    break;
                }
                end += 2;
            } else if chars[end] == '"' {
                break;
            } else {
                end += 1;
            }
        }
        if chars.get(end) == Some(&'"') {
            items.push(json!(yaml_unescape(
                &chars[start..end].iter().collect::<String>()
            )?));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    Ok(items)
}
pub fn parse_memory_doc(input: &str) -> Result<Option<Value>> {
    if !input.starts_with("---") {
        return Ok(None);
    }
    let mut lines = fsutil::split_lines(input);
    if lines.next().is_none_or(|s| strip(s) != "---") {
        return Ok(None);
    }
    let mut fields = Map::new();
    fields.insert("source_nodes".into(), json!([]));
    for line in lines {
        if strip(line) == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = strip(value);
        if key == "source_nodes" && value.starts_with('[') && value.ends_with(']') {
            fields.insert(key.into(), json!(quoted_items(&value[1..value.len() - 1])?));
            continue;
        }
        if matches!(
            key,
            "type" | "date" | "question" | "outcome" | "correction" | "contributor"
        ) && value.starts_with('"')
            && value.ends_with('"')
            && value.len() >= 2
        {
            fields.insert(
                key.into(),
                json!(yaml_unescape(&value[1..value.len() - 1])?),
            );
        }
    }
    Ok(Some(Value::Object(fields)))
}
pub fn load_memory_docs(directory: &Path) -> Result<Vec<Value>> {
    let mut docs = Vec::new();
    for path in md_files(directory) {
        let Ok(body) = read_text(&path) else {
            continue;
        };
        if let Some(mut parsed) = parse_memory_doc(&body)? {
            parsed["_path"] = json!(path.file_name().unwrap().to_string_lossy());
            docs.push(parsed);
        }
    }
    docs.sort_by_key(|d| {
        (
            d.get("date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            d["_path"].as_str().unwrap().to_owned(),
        )
    });
    Ok(docs)
}
pub fn load_node_community(
    graph_path: &Path,
    analysis_path: &Path,
    labels_path: &Path,
) -> Result<Option<Value>> {
    if !graph_path.exists() || !analysis_path.exists() {
        return Ok(None);
    }
    let Some(analysis) = read_json(analysis_path) else {
        return Ok(None);
    };
    let analysis = object(&analysis)?;
    let communities = field(analysis, "communities");
    if !truthy(communities) {
        return Ok(None);
    }
    let communities = object(communities)?;
    let labels = read_json(labels_path).unwrap_or_else(|| json!({}));
    let labels = object(&labels)?;
    let mut id_labels = BTreeMap::new();
    if let Some(data) = read_json(graph_path)
        && let Some(nodes) = object(&data)?.get("nodes")
    {
        for n in array(nodes)? {
            if let Some(n) = n.as_object()
                && !field(n, "id").is_null()
                && !field(n, "label").is_null()
            {
                id_labels.insert(display(field(n, "id")), display(field(n, "label")));
            }
        }
    }
    let mut keys: Vec<_> = communities.keys().collect();
    keys.sort();
    let mut mapping = Map::new();
    for cid in keys {
        let label = labels
            .get(cid)
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(format!("Community {cid}")));
        for nid in array(&communities[cid])? {
            let nid = display(nid);
            mapping.entry(nid.clone()).or_insert_with(|| label.clone());
            if let Some(name) = id_labels.get(&nid) {
                mapping.entry(name.clone()).or_insert_with(|| label.clone());
            }
        }
    }
    Ok(Some(Value::Object(mapping)))
}
pub fn load_known_nodes(path: &Path) -> Result<Option<BTreeSet<String>>> {
    let Some(data) = read_json(path) else {
        return Ok(None);
    };
    let data = object(&data)?;
    let Some(nodes) = field(data, "nodes").as_array() else {
        return Ok(None);
    };
    let mut known = BTreeSet::new();
    for n in nodes {
        if let Some(n) = n.as_object() {
            for key in ["id", "label"] {
                if !field(n, key).is_null() {
                    known.insert(display(field(n, key)));
                }
            }
        }
    }
    Ok((!known.is_empty()).then_some(known))
}
pub fn doc_community(nodes: &[String], mapping: Option<&Map<String, Value>>) -> Result<String> {
    let Some(mapping) = mapping.filter(|m| !m.is_empty()) else {
        return Ok(UNCATEGORIZED.into());
    };
    let mut counts = BTreeMap::<String, usize>::new();
    for node in nodes {
        if let Some(label) = mapping.get(node) {
            *counts.entry(text(label)?.to_string()).or_default() += 1;
        }
    }
    Ok(counts
        .into_iter()
        .min_by_key(|(label, count)| (std::cmp::Reverse(*count), label.clone()))
        .map(|(l, _)| l)
        .unwrap_or_else(|| UNCATEGORIZED.into()))
}
pub fn decay(date: &str, now: &DateTime, half_life_days: f64) -> f64 {
    let Some(date) = crate::python_datetime::parse(date) else {
        return 1.0;
    };
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let elapsed = (now.timestamp_micros() - date.timestamp_micros()) as f64 / 1_000_000.0;
    let age = (elapsed / 86400.0).max(0.0);
    0.5f64.powf(age / half_life_days)
}
#[derive(Default)]
struct NodeScore {
    score: f64,
    pos: usize,
    neg: usize,
    last: String,
    provenance: Vec<Value>,
}
#[derive(Default)]
struct Bucket {
    counts: [usize; 4],
    nodes: Vec<(String, NodeScore)>,
    index: BTreeMap<String, usize>,
    dead_ends: Vec<Value>,
    corrections: Vec<Value>,
}
impl Bucket {
    fn record(
        &mut self,
        node: &str,
        sign: i64,
        weight: f64,
        date: &str,
        outcome: &str,
        question: &str,
    ) {
        let index = *self.index.entry(node.into()).or_insert_with(|| {
            let i = self.nodes.len();
            self.nodes.push((node.into(), NodeScore::default()));
            i
        });
        let entry = &mut self.nodes[index].1;
        entry.score += sign as f64 * weight;
        if sign > 0 {
            entry.pos += 1;
        } else if sign < 0 {
            entry.neg += 1;
        }
        if date > entry.last.as_str() {
            entry.last = date.into();
        }
        if matches!(outcome, "useful" | "corrected") {
            entry.provenance.push(json!([date, question, outcome]));
        }
    }
    fn counts(&self) -> Value {
        json!({"useful":self.counts[0],"dead_end":self.counts[1],"corrected":self.counts[2],"unmarked":self.counts[3]})
    }
    fn finalize(&self, k: i64) -> Value {
        let mut preferred = Vec::new();
        let mut tentative = Vec::new();
        let mut contested = Vec::new();
        for (node, entry) in &self.nodes {
            let score = format!("{:.9}", entry.score).parse::<f64>().unwrap();
            if entry.pos > 0 && entry.neg > 0 {
                let verdict = if score > 0.0 {
                    "useful"
                } else if score < 0.0 {
                    "dead end"
                } else {
                    "even"
                };
                contested.push(json!({"node":node,"pos":entry.pos,"neg":entry.neg,"score":score,"verdict":verdict,"last":entry.last}));
            } else if entry.pos > 0 {
                let value = json!({"node":node,"n":entry.pos,"score":score});
                if entry.pos as i64 >= k {
                    preferred.push(value);
                } else {
                    tentative.push(value);
                }
            }
        }
        for values in [&mut preferred, &mut tentative, &mut contested] {
            values.sort_by(|a, b| {
                b["score"]
                    .as_f64()
                    .unwrap()
                    .partial_cmp(&a["score"].as_f64().unwrap())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a["node"].as_str().cmp(&b["node"].as_str()))
            });
        }
        json!({"preferred":preferred,"tentative":tentative,"contested":contested})
    }
    fn result(&self, k: i64) -> Value {
        let mut result = self.finalize(k);
        result["counts"] = self.counts();
        result["dead_ends"] = json!(dedupe_by_question(&self.dead_ends));
        result["corrections"] = json!(dedupe_by_question(&self.corrections));
        result
    }
    fn provenance(&self) -> Value {
        Value::Object(
            self.nodes
                .iter()
                .filter(|(_, n)| !n.provenance.is_empty())
                .map(|(name, n)| (name.clone(), json!(n.provenance)))
                .collect(),
        )
    }
}
pub fn dedupe_by_question(items: &[Value]) -> Vec<Value> {
    let mut latest = BTreeMap::new();
    for item in items {
        latest.insert(
            item.get("question").and_then(Value::as_str).unwrap_or(""),
            item.clone(),
        );
    }
    let mut rows: Vec<_> = latest.into_values().collect();
    rows.sort_by_key(|v| {
        (
            v.get("date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            v.get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )
    });
    rows
}
pub fn aggregate_lessons(
    docs: &[Value],
    mapping: Option<&Map<String, Value>>,
    now: &DateTime,
    half_life_days: f64,
    min_corroboration: i64,
    known: Option<&BTreeSet<String>>,
) -> Result<Value> {
    let mut overall = Bucket::default();
    let mut community_index = BTreeMap::new();
    let mut by_community: Vec<(String, Bucket)> = Vec::new();
    for doc in docs {
        let doc = object(doc)?;
        let outcome = doc.get("outcome").and_then(Value::as_str).unwrap_or("");
        let date = str_field(doc, "date")?;
        let mut seen = BTreeSet::new();
        let mut nodes = Vec::new();
        if let Some(values) = doc.get("source_nodes") {
            for node in array(values)? {
                let n = text(node)?;
                if known.is_none_or(|known| known.contains(n)) && seen.insert(n.to_string()) {
                    nodes.push(n.to_string());
                }
            }
        }
        let community = doc_community(&nodes, mapping)?;
        let index = *community_index.entry(community.clone()).or_insert_with(|| {
            let i = by_community.len();
            by_community.push((community, Bucket::default()));
            i
        });
        let sign = match outcome {
            "useful" => 1,
            "dead_end" | "corrected" => -1,
            _ => 0,
        };
        let weight = if sign != 0 {
            decay(&date, now, half_life_days)
        } else {
            0.0
        };
        let count = OUTCOMES.iter().position(|s| *s == outcome).unwrap_or(3);
        let question = str_field(doc, "question")?;
        for target in [&mut overall, &mut by_community[index].1] {
            target.counts[count] += 1;
            if sign != 0 {
                for node in &nodes {
                    target.record(node, sign, weight, &date, outcome, &question);
                }
            }
            if outcome == "dead_end" {
                target
                    .dead_ends
                    .push(json!({"question":question,"nodes":nodes,"date":date}));
            } else if outcome == "corrected" {
                target.corrections.push(json!({"question":question,"correction":default_field(doc,"correction",json!("")),"date":date}));
            }
        }
    }
    let mut result = overall.result(min_corroboration);
    result["total"] = json!(docs.len());
    result["min_corroboration"] = json!(min_corroboration);
    result["by_community"] = if mapping.is_some_and(|m| !m.is_empty()) {
        Value::Object(
            by_community
                .into_iter()
                .map(|(name, b)| (name, b.result(min_corroboration)))
                .collect(),
        )
    } else {
        json!({})
    };
    result["_node_provenance"] = overall.provenance();
    Ok(result)
}

fn render_bucket(out: &mut Vec<String>, data: &Value, k: &Value) -> Result<()> {
    let data = object(data)?;
    let preferred = array(field(data, "preferred"))?;
    let tentative = array(field(data, "tentative"))?;
    let contested = array(field(data, "contested"))?;
    let dead_ends = array(field(data, "dead_ends"))?;
    let corrections = array(field(data, "corrections"))?;
    if !preferred.is_empty() {
        out.extend([
            format!(
                "**Preferred sources** — corroborated by ≥{} useful results; start here.",
                display(k)
            ),
            String::new(),
        ]);
        for e in preferred {
            out.push(format!(
                "- `{}` ({}× useful)",
                display(&e["node"]),
                display(&e["n"])
            ));
        }
        out.push(String::new());
    }
    if !tentative.is_empty() {
        out.extend([
            format!(
                "**Tentative** — useful in fewer than {} results; verify before relying.",
                display(k)
            ),
            String::new(),
        ]);
        for e in tentative {
            out.push(format!(
                "- `{}` ({}× useful)",
                display(&e["node"]),
                display(&e["n"])
            ));
        }
        out.push(String::new());
    }
    if !contested.is_empty() {
        out.extend([
            "**Contested** — mixed signals; recency decides.".into(),
            String::new(),
        ]);
        for e in contested {
            let day: String = text(&e["last"])?.chars().take(10).collect();
            let verdict = if e["verdict"] == "even" {
                "evenly split".into()
            } else {
                format!("recency leans **{}**", display(&e["verdict"]))
            };
            out.push(format!(
                "- `{}` — {}× useful, {}× dead end/corrected → {verdict}{}",
                display(&e["node"]),
                display(&e["pos"]),
                display(&e["neg"]),
                if day.is_empty() {
                    String::new()
                } else {
                    format!(" (latest {day})")
                }
            ));
        }
        out.push(String::new());
    }
    if !dead_ends.is_empty() {
        out.extend([
            "**Known dead ends** — led nowhere; don't re-derive.".into(),
            String::new(),
        ]);
        for d in dead_ends {
            let nodes = array(&d["nodes"])?
                .iter()
                .map(|n| format!("`{}`", display(n)))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!(
                "- \"{}\"{}",
                display(&d["question"]),
                if nodes.is_empty() {
                    String::new()
                } else {
                    format!(" — {nodes}")
                }
            ));
        }
        out.push(String::new());
    }
    if !corrections.is_empty() {
        out.extend([
            "**Corrections** — do these differently.".into(),
            String::new(),
        ]);
        for c in corrections {
            out.push(format!(
                "- \"{}\" → {}",
                display(&c["question"]),
                display(&c["correction"])
            ));
        }
        out.push(String::new());
    }
    if preferred.is_empty()
        && tentative.is_empty()
        && contested.is_empty()
        && dead_ends.is_empty()
        && corrections.is_empty()
    {
        out.extend(["_No marked outcomes yet._".into(), String::new()]);
    }
    Ok(())
}
pub fn render_lessons_md(aggregate: &Value) -> Result<String> {
    let agg = object(aggregate)?;
    let counts = object(field(agg, "counts"))?;
    let k = default_field(agg, "min_corroboration", json!(2));
    let total = field(agg, "total");
    let mut out = vec![
        "# Lessons".into(),
        String::new(),
        format!(
            "_Auto-generated by `{} reflect` from {} session {} in .graphicle/memory/. Deterministic; no LLM. Use for orientation — verify before relying, and revisit dead ends if the code has changed since._",
            crate::cli::PROGRAM,
            display(total),
            if total == &json!(1) {
                "memory"
            } else {
                "memories"
            }
        ),
        String::new(),
        "## Summary".into(),
        String::new(),
        format!(
            "- {} useful · {} dead ends · {} corrected · {} unmarked",
            display(field(counts, "useful")),
            display(field(counts, "dead_end")),
            display(field(counts, "corrected")),
            display(field(counts, "unmarked"))
        ),
        String::new(),
        "## Lessons".into(),
        String::new(),
    ];
    render_bucket(&mut out, aggregate, &k)?;
    let communities = object(field(agg, "by_community"))?;
    if !communities.is_empty() {
        out.extend(["## By topic".into(), String::new()]);
        let mut keys: Vec<_> = communities.keys().collect();
        keys.sort_by_key(|label| (*label == UNCATEGORIZED, label.as_str()));
        for label in keys {
            out.extend([format!("### {label}"), String::new()]);
            render_bucket(&mut out, &communities[label], &k)?;
        }
    }
    Ok(out.join("\n").trim_end_matches('\n').to_owned() + "\n")
}
fn mtime(path: &Path) -> Option<f64> {
    let time = fs::metadata(path).ok()?.modified().ok()?;
    Some(match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    })
}
pub fn lessons_fresh(
    output: &Path,
    memory: &Path,
    graph: Option<&Path>,
    analysis: Option<&Path>,
    labels: Option<&Path>,
) -> bool {
    let Some(output_time) = mtime(output) else {
        return false;
    };
    let mut newest = 0.0f64;
    for file in md_files(memory) {
        if let Some(time) = mtime(&file) {
            newest = newest.max(time);
        }
    }
    for file in [graph, analysis, labels].into_iter().flatten() {
        if let Some(time) = mtime(file) {
            newest = newest.max(time);
        }
    }
    output_time >= newest
}
pub type IdLabelMaps = (Map<String, Value>, Map<String, Value>, Map<String, Value>);
pub fn build_id_label_maps(path: &Path) -> Result<IdLabelMaps> {
    let (mut ids, mut labels, mut by_id) = (Map::new(), Map::<String, Value>::new(), Map::new());
    let Some(data) = read_json(path) else {
        return Ok((ids, labels, by_id));
    };
    let data = object(&data)?;
    if let Some(nodes) = data.get("nodes") {
        for n in array(nodes)? {
            if let Some(node) = n.as_object() {
                if field(node, "id").is_null() {
                    continue;
                }
                let nid = display(field(node, "id"));
                ids.insert(nid.clone(), json!(nid));
                by_id.insert(nid.clone(), n.clone());
                if !field(node, "label").is_null() {
                    labels
                        .entry(display(field(node, "label")))
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap()
                        .push(json!(nid));
                }
            }
        }
    }
    Ok((ids, labels, by_id))
}
pub fn resolve_canonical_id(
    cited: &str,
    ids: &Map<String, Value>,
    labels: &Map<String, Value>,
) -> Option<Value> {
    if let Some(id) = ids.get(cited) {
        return Some(id.clone());
    }
    labels
        .get(cited)
        .and_then(Value::as_array)
        .filter(|v| v.len() == 1)
        .map(|v| v[0].clone())
}
pub fn resolve_source_path(source: &str, graph: &Path) -> Option<PathBuf> {
    if source.is_empty() {
        return None;
    }
    let path = Path::new(source);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let output = parent(graph);
    let mut candidates = Vec::new();
    if let Ok(marker) = read_text(&output.join(".graphicle_root")) {
        let marker = strip(marker.strip_prefix('\u{feff}').unwrap_or(&marker));
        if !marker.is_empty() {
            candidates.push(PathBuf::from(marker));
        }
    }
    if output.file_name().is_some_and(|s| {
        s == crate::paths::DEFAULT_OUT_DIR || s == crate::paths::OUT_DIR_NAME.as_str()
    }) {
        candidates.extend([parent(output).to_path_buf(), output.to_path_buf()]);
    } else {
        candidates.extend([output.to_path_buf(), parent(output).to_path_buf()]);
    }
    candidates.push(PathBuf::from("."));
    let mut seen = BTreeSet::new();
    for base in candidates {
        if seen.insert(fsutil::path_string(&base)) {
            let candidate = base.join(path);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
pub fn content_hash(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .unwrap_or_default()
}
pub fn code_fingerprint(node: Option<&Value>, graph: &Path) -> Result<String> {
    let Some(node) = node.filter(|n| truthy(n)) else {
        return Ok(String::new());
    };
    let node = object(node)?;
    let source = field(node, "source_file");
    let source = if truthy(source) { text(source)? } else { "" };
    Ok(resolve_source_path(source, graph)
        .map(|p| content_hash(&p))
        .unwrap_or_default())
}
pub fn provenance_for(node: &str, provenance: &Map<String, Value>) -> Result<Vec<Value>> {
    let Some(events) = provenance.get(node) else {
        return Ok(vec![]);
    };
    let mut events: Vec<_> = array(events)?.iter().collect();
    for event in &events {
        if array(event)?.len() != 3 {
            return Err("ValueError: expected date/question/outcome".into());
        }
        text(&event[0])?;
        text(&event[1])?;
    }
    events.sort_by(|a, b| {
        b[0].as_str()
            .cmp(&a[0].as_str())
            .then_with(|| b[1].as_str().cmp(&a[1].as_str()))
    });
    Ok(events
        .into_iter()
        .take(5)
        .map(|e| json!({"q":e[1],"date":e[0],"outcome":e[2]}))
        .collect())
}
pub fn build_learning_overlay(aggregate: &Value, graph: &Path, now: &DateTime) -> Result<Value> {
    let agg = object(aggregate)?;
    let (ids, labels, by_id) = build_id_label_maps(graph)?;
    let default_prov = Map::new();
    let provenance = if let Some(value) = agg.get("_node_provenance") {
        object(value)?
    } else {
        &default_prov
    };
    let mut nodes = Map::new();
    for status in ["preferred", "tentative", "contested"] {
        let Some(entries) = agg.get(status) else {
            continue;
        };
        for entry in array(entries)? {
            let entry = object(entry)?;
            let cited = text(field(entry, "node"))?;
            let Some(cid) = resolve_canonical_id(cited, &ids, &labels) else {
                continue;
            };
            let cid = text(&cid)?;
            if nodes.contains_key(cid) {
                continue;
            }
            let node = by_id.get(cid);
            let trail = provenance_for(cited, provenance)?;
            let node_label = node
                .and_then(Value::as_object)
                .map(|n| default_field(n, "label", json!(cited)))
                .unwrap_or_else(|| json!(cited));
            let source = node
                .and_then(|n| n.get("source_file"))
                .filter(|v| truthy(v))
                .map(display)
                .unwrap_or_default();
            let mut out = json!({"status":status,"score":field(entry,"score"),"uses":default_field(entry,"n",default_field(entry,"pos",json!(0))),"last":default_field(entry,"last",json!("")),
                "label":display(&node_label),"source_file":source,"code_fingerprint":code_fingerprint(node,graph)?,"provenance":trail});
            if status == "contested" {
                out["verdict"] = default_field(entry, "verdict", json!("even"));
                out["neg"] = default_field(entry, "neg", json!(0));
            } else if !truthy(&out["last"]) && !trail.is_empty() {
                out["last"] = trail[0]["date"].clone();
            }
            nodes.insert(cid.to_string(), out);
        }
    }
    Ok(json!({"version":1,"generated_at":now.isoformat(),"nodes":nodes}))
}
fn sorted_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map.iter().collect();
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted_json(v)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted_json).collect()),
        _ => value.clone(),
    }
}
pub fn write_learning_sidecar(aggregate: &Value, graph: &Path, now: &DateTime) -> Result<PathBuf> {
    let overlay = build_learning_overlay(aggregate, graph, now)?;
    let sidecar = parent(graph).join(LEARNING_SIDECAR_NAME);
    let mut bytes = Vec::new();
    python_json::write(&mut bytes, &sorted_json(&overlay), Some(2), false)
        .map_err(crate::Error::from)?;
    bytes.push(b'\n');
    fs::write(&sidecar, bytes).map_err(crate::Error::from)?;
    Ok(sidecar)
}
pub fn is_stale(entry: &Map<String, Value>, graph: &Path) -> Result<bool> {
    let source = field(entry, "source_file");
    if !truthy(source) {
        return Ok(false);
    }
    let Some(path) = resolve_source_path(text(source)?, graph) else {
        return Ok(true);
    };
    let stored = field(entry, "code_fingerprint");
    if !truthy(stored) {
        return Ok(true);
    }
    Ok(json!(content_hash(&path)) != *stored)
}
pub fn load_learning_overlay(graph: &Path) -> Result<Value> {
    let Some(data) = read_json(&parent(graph).join(LEARNING_SIDECAR_NAME)) else {
        return Ok(json!({}));
    };
    let data = object(&data)?;
    let Some(nodes) = field(data, "nodes").as_object() else {
        return Ok(json!({}));
    };
    let mut out = Map::new();
    for (id, entry) in nodes {
        if let Some(entry) = entry.as_object() {
            let mut entry = entry.clone();
            let stale = is_stale(&entry, graph)?;
            entry.insert("stale".into(), json!(stale));
            out.insert(id.clone(), Value::Object(entry));
        }
    }
    Ok(Value::Object(out))
}
pub fn reflect(
    memory: &Path,
    output: &Path,
    context: ReflectOptions<'_>,
) -> Result<(PathBuf, Value)> {
    let ReflectOptions {
        graph,
        analysis,
        labels,
        now,
        half_life,
        min_corroboration,
    } = context;

    let docs = load_memory_docs(memory)?;
    let mut mapping = None;
    let mut known = None;
    if let Some(graph) = graph {
        let a = analysis
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent(graph).join(".graphicle_analysis.json"));
        let l = labels
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent(graph).join(".graphicle_labels.json"));
        mapping = load_node_community(graph, &a, &l)?;
        known = load_known_nodes(graph)?;
    }
    let aggregate = aggregate_lessons(
        &docs,
        mapping.as_ref().and_then(Value::as_object),
        now,
        half_life,
        min_corroboration,
        known.as_ref(),
    )?;
    fs::create_dir_all(parent(output)).map_err(crate::Error::from)?;
    fs::write(output, render_lessons_md(&aggregate)?).map_err(crate::Error::from)?;
    if let Some(graph) = graph {
        let _ = write_learning_sidecar(&aggregate, graph, now);
    }
    Ok((output.to_path_buf(), aggregate))
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let operation = request["operation"].as_str().ok_or("Expected operation")?;
    let path = |name| -> Result<&Path> { Ok(Path::new(text(&request[name])?)) };
    let optional_path = |name| request.get(name).and_then(Value::as_str).map(Path::new);
    let now = || -> Result<DateTime> {
        match request.get("now").filter(|v| !v.is_null()) {
            Some(v) => crate::python_datetime::parse(text(v)?)
                .ok_or_else(|| "ValueError: invalid now datetime".into()),
            None => Ok(DateTime::now()),
        }
    };
    let half_life = || {
        request
            .get("half_life_days")
            .and_then(Value::as_f64)
            .unwrap_or(30.0)
    };
    let min_corroboration = || {
        request
            .get("min_corroboration")
            .and_then(Value::as_i64)
            .unwrap_or(2)
    };
    match operation {
        "reflect.parse" => Ok(parse_memory_doc(text(&request["text"])?)?.unwrap_or(Value::Null)),
        "reflect.unescape" => Ok(json!(yaml_unescape(text(&request["text"])?)?)),
        "reflect.parse_dt" => Ok(json!(
            crate::python_datetime::parse(text(&request["date"])?).map(|d| d.isoformat())
        )),
        "reflect.decay" => Ok(json!(decay(text(&request["date"])?, &now()?, half_life()))),
        "reflect.load" => Ok(json!(load_memory_docs(path("memory_dir")?)?)),
        "reflect.node_community" => Ok(load_node_community(
            path("graph_path")?,
            path("analysis_path")?,
            path("labels_path")?,
        )?
        .unwrap_or(Value::Null)),
        "reflect.known_nodes" => Ok(json!(load_known_nodes(path("graph_path")?)?)),
        "reflect.doc_community" => {
            let nodes = array(&request["nodes"])?
                .iter()
                .map(|v| text(v).map(str::to_owned))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!(doc_community(
                &nodes,
                request.get("node_community").and_then(Value::as_object)
            )?))
        }
        "reflect.aggregate" => {
            let known = request
                .get("known_nodes")
                .filter(|v| !v.is_null())
                .map(|v| -> Result<BTreeSet<String>> {
                    array(v)?
                        .iter()
                        .map(|v| text(v).map(str::to_owned))
                        .collect()
                })
                .transpose()?;
            aggregate_lessons(
                array(&request["docs"])?,
                request.get("node_community").and_then(Value::as_object),
                &now()?,
                half_life(),
                min_corroboration(),
                known.as_ref(),
            )
        }
        "reflect.dedupe" => Ok(json!(dedupe_by_question(array(&request["items"])?))),
        "reflect.render" => Ok(json!(render_lessons_md(&request["agg"])?)),
        "reflect.render_bucket" => {
            let mut lines = array(&request["lines"])?
                .iter()
                .map(|v| text(v).map(str::to_owned))
                .collect::<Result<Vec<_>>>()?;
            render_bucket(&mut lines, &request["data"], &request["k"])?;
            Ok(json!(lines))
        }
        "reflect.fresh" => Ok(json!(lessons_fresh(
            path("out_path")?,
            path("memory_dir")?,
            optional_path("graph_path"),
            optional_path("analysis_path"),
            optional_path("labels_path")
        ))),
        "reflect.run" => {
            let (out, agg) = reflect(
                path("memory_dir")?,
                path("out_path")?,
                ReflectOptions {
                    graph: optional_path("graph_path"),
                    analysis: optional_path("analysis_path"),
                    labels: optional_path("labels_path"),
                    now: &now()?,
                    half_life: half_life(),
                    min_corroboration: min_corroboration(),
                },
            )?;
            Ok(json!([fsutil::path_string(&out), agg]))
        }
        "reflect.id_label_maps" => {
            let (a, b, c) = build_id_label_maps(path("graph_path")?)?;
            Ok(json!([a, b, c]))
        }
        "reflect.resolve_id" => Ok(resolve_canonical_id(
            text(&request["cited"])?,
            object(&request["id_set"])?,
            object(&request["label_to_ids"])?,
        )
        .unwrap_or(Value::Null)),
        "reflect.resolve_path" => Ok(json!(
            resolve_source_path(text(&request["src"])?, path("graph_path")?)
                .map(|p| fsutil::path_string(&p))
        )),
        "reflect.hash" => Ok(json!(content_hash(path("path")?))),
        "reflect.fingerprint" => Ok(json!(code_fingerprint(
            request.get("node"),
            path("graph_path")?
        )?)),
        "reflect.provenance" => Ok(json!(provenance_for(
            text(&request["node"])?,
            object(&request["prov_map"])?
        )?)),
        "reflect.overlay" => build_learning_overlay(&request["agg"], path("graph_path")?, &now()?),
        "reflect.write_sidecar" => Ok(json!(fsutil::path_string(&write_learning_sidecar(
            &request["agg"],
            path("graph_path")?,
            &now()?
        )?))),
        "reflect.load_overlay" => load_learning_overlay(path("graph_path")?),
        "reflect.is_stale" => Ok(json!(is_stale(
            object(&request["entry"])?,
            path("graph_path")?
        )?)),
        _ => Err("Unsupported reflection operation".into()),
    }
}
