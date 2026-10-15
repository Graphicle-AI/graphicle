// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/export.py
//! Native graphify/export.py: protected artifacts and graph serialization.
pub use crate::export_formats::{to_graphml, to_svg};
pub use crate::export_obsidian::{VaultOptions, to_canvas, to_obsidian};
pub use crate::exporters::{
    base::COMMUNITY_COLORS,
    graphdb::{push_to_falkordb, push_to_neo4j},
    html::{to_html, to_html as generate_html},
};
use crate::{
    Result, build::Diagnostics, cluster::Communities, exporters::common::*, fsutil, graph::Graph,
    paths, process, security,
};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::Duration,
};
use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};

pub const BACKUP_ARTIFACTS: [&str; 7] = [
    "graph.json",
    "GRAPH_REPORT.md",
    ".graphicle_labels.json",
    ".graphicle_analysis.json",
    "manifest.json",
    ".graphicle_semantic_marker",
    "cost.json",
];
pub const DEDUP_SUFFIX_RESERVE: i64 = 5;
pub const COMMUNITY_PREFIX: &str = "_COMMUNITY_";
/// Increment when the persisted graph structure changes incompatibly for
/// consumers (graph.json `graph.schema_version`).
pub const GRAPH_SCHEMA_VERSION: i64 = 1;
/// graph.json's writer stamp: `graph.graphicle_version` (upstream records the
/// installed release under its own product's key, #4167).
pub const GRAPH_VERSION_KEY: &str = "graphicle_version";
/// The writer stamp the upstream tool leaves in a graph.json it wrote.
const UPSTREAM_GRAPH_VERSION_KEY: &str = "graphify_version";
/// The version graph.json records under [`GRAPH_VERSION_KEY`], in the form
/// the extraction cache spells the engine's version: the Graphify release it
/// ports, then its own version.
pub fn graphicle_version() -> String {
    format!(
        "{}-graphicle.{}",
        crate::UPSTREAM_VERSION,
        env!("CARGO_PKG_VERSION")
    )
}
/// Record the writer: the stamp takes the slot of an earlier writer stamp
/// (the engine's, or the one a Graphify-written graph.json carries), so the
/// document names one writer at the position upstream overwrites in place.
fn stamp_writer_version(attributes: &mut Map<String, Value>) {
    let mut stamped = Map::new();
    let mut version = Some(json!(graphicle_version()));
    for (key, value) in std::mem::take(attributes) {
        if key == GRAPH_VERSION_KEY || key == UPSTREAM_GRAPH_VERSION_KEY {
            if let Some(version) = version.take() {
                stamped.insert(GRAPH_VERSION_KEY.into(), version);
            }
        } else {
            stamped.insert(key, value);
        }
    }
    if let Some(version) = version {
        stamped.insert(GRAPH_VERSION_KEY.into(), version);
    }
    *attributes = stamped;
}
/// The new graph's nodes as `_lost_saved_semantic_nodes` reads them: id,
/// tier, source file and label, the only fields it consults.
fn semantic_check_nodes(graph: &Graph) -> Vec<Value> {
    graph
        .nodes
        .iter()
        .map(|node| {
            let mut record = Map::new();
            record.insert("id".into(), node.id.clone());
            for key in ["_origin", "source_file", "label"] {
                if let Some(value) = node.attributes.get(key) {
                    record.insert(key.into(), value.clone());
                }
            }
            Value::Object(record)
        })
        .collect()
}
static DIGITS: LazyLock<Vec<(u32, u32)>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("assets/export/digit_ranges.json"))
        .expect("pinned digit ranges")
});
pub fn python_word(c: char) -> bool {
    let cp = c as u32;
    crate::unicode15::WORD_RANGES
        .binary_search_by(|(start, end)| {
            if cp < *start {
                std::cmp::Ordering::Greater
            } else if cp > *end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}
pub fn obsidian_tag(name: &str) -> String {
    let tag = name
        .replace(' ', "_")
        .chars()
        .filter(|c| python_word(*c) || ['-', '/'].contains(c))
        .collect::<String>();
    if tag.trim_matches(['_', '-', '/']).is_empty() {
        "unnamed".into()
    } else if tag
        .chars()
        .all(|c| DIGITS.iter().any(|(a, b)| *a <= c as u32 && c as u32 <= *b))
    {
        format!("c{tag}")
    } else {
        tag
    }
}
pub fn strip_diacritics(text: &Value) -> String {
    let text = if text.is_null() {
        String::new()
    } else {
        string(text)
    };
    text.nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect()
}
pub fn yaml_str(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut out = String::new();
    for c in string(value).chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            _ => out.push(c),
        }
    }
    out
}
pub fn strip_xml_illegal(s: &str) -> String {
    s.chars()
        .filter(|c| *c >= ' ' || ['\t', '\n', '\r'].contains(c))
        .collect()
}
pub fn cypher_escape(s: &str) -> String {
    s.chars()
        .filter(|c| *c >= ' ' || *c == '\t')
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
}
pub fn cypher_label(raw: &str, fallback: &str) -> String {
    let s = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>();
    if s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        s
    } else {
        fallback.into()
    }
}
pub fn cap_filename(s: &str, limit: i64) -> String {
    if (s.len() as i64) <= limit {
        return s.into();
    }
    let digest = format!("{:x}", Sha1::digest(s.as_bytes()));
    let keep = limit.saturating_sub(9);
    let mut end = if keep < 0 {
        s.len().saturating_sub(keep.unsigned_abs() as usize)
    } else {
        s.len().min(keep as usize)
    };
    while !s.is_char_boundary(end) {
        end -= 1
    }
    format!("{}_{}", &s[..end], &digest[..8])
}
pub fn obsidian_safe_stem(label: &str, limit: i64) -> String {
    let label = label
        .chars()
        .map(|c| {
            if (c as u32) < 32 || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !"\\/*?:\"<>|#^[]".contains(*c))
        .collect::<String>();
    let mut cleaned = label
        .trim_matches(crate::dedup_strings::whitespace)
        .to_owned();
    if let Some((base, ext)) = cleaned.rsplit_once('.')
        && ["md", "mdx", "qmd", "markdown"].contains(&ext.to_ascii_lowercase().as_str())
    {
        cleaned = base.to_owned()
    }
    if cleaned.starts_with('.') && cleaned.trim_start_matches('.').chars().any(python_word) {
        cleaned = format!("dot-{}", cleaned.trim_start_matches('.'))
    }
    if !cleaned.chars().any(python_word) {
        "unnamed".into()
    } else {
        cap_filename(&cleaned, limit)
    }
}
pub fn is_generated_note(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(4096 * 4).read_to_end(&mut bytes).is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .take(4096)
        .collect::<String>();
    if path
        .file_name()
        .is_some_and(|s| s.to_string_lossy().starts_with(COMMUNITY_PREFIX))
        && head.contains("FROM #community/")
    {
        return true;
    }
    if !head.starts_with("---") {
        return false;
    }
    for line in fsutil::split_lines(&head).skip(1) {
        if line.trim_matches(crate::dedup_strings::whitespace) == "---" {
            return false;
        }
        if let Some(rest) = line
            .trim_start_matches(crate::dedup_strings::whitespace)
            .strip_prefix('-')
            && rest
                .chars()
                .next()
                .is_some_and(crate::dedup_strings::whitespace)
            && let Some(tag) = rest
                .trim_start_matches(crate::dedup_strings::whitespace)
                .strip_prefix("graphicle/")
            && tag
                .chars()
                .next()
                .is_some_and(|c| !crate::dedup_strings::whitespace(c))
        {
            return true;
        }
    }
    false
}
pub fn adopt_pre_manifest_notes(out: &Path) -> BTreeSet<String> {
    fs::read_dir(out)
        .into_iter()
        .flatten()
        .filter_map(|p| p.ok())
        .filter(|p| p.path().extension().is_some_and(|e| e == "md") && is_generated_note(&p.path()))
        .map(|p| p.file_name().to_string_lossy().into_owned())
        .collect()
}
pub fn dedup_node_filenames(
    graph: &Graph,
    mut safe: impl FnMut(&str) -> String,
) -> Result<Vec<String>> {
    let mut used = BTreeSet::new();
    let mut out = vec![];
    for n in &graph.nodes {
        let label = n
            .attributes
            .get("label")
            .unwrap_or(&n.id)
            .as_str()
            .ok_or("Node label must be a string")?;
        let base = safe(label);
        let mut candidate = base.clone();
        let mut i = 1;
        while used.contains(&candidate.to_lowercase()) {
            candidate = format!("{base}_{i}");
            i += 1
        }
        used.insert(candidate.to_lowercase());
        out.push(candidate)
    }
    Ok(out)
}
pub fn attach_hyperedges(graph: &mut Graph, hyperedges: &[Value]) -> Result<()> {
    let mut existing = graph
        .attributes
        .get("hyperedges")
        .cloned()
        .unwrap_or(json!([]));
    let items = existing.as_array_mut().ok_or("Expected hyperedges array")?;
    let mut seen = items
        .iter()
        .filter_map(|h| h.get("id"))
        .filter(|v| truth(v))
        .filter_map(crate::python_compat::scalar_key)
        .collect::<BTreeSet<_>>();
    for h in hyperedges {
        if let Some(id) = h.get("id").filter(|v| truth(v)) {
            let key = crate::python_compat::scalar_key(id).ok_or("Unhashable hyperedge ID")?;
            if seen.insert(key) {
                items.push(h.clone())
            }
        }
    }
    graph.attributes["hyperedges"] = existing;
    Ok(())
}
pub fn git_head(cwd: Option<&Path>) -> Option<String> {
    let mut command = Command::new("git");
    command.args(["rev-parse", "HEAD"]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let out = process::capture(&mut command, Duration::from_secs(3)).ok()?;
    out.success
        .then(|| String::from_utf8_lossy(&out.stdout).trim().into())
}
#[derive(Debug, PartialEq, Eq)]
pub enum ExistingGraph {
    Nothing,
    Count(usize),
    Malformed,
}
pub fn existing_graph_node_count(path: &Path) -> ExistingGraph {
    if !path.exists() || security::check_graph_file_size_cap(path).is_err() {
        return ExistingGraph::Nothing;
    }
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            return if fs::metadata(path).is_ok_and(|m| m.len() > 0) {
                ExistingGraph::Malformed
            } else {
                ExistingGraph::Nothing
            };
        }
    };
    if raw
        .trim_matches(crate::dedup_strings::whitespace)
        .is_empty()
    {
        return ExistingGraph::Nothing;
    }
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("nodes").and_then(Value::as_array).map(Vec::len))
        .map(ExistingGraph::Count)
        .unwrap_or(ExistingGraph::Malformed)
}
pub fn backup_if_protected(out: &Path, d: &mut Diagnostics) -> Option<PathBuf> {
    backup_if_protected_at(out, &chrono::Local::now().format("%Y-%m-%d").to_string(), d)
}
pub fn backup_if_protected_at(out: &Path, today: &str, d: &mut Diagnostics) -> Option<PathBuf> {
    if std::env::var_os("GRAPHICLE_NO_BACKUP").is_some_and(|s| !s.is_empty())
        || !out.join("graph.json").exists()
    {
        return None;
    }
    let semantic = out.join(".graphicle_semantic_marker").exists();
    let curated = fs::read_to_string(out.join(".graphicle_labels.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.as_object()
                .map(|m| m.iter().any(|(k, v)| *v != json!(format!("Community {k}"))))
        })
        .unwrap_or(false);
    if !semantic && !curated {
        return None;
    }
    let backup = out.join(today);
    if let (Ok(a), Ok(b)) = (
        fs::read(out.join("graph.json")),
        fs::read(backup.join("graph.json")),
    ) && a == b
    {
        return Some(backup);
    }
    if let Err(e) = fs::create_dir_all(&backup) {
        d.stderr.push(format!(
            "[graphicle] warning: backup failed ({e}) - continuing with overwrite"
        ));
        return None;
    }
    let mut copied = 0;
    for name in BACKUP_ARTIFACTS {
        let src = out.join(name);
        let dst = backup.join(name);
        if src.exists() && fs::copy(&src, &dst).is_ok() {
            if let (Ok(metadata), Ok(file)) = (
                fs::metadata(&src),
                fs::File::options().write(true).open(&dst),
            ) {
                let mut times = fs::FileTimes::new();
                if let Ok(t) = metadata.accessed() {
                    times = times.set_accessed(t)
                }
                if let Ok(t) = metadata.modified() {
                    times = times.set_modified(t)
                }
                let _ = file.set_times(times);
            }
            copied += 1;
        }
    }
    if copied > 0 {
        let reason = match (semantic, curated) {
            (true, true) => "semantic+curated",
            (true, false) => "semantic",
            _ => "curated",
        };
        d.stdout.push(format!(
            "[graphicle] backed up {reason} graph ({copied} files) -> {today}/"
        ))
    }
    Some(backup)
}
fn canonical(item: &Value, lead: &[&str]) -> Result<Value> {
    let item = item.as_object().ok_or("Expected graph item")?;
    let mut out = Map::new();
    for key in lead {
        if let Some(v) = item.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    let mut keys = item
        .keys()
        .filter(|k| !lead.contains(&k.as_str()))
        .collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        out.insert(key.clone(), item[key].clone());
    }
    Ok(Value::Object(out))
}
pub(crate) fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(v) => {
            let mut keys = v.keys().collect::<Vec<_>>();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted_value(&v[k])))
                    .collect(),
            )
        }
        Value::Array(v) => Value::Array(v.iter().map(sorted_value).collect()),
        _ => value.clone(),
    }
}
/// The key graph.json records sort by: compact JSON with keys sorted at every
/// level, built in one buffer. As bytes: UTF-8 byte order is code point order,
/// so byte keys sort exactly as the strings would.
fn sort_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    sort_key_into(v, &mut out);
    out
}
fn sort_key_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Object(v) => {
            let mut keys = v.keys().collect::<Vec<_>>();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                crate::python_json::write(&mut *out, k, None, false)
                    .expect("writing JSON to memory");
                out.push(b':');
                sort_key_into(&v[k], out);
            }
            out.push(b'}');
        }
        Value::Array(v) => {
            out.push(b'[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                sort_key_into(x, out);
            }
            out.push(b']');
        }
        _ => crate::python_json::write(&mut *out, v, None, false).expect("writing JSON to memory"),
    }
}
#[derive(Default)]
pub struct JsonOptions {
    pub force: bool,
    pub built_at_commit: Option<String>,
    pub community_labels: BTreeMap<i64, Value>,
    /// The reloaded file's own links, written back verbatim (endpoints still
    /// in the graph) instead of re-deriving them from a simple graph that
    /// keeps one edge per node pair (cluster-only/label, Graphify 7527e73).
    pub original_links: Option<Vec<Value>>,
}
/// One top-level field of an existing graph file, the others skipped
/// unparsed: `None` when the document is not a JSON object (unreadable),
/// `Some(None)` when the field is absent. A repeated key keeps its last
/// value, as the dict does.
fn document_field(raw: &str, name: &str) -> Option<Option<Value>> {
    struct Field<'n>(&'n str);
    impl<'de> serde::de::DeserializeSeed<'de> for Field<'_> {
        type Value = Option<Value>;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Field<'_> {
        type Value = Option<Value>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a graph document")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut found = None;
            while let Some(key) = map.next_key::<String>()? {
                if key == self.0 {
                    found = Some(map.next_value::<Value>()?);
                } else {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(found)
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw);
    let value = serde::de::DeserializeSeed::deserialize(Field(name), &mut d).ok()?;
    d.end().ok()?;
    Some(value)
}
/// The `nodes` list of an existing graph file: `None` when the document is
/// unreadable, an empty list when `nodes` is absent or not a list (Graphify
/// 0.9.87; the count used to take any sized value's length).
fn existing_nodes(raw: &str) -> Option<Vec<Value>> {
    document_field(raw, "nodes").map(|nodes| match nodes {
        Some(Value::Array(nodes)) => nodes,
        _ => vec![],
    })
}
/// The `hyperedges` field of an existing graph file, parsed alone.
fn existing_hyperedges(raw: &str) -> Option<Value> {
    document_field(raw, "hyperedges").flatten()
}
pub fn to_json(
    graph: &Graph,
    communities: &Communities,
    output: &Path,
    options: &JsonOptions,
    d: &mut Diagnostics,
) -> Result<bool> {
    if !options.force && output.exists() && security::check_graph_file_size_cap(output).is_ok() {
        let raw = fs::read_to_string(output).unwrap_or_default();
        let existing = if raw
            .trim_matches(crate::dedup_strings::whitespace)
            .is_empty()
        {
            Some(vec![])
        } else {
            existing_nodes(&raw)
        };
        let Some(existing) = existing else {
            d.stderr.push(format!("[graphicle] WARNING: existing {} could not be read to verify the new graph is not smaller (malformed graph JSON). Refusing to overwrite; pass force=True to override.",output.display()));
            return Ok(false);
        };
        // A saved semantic node (rationale, document) the new graph lacks is
        // lost even when AST growth keeps the total up (#4115/#2229).
        let missing =
            crate::build::lost_saved_semantic_nodes(&existing, &semantic_check_nodes(graph), &[]);
        if !missing.is_empty() {
            let ids = missing
                .iter()
                .map(|node| crate::python_compat::string(node.get("id").unwrap_or(&Value::Null)))
                .collect::<Vec<_>>()
                .join(", ");
            d.stderr.push(format!("[graphicle] WARNING: Refusing to overwrite — saved semantic node(s) are missing from the new graph: {ids}."));
            return Ok(false);
        }
        let (n, old) = (graph.nodes.len(), existing.len());
        if n < old {
            d.stderr.push(format!("[graphicle] WARNING: new graph has {n} nodes but existing graph.json has {old} (net -{}). Refusing to overwrite. Possible causes: missing chunk files from a previous session, or fuzzy dedup collapsed same-named symbols across files during an --update on an already-current graph. Run a full rebuild (/graphicle .) to be safe, or pass force=True only if you have verified the reduction is legitimate.",old-n));
            return Ok(false);
        }
    }
    let assign = assignments(graph, communities);
    let edges = graph.iter_edges().collect::<Vec<_>>();
    // Clustering only ever changes node attributes, so a reload-and-recluster
    // writes back every original link, parallel ones included (#3999).
    let originals = options.original_links.as_ref().map(|links| {
        let ids = graph
            .nodes
            .iter()
            .filter_map(|n| crate::python_compat::scalar_key(&n.id))
            .collect::<std::collections::BTreeSet<_>>();
        let member = |link: &Value, k: &str| {
            crate::python_compat::scalar_key(link.get(k).unwrap_or(&Value::Null))
                .is_some_and(|key| ids.contains(&key))
        };
        links
            .iter()
            .filter(|l| l.is_object() && member(l, "source") && member(l, "target"))
            .collect::<Vec<_>>()
    });
    let edge_count = originals.as_ref().map_or(edges.len(), Vec::len);
    // Each record is built exactly as `to_node_link` plus the fields below
    // would build it, but only while it is being keyed or written, so the
    // document is never held as one Value tree next to the graph.
    let node_value = |i: usize| -> Result<Value> {
        let mut node = graph.node_link_node(i);
        node["community"] = json!(assign[i]);
        if assign[i].is_some() && !options.community_labels.is_empty() {
            node["community_name"] = community_name(assign[i], &options.community_labels)
        }
        node["norm_label"] =
            json!(strip_diacritics(node.get("label").unwrap_or(&json!(""))).to_lowercase());
        canonical(&node, &["id", "label"])
    };
    let edge_value = |k: usize| -> Result<Value> {
        let mut edge = match &originals {
            Some(links) => links[k].clone(),
            None => {
                let (u, v, e) = edges[k];
                graph.node_link_edge(u, v, e)
            }
        };
        if edge.get("confidence_score").is_none() {
            let confidence = edge
                .get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("EXTRACTED");
            edge["confidence_score"] = json!(match confidence {
                "INFERRED" => 0.55,
                "AMBIGUOUS" => 0.2,
                _ => 1.0,
            })
        }
        let e = edge.as_object_mut().unwrap();
        let source = e.shift_remove("_src");
        let target = e.shift_remove("_tgt");
        if let (Some(source), Some(target)) = (
            source.filter(|v| !v.is_null()),
            target.filter(|v| !v.is_null()),
        ) {
            e.insert("source".into(), source);
            e.insert("target".into(), target);
        }
        canonical(&edge, &["source", "target", "relation"])
    };
    let profiled = crate::profile::span("export.json.sort_keys");
    let node_order = sorted_by_key(graph.nodes.len(), &node_value)?;
    let edge_order = sorted_by_key(edge_count, &edge_value)?;
    drop(profiled);
    if graph.attributes.get("hyperedges").is_none()
        && security::check_graph_file_size_cap(output).is_ok()
        && let Some(old) = fs::read_to_string(output)
            .ok()
            .and_then(|s| existing_hyperedges(&s))
            .filter(truth)
    {
        let count = old.as_array().map(Vec::len).unwrap_or(0);
        d.stderr.push(format!("[graphicle] WARNING: graph carries no hyperedge metadata but {} already holds {count} hyperedge(s); writing an empty set. Rebuild from the original extraction if this is unexpected.",output.display()));
    }
    let mut hyperedges = graph
        .attributes
        .get("hyperedges")
        .cloned()
        .unwrap_or(json!([]));
    hyperedges
        .as_array_mut()
        .ok_or("Expected hyperedges array")?
        .sort_by_cached_key(sort_key);
    let mut graph_attributes = graph.attributes.clone();
    // The #3774 shrink accounting is never persisted, whoever writes.
    if let Some(attributes) = graph_attributes.as_object_mut() {
        attributes.shift_remove(crate::build::DEDUP_COLLAPSED);
        attributes.shift_remove(crate::build::PRUNED_NODE_COUNT);
    }
    if graph_attributes.get("hyperedges").is_some() {
        graph_attributes["hyperedges"] = hyperedges.clone()
    };
    // Graph provenance (#4167): the persisted structure's version and the
    // release that wrote it.
    if !graph_attributes.is_object() {
        graph_attributes = json!({});
    }
    graph_attributes["schema_version"] = json!(GRAPH_SCHEMA_VERSION);
    if let Some(attributes) = graph_attributes.as_object_mut() {
        stamp_writer_version(attributes);
    }
    let commit = options.built_at_commit.clone().or_else(|| {
        fsutil::resolve_missing(output)
            .ok()
            .and_then(|p| git_head(p.parent()))
    });
    let document = NodeLinkDocument {
        directed: graph.directed,
        multigraph: graph.multigraph,
        graph: &graph_attributes,
        nodes: Records {
            order: &node_order,
            value: &node_value,
        },
        links: Records {
            order: &edge_order,
            value: &edge_value,
        },
        hyperedges: &hyperedges,
        built_at_commit: commit.filter(|c| !c.is_empty()),
    };
    let workers = crate::par::workers(None);
    let _profiled = crate::profile::span("export.json.write");
    paths::atomic_replace_with(
        output,
        |f| {
            let mut buffered = std::io::BufWriter::with_capacity(1 << 20, f);
            document
                .write_to(&mut buffered, workers)
                .map_err(std::io::Error::other)?;
            std::io::Write::flush(&mut buffered)
        },
        &paths::SystemReplace,
    )
    .map_err(crate::Error::from)?;
    Ok(true)
}
/// Indices `0..n` in the order a stable sort of their records by `sort_key`
/// puts them, keying records on all cores without keeping them.
///
/// The keys are packed into [`KeyArena`] blocks rather than held one `Vec`
/// per record: a key costs its bytes plus a 16-byte row, with no per-record
/// header, allocator rounding or doubling slack. Sorting the rows by (key
/// bytes, index) is the stable sort by key bytes, so the order is exactly the
/// one [`sorted_by_key_oracle`] gives.
fn sorted_by_key<F>(n: usize, value: &F) -> Result<Vec<usize>>
where
    F: Fn(usize) -> Result<Value> + Sync,
{
    sorted_by_key_in(n, value, key_workers(n), KEY_BLOCK)
}
fn key_workers(n: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(n.div_ceil(2048).max(1))
}
/// Bytes per arena block: big enough that a block's unused tail is noise
/// next to the keys it holds, small enough that a worker's last, partly
/// filled block is too.
const KEY_BLOCK: usize = 1 << 20;
/// One record's key: `len` bytes at `offset` in arena block `block`.
#[derive(Clone, Copy, Default)]
struct KeyRow {
    block: u32,
    offset: u32,
    len: u32,
    index: u32,
}
/// Sort keys packed back to back in blocks that are allocated at their final
/// size and never grow; a key longer than a block gets a block of its own.
#[derive(Default)]
struct KeyArena {
    blocks: Vec<Vec<u8>>,
}
impl KeyArena {
    /// Append `key` as record `index`'s row; `None` when the key, its block
    /// or the index does not fit the 32-bit row fields.
    fn push(&mut self, key: &[u8], index: usize, block_size: usize) -> Option<KeyRow> {
        if self
            .blocks
            .last()
            .is_none_or(|b| b.capacity() - b.len() < key.len())
        {
            self.blocks
                .push(Vec::with_capacity(block_size.max(key.len())));
        }
        let block = self.blocks.len() - 1;
        let last = &mut self.blocks[block];
        let offset = last.len();
        last.extend_from_slice(key);
        Some(KeyRow {
            block: u32::try_from(block).ok()?,
            offset: u32::try_from(offset).ok()?,
            len: u32::try_from(key.len()).ok()?,
            index: u32::try_from(index).ok()?,
        })
    }
    fn key(&self, row: &KeyRow) -> &[u8] {
        let start = row.offset as usize;
        &self.blocks[row.block as usize][start..start + row.len as usize]
    }
}
/// [`sorted_by_key`] with the worker count and block size given.
fn sorted_by_key_in<F>(n: usize, value: &F, workers: usize, block: usize) -> Result<Vec<usize>>
where
    F: Fn(usize) -> Result<Value> + Sync,
{
    match arena_order(n, value, workers, block)? {
        Some(order) => Ok(order),
        None => sorted_by_key_oracle(n, value),
    }
}
/// The arena sort; `None` when a row field would overflow 32 bits (the
/// arena is freed before the caller falls back).
fn arena_order<F>(n: usize, value: &F, workers: usize, block: usize) -> Result<Option<Vec<usize>>>
where
    F: Fn(usize) -> Result<Value> + Sync,
{
    if u32::try_from(n).is_err() {
        return Ok(None);
    }
    let chunk = n.div_ceil(workers.max(1)).max(1);
    // Each worker fills its own slice of the one row table.
    let mut rows = vec![KeyRow::default(); n];
    let arenas = std::thread::scope(|scope| {
        rows.chunks_mut(chunk)
            .enumerate()
            .map(|(c, slots)| {
                scope.spawn(move || -> Result<Option<KeyArena>> {
                    let mut arena = KeyArena::default();
                    let mut key = Vec::new();
                    for (j, slot) in slots.iter_mut().enumerate() {
                        let i = c * chunk + j;
                        key.clear();
                        sort_key_into(&value(i)?, &mut key);
                        match arena.push(&key, i, block) {
                            Some(row) => *slot = row,
                            None => return Ok(None),
                        }
                    }
                    Ok(Some(arena))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("export key worker panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    // One arena: each chunk's blocks follow the earlier chunks', and its rows
    // are renumbered to match.
    let mut keys = KeyArena::default();
    for (arena, slots) in arenas.into_iter().zip(rows.chunks_mut(chunk)) {
        let Some(arena) = arena else {
            return Ok(None);
        };
        let Ok(base) = u32::try_from(keys.blocks.len()) else {
            return Ok(None);
        };
        keys.blocks.extend(arena.blocks);
        if u32::try_from(keys.blocks.len()).is_err() {
            return Ok(None);
        }
        for row in slots {
            row.block += base;
        }
    }
    rows.sort_unstable_by(|a, b| keys.key(a).cmp(keys.key(b)).then(a.index.cmp(&b.index)));
    drop(keys);
    Ok(Some(
        rows.into_iter().map(|row| row.index as usize).collect(),
    ))
}
/// The per-record path: every key its own `Vec`, then one stable sort. The
/// arena's oracle in tests, and its fallback past the 32-bit row fields.
fn sorted_by_key_oracle<F>(n: usize, value: &F) -> Result<Vec<usize>>
where
    F: Fn(usize) -> Result<Value> + Sync,
{
    let workers = key_workers(n);
    let chunk = n.div_ceil(workers).max(1);
    let keyed = std::thread::scope(|scope| {
        (0..n)
            .step_by(chunk)
            .map(|start| {
                scope.spawn(move || {
                    (start..(start + chunk).min(n))
                        .map(|i| value(i).map(|v| (sort_key(&v), i)))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("export key worker panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    let mut keyed = keyed.into_iter().flatten().collect::<Vec<_>>();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(keyed.into_iter().map(|(_, i)| i).collect())
}
/// Records serialized in `order`, each built only while it is written.
struct Records<'a, F: Fn(usize) -> Result<Value>> {
    order: &'a [usize],
    value: &'a F,
}
#[cfg(test)]
impl<F: Fn(usize) -> Result<Value>> serde::Serialize for Records<'_, F> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeSeq};
        let mut seq = s.serialize_seq(Some(self.order.len()))?;
        for &i in self.order {
            seq.serialize_element(&(self.value)(i).map_err(S::Error::custom)?)?;
        }
        seq.end()
    }
}
impl<F: Fn(usize) -> Result<Value> + Sync> Records<'_, F> {
    /// The records as a compact JSON array, rendered on `workers` threads a
    /// chunk at a time, so the text is what one serializing pass writes. A
    /// wave of chunks is rendered, written, then dropped: only a wave is ever
    /// held as text.
    fn write_to<W: std::io::Write>(&self, w: &mut W, workers: usize) -> Result<()> {
        w.write_all(b"[")?;
        const CHUNK: usize = 256;
        let chunks = self.order.chunks(CHUNK).collect::<Vec<_>>();
        let mut first = true;
        for wave in chunks.chunks(workers.max(1) * 4) {
            let rendered = crate::par::map(wave, workers, |chunk| -> Result<Vec<u8>> {
                let mut text = Vec::with_capacity(chunk.len() * 320);
                for &i in *chunk {
                    text.push(b',');
                    write_compact(&mut text, &(self.value)(i)?)?;
                }
                Ok(text)
            });
            for text in rendered {
                let text = text?;
                // Every record is preceded by its separator but the first.
                w.write_all(if first { &text[1..] } else { &text })?;
                first = false;
            }
        }
        w.write_all(b"]")?;
        Ok(())
    }
}
/// `json.dump(..., separators=(",", ":"))` with `ensure_ascii`, the layout
/// graph.json is written in since Graphify 0.9.86 (#3413).
fn write_compact<W: std::io::Write, T: serde::Serialize + ?Sized>(w: W, value: &T) -> Result<()> {
    crate::python_json::write_compact(w, &value, true).map_err(crate::Error::from)
}
/// The graph.json document in `to_node_link` key order plus the export fields.
struct NodeLinkDocument<'a, N, E>
where
    N: Fn(usize) -> Result<Value>,
    E: Fn(usize) -> Result<Value>,
{
    directed: bool,
    multigraph: bool,
    graph: &'a Value,
    nodes: Records<'a, N>,
    links: Records<'a, E>,
    hyperedges: &'a Value,
    built_at_commit: Option<String>,
}
impl<N, E> NodeLinkDocument<'_, N, E>
where
    N: Fn(usize) -> Result<Value> + Sync,
    E: Fn(usize) -> Result<Value> + Sync,
{
    /// The document as the compact `json.dump` lays it out on one line (the
    /// bytes of the `Serialize` form below through [`write_compact`]), with
    /// the records rendered in parallel.
    fn write_to<W: std::io::Write>(&self, w: &mut W, workers: usize) -> Result<()> {
        write!(
            w,
            "{{\"directed\":{},\"multigraph\":{},\"graph\":",
            self.directed, self.multigraph
        )?;
        write_compact(&mut *w, self.graph)?;
        w.write_all(b",\"nodes\":")?;
        self.nodes.write_to(w, workers)?;
        w.write_all(b",\"links\":")?;
        self.links.write_to(w, workers)?;
        w.write_all(b",\"hyperedges\":")?;
        write_compact(&mut *w, self.hyperedges)?;
        if let Some(commit) = &self.built_at_commit {
            w.write_all(b",\"built_at_commit\":")?;
            write_compact(&mut *w, commit)?;
        }
        w.write_all(b"}")?;
        Ok(())
    }
}
#[cfg(test)]
impl<N, E> serde::Serialize for NodeLinkDocument<'_, N, E>
where
    N: Fn(usize) -> Result<Value>,
    E: Fn(usize) -> Result<Value>,
{
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("directed", &self.directed)?;
        map.serialize_entry("multigraph", &self.multigraph)?;
        map.serialize_entry("graph", self.graph)?;
        map.serialize_entry("nodes", &self.nodes)?;
        map.serialize_entry("links", &self.links)?;
        map.serialize_entry("hyperedges", self.hyperedges)?;
        if let Some(commit) = &self.built_at_commit {
            map.serialize_entry("built_at_commit", commit)?;
        }
        map.end()
    }
}
pub fn prune_dangling_edges(data: &mut Value) -> Result<usize> {
    let nodes = data["nodes"].as_array().ok_or("Expected nodes array")?;
    let ids = nodes
        .iter()
        .map(|n| crate::python_compat::scalar_key(&n["id"]).ok_or("Unhashable node ID"))
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    let key = if data.get("links").is_some() {
        "links"
    } else {
        "edges"
    };
    let edges = data[key].as_array_mut().ok_or("Expected graph edges")?;
    let before = edges.len();
    edges.retain(|e| {
        ["source", "target"]
            .iter()
            .all(|k| crate::python_compat::scalar_key(&e[*k]).is_some_and(|id| ids.contains(&id)))
    });
    Ok(before - edges.len())
}
pub fn to_cypher(graph: &Graph, output: &Path) -> Result<()> {
    let mut lines = vec![
        "// Neo4j Cypher import - generated by Graphicle".into(),
        String::new(),
    ];
    for n in &graph.nodes {
        let id = n.id.as_str().ok_or("Cypher node ID must be a string")?;
        let label = n
            .attributes
            .get("label")
            .unwrap_or(&n.id)
            .as_str()
            .ok_or("Cypher label must be a string")?;
        let ftype = n
            .attributes
            .get("file_type")
            .filter(|v| truth(v))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let ftype = cypher_label(&crate::exporters::graphdb::capitalize(ftype), "Entity");
        lines.push(format!(
            "MERGE (n:{ftype} {{id: '{}', label: '{}'}});",
            cypher_escape(id),
            cypher_escape(label)
        ));
    }
    lines.push(String::new());
    for (u, v, e) in graph.iter_edges() {
        let raw = e
            .attributes
            .get("relation")
            .filter(|v| truth(v))
            .and_then(Value::as_str)
            .unwrap_or("RELATES_TO");
        let rel = cypher_label(&raw.to_uppercase(), "RELATES_TO");
        let conf = e
            .attributes
            .get("confidence")
            .map(|v| v.as_str().ok_or("confidence must be a string"))
            .transpose()?
            .unwrap_or("EXTRACTED");
        let u = e
            .attributes
            .get("_src")
            .unwrap_or(&graph.nodes[u].id)
            .as_str()
            .ok_or("Cypher node ID must be a string")?;
        let v = e
            .attributes
            .get("_tgt")
            .unwrap_or(&graph.nodes[v].id)
            .as_str()
            .ok_or("Cypher node ID must be a string")?;
        lines.push(format!("MATCH (a {{id: '{}'}}), (b {{id: '{}'}}) MERGE (a)-[:{rel} {{confidence: '{}'}}]->(b);",cypher_escape(u),cypher_escape(v),cypher_escape(conf)));
    }
    fs::write(output, lines.join("\n")).map_err(crate::Error::from)
}

#[cfg(test)]
mod existing_graph_tests {
    use super::{existing_hyperedges, existing_nodes};
    use serde_json::Value;

    /// The full-parse reading these helpers replace: the document must be an
    /// object; `nodes` counts only when it is a list (Graphify 0.9.87).
    fn nodes_by_value(raw: &str) -> Option<Vec<Value>> {
        serde_json::from_str::<Value>(raw).ok().and_then(|v| {
            v.as_object().map(|m| match m.get("nodes") {
                Some(Value::Array(nodes)) => nodes.clone(),
                _ => vec![],
            })
        })
    }
    fn hyperedges_by_value(raw: &str) -> Option<Value> {
        serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v.get("hyperedges").cloned())
            .filter(|v| !v.is_null())
    }

    #[test]
    fn field_reads_match_full_parses() {
        for raw in [
            r#"{"nodes": [{"id": 1}, {"id": 2, "x": [1, 2]}], "links": []}"#,
            r#"{"links": []}"#,
            r#"{"nodes": null}"#,
            r#"{"nodes": 3}"#,
            r#"{"nodes": 3.5}"#,
            r#"{"nodes": true}"#,
            r#"{"nodes": "héllo"}"#,
            r#"{"nodes": {"a": 1, "b": 2, "a": 3}}"#,
            r#"{"nodes": [], "hyperedges": [{"id": "h"}]}"#,
            r#"{"nodes": [], "hyperedges": null}"#,
            r#"{"nodes": [], "hyperedges": []}"#,
            r#"{"nodes": [{"id": "a"}], "nodes": [{"id": "b"}]}"#,
            r#"[1, 2]"#,
            r#"[{"id": 1}]"#,
            r#""text""#,
            r#"7"#,
            r#"{"nodes": [1, 2"#,
            "not json",
        ] {
            assert_eq!(existing_nodes(raw), nodes_by_value(raw), "{raw}");
            assert_eq!(
                existing_hyperedges(raw).filter(|v| !v.is_null()),
                hyperedges_by_value(raw),
                "{raw}"
            );
        }
    }
}

#[cfg(test)]
mod document_tests {
    use super::{NodeLinkDocument, Records, sort_key};
    use serde_json::{Value, json};

    /// The compact sorted-keys text `sort_key` produced as a string.
    fn reference_key(v: &Value) -> String {
        match v {
            Value::Object(v) => {
                let mut keys = v.keys().collect::<Vec<_>>();
                keys.sort();
                format!(
                    "{{{}}}",
                    keys.into_iter()
                        .map(|k| format!(
                            "{}:{}",
                            super::json_text(&json!(k), None, false).unwrap(),
                            reference_key(&v[k])
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            Value::Array(v) => format!(
                "[{}]",
                v.iter().map(reference_key).collect::<Vec<_>>().join(",")
            ),
            _ => super::json_text(v, None, false).unwrap(),
        }
    }

    fn records(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({"id": format!("n{i}"), "label": format!("caf\u{e9} {i}"),
                       "tags": [i, {"z": null, "a": [true, 1.5]}], "empty": {},
                       "text": "a, b: \"c, d\" \\, e: [] {}"})
            })
            .collect()
    }

    #[test]
    fn sort_keys_match_the_string_keys() {
        for v in records(3).into_iter().chain([
            json!("x\"\u{1f600}"),
            json!([]),
            json!({"b": 1, "a": {"d": [], "c": 2}}),
        ]) {
            assert_eq!(String::from_utf8(sort_key(&v)).unwrap(), reference_key(&v));
        }
    }

    /// The compact layout keeps every separator inside strings and drops
    /// the space after each one outside them.
    #[test]
    fn compact_text_matches_the_python_separators() {
        let value = json!({"a, b": [1, 2.5, "x: y, z", {"q": "\\\", "}, null, []], "c": {}});
        let mut text = vec![];
        super::write_compact(&mut text, &value).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            r#"{"a, b":[1,2.5,"x: y, z",{"q":"\\\", "},null,[]],"c":{}}"#
        );
    }

    /// A small deterministic generator (xorshift64*), so the fuzz cases are
    /// the same on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())]
        }
    }

    /// Text with shared prefixes, JSON escapes, control, non-ASCII and
    /// astral characters.
    fn fuzz_text(rng: &mut Rng) -> String {
        const PREFIXES: [&str; 5] = ["", "pkg/api/", "pkg/api/v1.", "k8s.io/", "a"];
        const PIECES: [&str; 16] = [
            "a",
            "b",
            "z",
            "\"",
            "\\",
            "/",
            "\n",
            "\t",
            "\u{1}",
            "\u{1f}",
            "\u{7f}",
            "\u{e9}",
            "\u{2028}",
            "\u{1f600}",
            "\u{10ffff}",
            ",",
        ];
        let mut text = rng.pick(&PREFIXES).to_owned();
        for _ in 0..rng.below(6) {
            text.push_str(rng.pick(&PIECES));
        }
        text
    }

    /// A scalar, including numbers whose texts share prefixes (`1.0`,
    /// `1.05`) and are kept exactly as written.
    fn fuzz_scalar(rng: &mut Rng) -> Value {
        const NUMBERS: [&str; 12] = [
            "1",
            "1.0",
            "1.05",
            "10",
            "-1",
            "0",
            "0.55",
            "0.2",
            "1e5",
            "1.0e5",
            "-0.0",
            "12345678901234567890123",
        ];
        match rng.below(6) {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 1),
            2 => serde_json::from_str(rng.pick(&NUMBERS)).unwrap(),
            _ => Value::String(fuzz_text(rng)),
        }
    }

    fn fuzz_value(rng: &mut Rng, depth: usize) -> Value {
        match if depth == 0 { 0 } else { rng.below(5) } {
            3 => Value::Array(
                (0..rng.below(4))
                    .map(|_| fuzz_value(rng, depth - 1))
                    .collect(),
            ),
            4 => Value::Object(
                (0..rng.below(4))
                    .map(|_| (fuzz_text(rng), fuzz_value(rng, depth - 1)))
                    .collect(),
            ),
            _ => fuzz_scalar(rng),
        }
    }

    /// Edge- and node-like records with nested metadata; some repeat an
    /// earlier record, so equal keys must keep their index order.
    fn fuzz_records(rng: &mut Rng, n: usize) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::with_capacity(n);
        for _ in 0..n {
            if !out.is_empty() && rng.below(8) == 0 {
                let again = out[rng.below(out.len())].clone();
                out.push(again);
                continue;
            }
            let mut record = serde_json::Map::new();
            for key in ["source", "target", "relation", "id", "label"] {
                if rng.below(3) > 0 {
                    record.insert(key.into(), fuzz_scalar(rng));
                }
            }
            for _ in 0..rng.below(3) {
                record.insert(fuzz_text(rng), fuzz_value(rng, 3));
            }
            out.push(if rng.below(20) == 0 {
                fuzz_value(rng, 2)
            } else {
                Value::Object(record)
            });
        }
        out
    }

    /// The arena orders records exactly as the per-record keys did, whatever
    /// the chunking across workers and wherever block boundaries fall.
    #[test]
    fn arena_order_matches_the_per_record_oracle() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for n in [0, 1, 2, 3, 17, 256, 1000, 5000] {
            let records = fuzz_records(&mut rng, n);
            let value = |i: usize| Ok(records[i].clone());
            let oracle = super::sorted_by_key_oracle(n, &value).unwrap();
            let keys = records.iter().map(super::sort_key).collect::<Vec<_>>();
            assert!(oracle.windows(2).all(|w| {
                let (a, b) = (&keys[w[0]], &keys[w[1]]);
                a < b || (a == b && w[0] < w[1])
            }));
            assert_eq!(super::sorted_by_key(n, &value).unwrap(), oracle, "n={n}");
            for workers in [1, 2, 3, 8, 64] {
                for block in [1, 5, 64, 4096, super::KEY_BLOCK] {
                    assert_eq!(
                        super::sorted_by_key_in(n, &value, workers, block).unwrap(),
                        oracle,
                        "n={n} workers={workers} block={block}"
                    );
                }
            }
        }
    }

    /// A record that fails to build fails the sort, as it did per record.
    #[test]
    fn arena_order_reports_record_errors() {
        let value = |i: usize| {
            if i == 4100 {
                Err(crate::Error::from("bad record"))
            } else {
                Ok(json!({"id": i}))
            }
        };
        for workers in [1, 3] {
            assert!(super::sorted_by_key_in(5000, &value, workers, 64).is_err());
        }
        assert!(super::sorted_by_key_oracle(5000, &value).is_err());
    }

    #[test]
    fn parallel_document_matches_one_serializing_pass() {
        for (nodes, links, commit) in [
            (0, 0, None),
            (1, 0, Some("abc")),
            (700, 1300, Some("d\u{e9}f")),
        ] {
            let nodes = records(nodes);
            let links = records(links);
            let order = |n: usize| (0..n).rev().collect::<Vec<_>>();
            let (node_order, link_order) = (order(nodes.len()), order(links.len()));
            let node_value = |i: usize| Ok(nodes[i].clone());
            let link_value = |i: usize| Ok(links[i].clone());
            let graph = json!({"name": "g", "hyperedges": []});
            let hyperedges = json!([{"id": "h", "nodes": ["n1"]}]);
            let document = NodeLinkDocument {
                directed: false,
                multigraph: true,
                graph: &graph,
                nodes: Records {
                    order: &node_order,
                    value: &node_value,
                },
                links: Records {
                    order: &link_order,
                    value: &link_value,
                },
                hyperedges: &hyperedges,
                built_at_commit: commit.map(str::to_string),
            };
            let mut serial = vec![];
            super::write_compact(&mut serial, &document).unwrap();
            for workers in [1, 3, 16] {
                let mut parallel = vec![];
                document.write_to(&mut parallel, workers).unwrap();
                assert_eq!(
                    String::from_utf8(parallel).unwrap(),
                    String::from_utf8(serial.clone()).unwrap()
                );
            }
        }
    }
}
