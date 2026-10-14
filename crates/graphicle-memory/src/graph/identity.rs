//! Durable identity and exact provenance for a built graph (`identity.annotate`).
//!
//! Native-only capability: the pinned Python derives node identifiers from
//! labels (`ids.make_id`), so two symbols with one label collapse and a rename
//! looks like a delete plus an add. This pass leaves those identifiers untouched
//! and adds, per node, a logical `stable_id` (repository, path, qualified name,
//! kind, ordinal), a content `revision` (hash of the symbol's own source lines)
//! and the `qualified_name` it was derived from; per edge it adds `derivation`
//! (from the confidence label) and a `provenance` record. The graph shape is
//! unchanged, so every parity oracle still holds on the annotated graph.
//!
//! The Python oracle (`tools/check_identity.py`) recomputes the same values
//! from the node-link graph and the source files.
use crate::graph::{
    Error, Result,
    graph::Graph,
    python_compat::string,
    session::{self, GraphHandle},
    span::{self, start_line},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

/// Relations along which a symbol is nested inside another symbol.
pub const NESTING_RELATIONS: &[&str] = &["contains", "method"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityOptions {
    /// Repository tag mixed into every `stable_id`; empty means unscoped.
    pub repo: String,
    /// Directory the graph's `source_file` values are relative to. Source
    /// bytes for revisions are read below it unless `sources` provides them.
    pub root: Option<PathBuf>,
    /// Prefix stripped from `source_file` before it becomes `relpath`.
    pub strip_prefix: Option<String>,
    /// In-memory sources keyed by `relpath`; take precedence over `root`.
    pub sources: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySummary {
    pub nodes: usize,
    pub edges: usize,
    pub with_revision: usize,
    /// Nodes whose `(relpath, qualified_name, kind)` collided and needed an ordinal.
    pub disambiguated: usize,
    /// Relative paths referenced by nodes but not readable from `sources` or `root`.
    pub missing_sources: Vec<String>,
}

/// The logical identity a `stable_id` hashes: the
/// node's `relpath`, `qualified_name` and `kind`, and the `ordinal` that tells
/// apart symbols sharing those three. `stable_id` = sha256 of the identity
/// scope ([`IdentityOptions::repo`], an opaque project id in the service)
/// and these four. A link to a symbol keeps this next to
/// the `stable_id`, so it can be re-grounded in a project whose identity scope
/// differs: [`LogicalIdentity::stable_id`] recomputes the hash there. The
/// link tables keep it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogicalIdentity {
    /// Path relative to the repository root; empty for external nodes.
    pub relpath: String,
    pub qualified_name: String,
    pub kind: String,
    pub ordinal: usize,
}

impl LogicalIdentity {
    /// The `stable_id` of this identity in the identity scope `repo`.
    pub fn stable_id(&self, repo: &str) -> String {
        stable_hash(
            repo,
            &self.relpath,
            &self.qualified_name,
            &self.kind,
            self.ordinal,
        )
    }
}

/// The one definition of the `stable_id` hash, for [`annotate`] and
/// [`LogicalIdentity::stable_id`].
fn stable_hash(repo: &str, relpath: &str, qualified: &str, kind: &str, ordinal: usize) -> String {
    sha256_hex(&[
        repo.as_bytes(),
        b"\0",
        relpath.as_bytes(),
        b"\0",
        qualified.as_bytes(),
        b"\0",
        kind.as_bytes(),
        b"\0",
        ordinal.to_string().as_bytes(),
    ])
}

/// The logical identity of every node of a graph [`annotate`] annotated, by
/// node index; `None` for a node it did not annotate (no `qualified_name` or
/// `kind`). The ordinal is assigned as `annotate` assigns it: within each
/// group of equal `(relpath, qualified_name, kind)`, by start line, then by
/// node id.
pub fn logical_identities(graph: &Graph) -> Vec<Option<LogicalIdentity>> {
    fn field(attributes: &impl crate::graph::record::Properties, key: &str) -> Option<String> {
        attributes
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }
    let mut identities: Vec<Option<LogicalIdentity>> = Vec::with_capacity(graph.node_count());
    let mut groups: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
    for (index, node) in graph.nodes().iter().enumerate() {
        let (Some(qualified_name), Some(kind)) = (
            field(&node.attributes, "qualified_name"),
            field(&node.attributes, "kind"),
        ) else {
            identities.push(None);
            continue;
        };
        let relpath = field(&node.attributes, "relpath").unwrap_or_default();
        groups
            .entry((relpath.clone(), qualified_name.clone(), kind.clone()))
            .or_default()
            .push(index);
        identities.push(Some(LogicalIdentity {
            relpath,
            qualified_name,
            kind,
            ordinal: 0,
        }));
    }
    for members in groups.values_mut().filter(|members| members.len() > 1) {
        members.sort_by(|&a, &b| {
            let nodes = graph.nodes();
            start_line(&nodes[a].attributes)
                .unwrap_or(0)
                .cmp(&start_line(&nodes[b].attributes).unwrap_or(0))
                .then_with(|| string(&nodes[a].id).cmp(&string(&nodes[b].id)))
        });
        for (position, &member) in members.iter().enumerate() {
            if let Some(identity) = &mut identities[member] {
                identity.ordinal = position;
            }
        }
    }
    identities
}

fn text(data: &impl crate::graph::record::Properties, key: &str) -> Option<String> {
    data.get(key)
        .filter(|v| !v.is_null())
        .map(string)
        .filter(|s| !s.is_empty())
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn relpath(source_file: &str, strip_prefix: Option<&str>) -> String {
    let normalized = source_file.replace('\\', "/");
    match strip_prefix {
        Some(prefix) if !prefix.is_empty() => {
            let prefix = prefix.replace('\\', "/");
            let prefix = prefix.trim_end_matches('/');
            normalized
                .strip_prefix(prefix)
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .map(|rest| rest.trim_start_matches('/').to_string())
                .unwrap_or(normalized)
        }
        _ => normalized,
    }
}

/// A label as it appears in a qualified name: methods lose the leading dot
/// and the call parentheses, everything else is kept verbatim.
fn name_part(label: &str) -> String {
    let trimmed = label.trim();
    let trimmed = trimmed.strip_prefix('.').unwrap_or(trimmed);
    trimmed.strip_suffix("()").unwrap_or(trimmed).to_string()
}

fn is_file_node(attributes: &impl crate::graph::record::Properties, relpath: &str) -> bool {
    let Some(label) = text(attributes, "label") else {
        return false;
    };
    let basename = relpath.rsplit('/').next().unwrap_or(relpath);
    !basename.is_empty() && (label == basename || label == relpath)
}

/// The content revision: the node's own lines, from its start line up to the
/// line before the next symbol that starts in the same file
/// ([`span::byte_range`]). File nodes hash the whole file. Lines are compared
/// as bytes; a trailing newline is kept.
struct SourceText<'a> {
    text: std::borrow::Cow<'a, str>,
    starts: Vec<usize>,
}

impl<'a> SourceText<'a> {
    fn new(text: std::borrow::Cow<'a, str>) -> Self {
        let starts = span::line_starts(&text);
        Self { text, starts }
    }

    fn revision(&self, start: Option<usize>, next: Option<usize>, file: bool) -> String {
        let source = self.text.as_bytes();
        if file {
            return sha256_hex(&[source]);
        }
        sha256_hex(&[&source[span::byte_range(&self.starts, source.len(), start, next)]])
    }
}

fn derivation(confidence: Option<&str>) -> &'static str {
    match confidence.unwrap_or("") {
        "EXTRACTED" => "syntactic",
        "INFERRED" | "AMBIGUOUS" => "inferred",
        "" => "unknown",
        _ => "semantic",
    }
}

struct Loader<'a> {
    options: &'a IdentityOptions,
    cache: HashMap<String, Option<SourceText<'a>>>,
}

impl Loader<'_> {
    fn source(&mut self, relpath: &str) -> Option<&SourceText<'_>> {
        let options = self.options;
        self.cache
            .entry(relpath.to_owned())
            .or_insert_with(|| {
                let source = match options.sources.get(relpath) {
                    Some(text) => std::borrow::Cow::Borrowed(text.as_str()),
                    None => {
                        let bytes = std::fs::read(options.root.as_ref()?.join(relpath)).ok()?;
                        std::borrow::Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
                    }
                };
                Some(SourceText::new(source))
            })
            .as_ref()
    }
}

/// Annotate `graph` in place and report what was derived.
pub fn annotate(graph: &mut Graph, options: &IdentityOptions) -> Result<IdentitySummary> {
    let count = graph.node_count();
    // Unresolved local imports can become external placeholder labels that
    // contain the normalized absolute checkout path. Preserve the parity id
    // and label, but do not make this machine's directory a durable identity.
    let external_prefix = options
        .strip_prefix
        .as_deref()
        .or_else(|| options.root.as_ref().and_then(|p| p.to_str()))
        .filter(|p| Path::new(p).is_absolute() || p.as_bytes().get(1) == Some(&b':'))
        .map(graphicle::ids::normalize_id)
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}_"));
    // Pass 1: relpath, start line, file-ness, innermost nesting parent.
    let mut relpaths = Vec::with_capacity(count);
    let mut starts = Vec::with_capacity(count);
    let mut files = Vec::with_capacity(count);
    for node in graph.nodes() {
        let path = text(&node.attributes, "source_file")
            .map(|f| relpath(&f, options.strip_prefix.as_deref()))
            .unwrap_or_default();
        starts.push(start_line(&node.attributes));
        files.push(is_file_node(&node.attributes, &path));
        relpaths.push(path);
    }
    let mut parent: Vec<Option<usize>> = vec![None; count];
    for index in 0..count {
        if files[index] {
            continue;
        }
        let mut best: Option<usize> = None;
        for (source, _, edge) in graph.in_edges(index) {
            let relation = text(&edge.attributes, "relation").unwrap_or_default();
            if !NESTING_RELATIONS.contains(&relation.as_str())
                || source == index
                || files[source]
                || relpaths[source] != relpaths[index]
            {
                continue;
            }
            // Innermost = latest start not after ours; ties by smaller identifier.
            let better = match best {
                None => true,
                Some(current) => {
                    let candidate = starts[source].unwrap_or(0);
                    let existing = starts[current].unwrap_or(0);
                    candidate > existing
                        || (candidate == existing
                            && string(&graph.nodes()[source].id)
                                < string(&graph.nodes()[current].id))
                }
            };
            if better && starts[source].unwrap_or(0) <= starts[index].unwrap_or(usize::MAX) {
                best = Some(source);
            }
        }
        parent[index] = best;
    }
    // Pass 2: qualified names along the parent chain (cycle-safe).
    let mut qualified: Vec<String> = vec![String::new(); count];
    for index in 0..count {
        let mut chain = vec![];
        let mut seen = BTreeSet::new();
        let mut cursor = Some(index);
        while let Some(node) = cursor {
            if !seen.insert(node) {
                break;
            }
            chain.push(node);
            cursor = parent[node];
        }
        chain.reverse();
        let own = |i: usize| {
            let node = &graph.nodes()[i];
            let label = text(&node.attributes, "label").unwrap_or_else(|| string(&node.id));
            if relpaths[i].is_empty()
                && node.attributes.get("external") == Some(&Value::Bool(true))
                && label == string(&node.id)
                && let Some(suffix) = external_prefix
                    .as_deref()
                    .and_then(|p| label.strip_prefix(p))
                && !suffix.is_empty()
            {
                return format!("@external:{suffix}");
            }
            label
        };
        qualified[index] = if files[index] {
            relpaths[index].clone()
        } else {
            chain
                .iter()
                .map(|&i| name_part(&own(i)))
                .collect::<Vec<_>>()
                .join(".")
        };
    }
    // Pass 3: ordinals for identical (relpath, qualified, kind), in line order
    // then identifier order, so overloads get distinct stable ids.
    let mut kinds: Vec<String> = (0..count)
        .map(|index| {
            if files[index] {
                "file".into()
            } else {
                text(&graph.nodes()[index].attributes, "file_type")
                    .unwrap_or_else(|| "unknown".into())
            }
        })
        .collect();
    let mut groups: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
    for index in 0..count {
        groups
            .entry((
                relpaths[index].clone(),
                qualified[index].clone(),
                kinds[index].clone(),
            ))
            .or_default()
            .push(index);
    }
    let mut ordinal = vec![0usize; count];
    let mut disambiguated = 0;
    for members in groups.values_mut() {
        if members.len() > 1 {
            members.sort_by(|&a, &b| {
                starts[a]
                    .unwrap_or(0)
                    .cmp(&starts[b].unwrap_or(0))
                    .then_with(|| string(&graph.nodes()[a].id).cmp(&string(&graph.nodes()[b].id)))
            });
            disambiguated += members.len();
            for (position, &member) in members.iter().enumerate() {
                ordinal[member] = position;
            }
        }
    }
    // Pass 4: next symbol start per file, for revision slices.
    let mut per_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for index in 0..count {
        if !files[index] && starts[index].is_some() && !relpaths[index].is_empty() {
            per_file.entry(&relpaths[index]).or_default().push(index);
        }
    }
    let mut next_start: Vec<Option<usize>> = vec![None; count];
    for members in per_file.values() {
        let mut lines: Vec<usize> = members.iter().filter_map(|&i| starts[i]).collect();
        lines.sort_unstable();
        lines.dedup();
        for &member in members {
            next_start[member] = span::next_start(&lines, starts[member].unwrap_or(0));
        }
    }
    // Pass 5: write attributes.
    let mut loader = Loader {
        options,
        cache: HashMap::new(),
    };
    let mut with_revision = 0;
    let mut missing = BTreeSet::new();
    let mut updates: Vec<(String, Option<String>)> = Vec::with_capacity(count);
    for index in 0..count {
        let stable_id = stable_hash(
            &options.repo,
            &relpaths[index],
            &qualified[index],
            &kinds[index],
            ordinal[index],
        );
        let revision = if relpaths[index].is_empty() {
            None
        } else {
            match loader.source(&relpaths[index]) {
                Some(source) => {
                    Some(source.revision(starts[index], next_start[index], files[index]))
                }
                None => {
                    missing.insert(relpaths[index].clone());
                    None
                }
            }
        };
        if revision.is_some() {
            with_revision += 1;
        }
        updates.push((stable_id, revision));
    }
    for ((index, node), (stable_id, revision)) in graph.nodes_mut().enumerate().zip(updates) {
        node.attributes
            .insert("stable_id".into(), Value::String(stable_id));
        node.attributes.insert(
            "revision".into(),
            revision.map(Value::String).unwrap_or(Value::Null),
        );
        node.attributes.insert(
            "qualified_name".into(),
            Value::String(std::mem::take(&mut qualified[index])),
        );
        node.attributes.insert(
            "kind".into(),
            Value::String(std::mem::take(&mut kinds[index])),
        );
        if !relpaths[index].is_empty() {
            node.attributes.insert(
                "relpath".into(),
                Value::String(std::mem::take(&mut relpaths[index])),
            );
        }
        if !options.repo.is_empty() {
            node.attributes.insert("repo".into(), json!(options.repo));
        }
    }
    let mut edges = 0;
    for edge in graph.edges_mut() {
        edges += 1;
        let confidence = text(&edge.attributes, "confidence");
        edge.attributes.insert(
            "derivation".into(),
            json!(derivation(confidence.as_deref())),
        );
        let mut record = Map::new();
        if let Some(file) = text(&edge.attributes, "source_file") {
            record.insert(
                "source_file".into(),
                json!(relpath(&file, options.strip_prefix.as_deref())),
            );
        }
        if let Some(location) = text(&edge.attributes, "source_location") {
            record.insert("source_location".into(), json!(location));
        }
        record.insert(
            "extractor".into(),
            json!(match derivation(confidence.as_deref()) {
                "syntactic" | "inferred" => "tree-sitter",
                "semantic" => "model",
                _ => "unknown",
            }),
        );
        edge.attributes.insert(
            "provenance".into(),
            Value::Array(vec![Value::Object(record)]),
        );
    }
    Ok(IdentitySummary {
        nodes: count,
        edges,
        with_revision,
        disambiguated,
        missing_sources: missing.into_iter().collect(),
    })
}

/// Parse the options shared by the JSON boundary and the typed API.
pub(crate) fn options_from(request: &Value) -> Result<IdentityOptions> {
    let mut options = IdentityOptions::default();
    if let Some(repo) = request.get("repo").filter(|v| !v.is_null()) {
        options.repo = repo
            .as_str()
            .ok_or_else(|| Error::invalid("Expected repo to be a string"))?
            .to_string();
    }
    if let Some(root) = request.get("root").filter(|v| !v.is_null()) {
        options.root =
            Some(PathBuf::from(root.as_str().ok_or_else(|| {
                Error::invalid("Expected root to be a string")
            })?));
    }
    if let Some(prefix) = request.get("strip_prefix").filter(|v| !v.is_null()) {
        options.strip_prefix = Some(
            prefix
                .as_str()
                .ok_or_else(|| Error::invalid("Expected strip_prefix to be a string"))?
                .to_string(),
        );
    }
    if let Some(sources) = request.get("sources").filter(|v| !v.is_null()) {
        let sources = sources
            .as_object()
            .ok_or_else(|| Error::invalid("Expected sources to be an object"))?;
        for (path, content) in sources {
            let content = content
                .as_str()
                .ok_or_else(|| Error::invalid("Expected sources values to be strings"))?;
            options
                .sources
                .insert(path.replace('\\', "/"), content.to_string());
        }
    }
    Ok(options)
}

fn input_graph(request: &Value) -> Result<Graph> {
    let reference = request.get("graph_ref").filter(|v| !v.is_null());
    let inline = request.get("graph").filter(|v| !v.is_null());
    let path = request.get("graph_path").filter(|v| !v.is_null());
    match (reference, inline, path) {
        (Some(Value::String(reference)), None, None) if !reference.is_empty() => {
            let handle = session::registry().get(reference)?;
            Ok((*handle).clone())
        }
        (Some(_), None, None) => Err(Error::invalid(
            "Expected graph_ref to be a non-empty string",
        )),
        (None, Some(graph), None) => Graph::from_node_link(graph, session::edges_key(request)?),
        (None, None, Some(Value::String(path))) => {
            crate::graph::affected::load_graph(Path::new(path))
        }
        (None, None, Some(_)) => Err(Error::invalid("Expected graph_path to be a string")),
        (None, None, None) => Err(Error::invalid("Expected graph, graph_path or graph_ref")),
        _ => Err(Error::invalid(
            "Provide exactly one of graph, graph_path or graph_ref",
        )),
    }
}

/// `identity.annotate` on the JSON boundary. Returns the annotated node-link
/// graph and the summary; with `open: <name>` the result is also opened as a
/// resident graph and its `graph_ref` returned instead of the payload.
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "identity.annotate" => {
            let options = options_from(request)?;
            let mut graph = input_graph(request)?;
            let summary = annotate(&mut graph, &options)?;
            let edges_key = session::edges_key(request)?;
            match request.get("open").filter(|v| !v.is_null()) {
                Some(Value::String(name)) => {
                    let info = session::registry().open(GraphHandle::new(graph), Some(name));
                    Ok(json!({"summary": summary, "graph_ref": info.graph_ref}))
                }
                Some(Value::Bool(true)) => {
                    let info = session::registry().open(GraphHandle::new(graph), None);
                    Ok(json!({"summary": summary, "graph_ref": info.graph_ref}))
                }
                Some(Value::Bool(false)) | None => Ok(json!({
                    "summary": summary,
                    "graph": graph.to_node_link(edges_key),
                })),
                Some(_) => Err(Error::invalid("Expected open to be a name or a boolean")),
            }
        }
        _ => Err(Error::UnsupportedOperation(
            "Unsupported identity operation".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::attributes;

    #[test]
    fn indexed_revision_keeps_exact_line_bytes_and_empty_ranges() {
        let source = SourceText::new(std::borrow::Cow::Borrowed("α\r\n\nlast 💡"));
        assert_eq!(
            source.revision(Some(1), Some(2), false),
            sha256_hex(&["α\r\n".as_bytes()])
        );
        assert_eq!(
            source.revision(Some(2), Some(3), false),
            sha256_hex(&[b"\n"])
        );
        assert_eq!(
            source.revision(Some(3), None, false),
            sha256_hex(&["last 💡".as_bytes()])
        );
        assert_eq!(source.revision(Some(8), None, false), sha256_hex(&[b""]));
        assert_eq!(source.revision(Some(3), Some(2), false), sha256_hex(&[b""]));
        assert_eq!(
            source.revision(Some(8), Some(2), true),
            sha256_hex(&["α\r\n\nlast 💡".as_bytes()])
        );
        let trailing = SourceText::new(std::borrow::Cow::Borrowed("a\n"));
        assert_eq!(trailing.revision(Some(2), None, false), sha256_hex(&[b""]));
    }

    fn sample() -> Graph {
        let mut graph = Graph::new(true, false);
        let node = |graph: &mut Graph, id: &str, label: &str, file: &str, line: &str| {
            graph
                .add_node(
                    json!(id),
                    attributes([
                        ("label", json!(label)),
                        ("file_type", json!("code")),
                        ("source_file", json!(file)),
                        ("source_location", json!(line)),
                    ]),
                )
                .unwrap();
        };
        node(&mut graph, "client", "client.py", "src/client.py", "L1");
        node(
            &mut graph,
            "client_timeout",
            "Timeout",
            "src/client.py",
            "L3",
        );
        node(
            &mut graph,
            "client_timeout_init",
            ".__init__()",
            "src/client.py",
            "L4",
        );
        node(&mut graph, "client_limits", "Limits", "src/client.py", "L7");
        node(
            &mut graph,
            "client_limits_init",
            ".__init__()",
            "src/client.py",
            "L8",
        );
        node(&mut graph, "client_get", "get", "src/client.py", "L11");
        node(&mut graph, "client_get_dup", "get", "src/client.py", "L14");
        let edge = |graph: &mut Graph, s: &str, t: &str, relation: &str, confidence: &str| {
            graph
                .add_edge(
                    json!(s),
                    json!(t),
                    None,
                    attributes([
                        ("relation", json!(relation)),
                        ("confidence", json!(confidence)),
                        ("source_file", json!("src/client.py")),
                    ]),
                )
                .unwrap();
        };
        edge(
            &mut graph,
            "client",
            "client_timeout",
            "contains",
            "EXTRACTED",
        );
        edge(
            &mut graph,
            "client_timeout",
            "client_timeout_init",
            "method",
            "EXTRACTED",
        );
        edge(
            &mut graph,
            "client",
            "client_limits",
            "contains",
            "EXTRACTED",
        );
        edge(
            &mut graph,
            "client_limits",
            "client_limits_init",
            "method",
            "EXTRACTED",
        );
        edge(&mut graph, "client", "client_get", "contains", "EXTRACTED");
        edge(
            &mut graph,
            "client_get",
            "client_timeout",
            "calls",
            "INFERRED",
        );
        graph
    }

    const SOURCE: &str = "# client\n\nclass Timeout:\n    def __init__(self):\n        pass\n\nclass Limits:\n    def __init__(self):\n        pass\n\ndef get():\n    return Timeout()\n\ndef get(x):\n    return x\n";

    fn options() -> IdentityOptions {
        IdentityOptions {
            repo: "acme/httpx".into(),
            sources: BTreeMap::from([("client.py".to_string(), SOURCE.to_string())]),
            strip_prefix: Some("src".into()),
            ..Default::default()
        }
    }

    fn attr(graph: &Graph, id: &str, key: &str) -> Value {
        let index = graph.node_index(&json!(id)).unwrap();
        graph.nodes()[index].attributes[key].clone()
    }

    #[test]
    fn checkout_prefixes_are_components_and_external_identity_is_portable() {
        assert_eq!(
            relpath("/work/repository/a.py", Some("/work/repo")),
            "/work/repository/a.py"
        );
        assert_eq!(relpath("srcology/a.py", Some("src")), "srcology/a.py");
        assert_eq!(relpath("src/a.py", Some("src")), "a.py");
        let build = |root: &str| {
            let identifier = graphicle::ids::make_id([root, "missing/module"]);
            let mut graph = Graph::new(true, false);
            graph
                .add_node(
                    json!(&identifier),
                    attributes([
                        ("label", json!(&identifier)),
                        ("external", json!(true)),
                        ("source_file", json!("")),
                        ("file_type", json!("concept")),
                    ]),
                )
                .unwrap();
            annotate(
                &mut graph,
                &IdentityOptions {
                    repo: "same/repo".into(),
                    strip_prefix: Some(root.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            (identifier, graph)
        };
        let (left, a) = build("/work/one/repo");
        let (right, b) = build("/elsewhere/two/repo");
        assert_ne!(left, right);
        assert_eq!(attr(&a, &left, "stable_id"), attr(&b, &right, "stable_id"));
        assert_eq!(
            attr(&a, &left, "qualified_name"),
            json!("@external:missing_module")
        );
        assert_eq!(a.nodes()[0].id, json!(left));
        assert_eq!(attr(&a, &left, "label"), json!(left));
    }

    #[test]
    fn qualified_names_follow_the_nesting_chain() {
        let mut graph = sample();
        let summary = annotate(&mut graph, &options()).unwrap();
        assert_eq!(attr(&graph, "client", "qualified_name"), json!("client.py"));
        assert_eq!(attr(&graph, "client", "kind"), json!("file"));
        assert_eq!(
            attr(&graph, "client_timeout_init", "qualified_name"),
            json!("Timeout.__init__")
        );
        assert_eq!(
            attr(&graph, "client_limits_init", "qualified_name"),
            json!("Limits.__init__")
        );
        assert_eq!(attr(&graph, "client_get", "relpath"), json!("client.py"));
        assert_eq!(summary.nodes, 7);
        assert_eq!(summary.with_revision, 7);
        assert!(summary.missing_sources.is_empty());
    }

    #[test]
    fn same_label_symbols_keep_distinct_stable_ids() {
        let mut graph = sample();
        let summary = annotate(&mut graph, &options()).unwrap();
        assert_ne!(
            attr(&graph, "client_timeout_init", "stable_id"),
            attr(&graph, "client_limits_init", "stable_id")
        );
        assert_ne!(
            attr(&graph, "client_get", "stable_id"),
            attr(&graph, "client_get_dup", "stable_id")
        );
        assert_eq!(summary.disambiguated, 2);
    }

    #[test]
    fn revision_covers_only_the_symbols_own_lines() {
        let mut graph = sample();
        annotate(&mut graph, &options()).unwrap();
        let before = attr(&graph, "client_limits_init", "revision");
        let timeout_before = attr(&graph, "client_timeout_init", "revision");
        // Edit Limits.__init__ only: shift nothing, change its body.
        let mut edited = options();
        edited.sources.insert(
            "client.py".into(),
            SOURCE.replace(
                "        pass\n\ndef get():",
                "        self.x = 1\n\ndef get():",
            ),
        );
        let mut graph2 = sample();
        annotate(&mut graph2, &edited).unwrap();
        assert_ne!(attr(&graph2, "client_limits_init", "revision"), before);
        assert_eq!(
            attr(&graph2, "client_timeout_init", "revision"),
            timeout_before
        );
        assert_eq!(
            attr(&graph2, "client_timeout_init", "stable_id"),
            attr(&graph, "client_timeout_init", "stable_id")
        );
    }

    #[test]
    fn stable_ids_are_scoped_by_repository() {
        let mut a = sample();
        let mut b = sample();
        annotate(&mut a, &options()).unwrap();
        let mut other = options();
        other.repo = "acme/other".into();
        annotate(&mut b, &other).unwrap();
        assert_ne!(
            attr(&a, "client_get", "stable_id"),
            attr(&b, "client_get", "stable_id")
        );
        assert_eq!(attr(&a, "client_get", "repo"), json!("acme/httpx"));
    }

    /// The logical identity read back from an
    /// annotated graph recomputes every `stable_id`, in the graph's own scope
    /// and in another one (re-grounding), including the overloads that need
    /// an ordinal.
    #[test]
    fn logical_identities_recompute_stable_ids_in_any_scope() {
        let mut here = sample();
        annotate(&mut here, &options()).unwrap();
        let mut elsewhere = sample();
        let mut other = options();
        other.repo = "acme/other".into();
        annotate(&mut elsewhere, &other).unwrap();
        let identities = logical_identities(&here);
        assert_eq!(identities.len(), here.node_count());
        for (index, identity) in identities.iter().enumerate() {
            let identity = identity.as_ref().unwrap();
            let node = &here.nodes()[index];
            assert_eq!(
                json!(identity.stable_id("acme/httpx")),
                node.attributes["stable_id"],
                "{}",
                string(&node.id)
            );
            assert_eq!(
                json!(identity.stable_id("acme/other")),
                elsewhere.nodes()[index].attributes["stable_id"]
            );
        }
        let get = |id: &str| identities[here.node_index(&json!(id)).unwrap()].clone();
        let (first, second) = (get("client_get").unwrap(), get("client_get_dup").unwrap());
        assert_eq!((first.ordinal, second.ordinal), (0, 1));
        assert_eq!(first.qualified_name, second.qualified_name);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!({"relpath":"client.py","qualified_name":"get","kind":"code","ordinal":0})
        );
        // A graph `annotate` never saw has no identities.
        assert!(logical_identities(&sample()).iter().all(Option::is_none));
        // The worked examples, when this tree ships them.
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../upstream-assets");
        for name in ["httpx", "rsl-siege-manager", "mixed-corpus"] {
            let path = assets.join("worked").join(name).join("graph.json");
            if !path.exists() {
                continue;
            }
            let mut graph = crate::graph::affected::load_graph(&path).unwrap();
            annotate(
                &mut graph,
                &IdentityOptions {
                    repo: name.into(),
                    root: Some(assets.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            for (node, identity) in graph.nodes().iter().zip(logical_identities(&graph)) {
                assert_eq!(
                    json!(identity.unwrap().stable_id(name)),
                    node.attributes["stable_id"],
                    "{name}: {}",
                    string(&node.id)
                );
            }
        }
    }

    #[test]
    fn missing_sources_are_reported_not_fabricated() {
        let mut graph = sample();
        let summary = annotate(&mut graph, &IdentityOptions::default()).unwrap();
        assert_eq!(summary.with_revision, 0);
        assert_eq!(summary.missing_sources, vec!["src/client.py".to_string()]);
        assert_eq!(attr(&graph, "client_get", "revision"), Value::Null);
        assert_eq!(
            attr(&graph, "client_get", "relpath"),
            json!("src/client.py")
        );
    }

    #[test]
    fn edges_carry_derivation_and_provenance() {
        let mut graph = sample();
        annotate(&mut graph, &options()).unwrap();
        let get = graph.node_index(&json!("client_get")).unwrap();
        let (_, _, edge) = graph.out_edges(get)[0];
        assert_eq!(edge.attributes["derivation"], json!("inferred"));
        assert_eq!(
            edge.attributes["provenance"],
            json!([{"source_file": "client.py", "extractor": "tree-sitter"}])
        );
    }

    #[test]
    fn boundary_returns_graph_or_opens_it() {
        let graph = sample();
        let request = json!({
            "operation": "identity.annotate",
            "graph": graph.to_node_link("links"),
            "repo": "acme/httpx",
            "strip_prefix": "src",
            "sources": {"client.py": SOURCE},
        });
        let result = dispatch(&request).unwrap();
        assert_eq!(result["summary"]["with_revision"], json!(7));
        assert!(result["graph"]["nodes"][0]["stable_id"].is_string());
        let mut opened = request.clone();
        opened["open"] = json!("annotated-sample");
        let result = dispatch(&opened).unwrap();
        assert_eq!(result["graph_ref"], json!("annotated-sample"));
        let handle = session::registry().get("annotated-sample").unwrap();
        assert!(handle.nodes()[1].attributes.contains_key("stable_id"));
        assert!(session::registry().close("annotated-sample"));
        assert!(dispatch(&json!({"operation": "identity.annotate"})).is_err());
        assert!(
            dispatch(&json!({"operation": "identity.annotate", "graph": graph.to_node_link("links"), "graph_ref": "x"}))
                .is_err()
        );
    }

    /// The shared span helper must not change one byte of any
    /// `revision`. `frozen` is the derivation as it stood before the helper;
    /// the corpus is the identity oracle's worked examples
    /// (`check_identity.py`) against their shipped sources. Every line pair
    /// of every source is compared too.
    mod frozen {
        use super::super::{is_file_node, relpath, sha256_hex, text};
        use crate::graph::graph::Graph;
        use serde_json::Value;
        use std::collections::BTreeMap;
        use std::path::Path;

        pub fn start_line(attributes: &impl crate::graph::record::Properties) -> Option<usize> {
            if let Some(span) = attributes.get("span").and_then(Value::as_object)
                && let Some(line) = span.get("start_line").and_then(Value::as_u64)
            {
                return Some(line as usize);
            }
            let location = text(attributes, "source_location")?;
            location.strip_prefix('L')?.split('-').next()?.parse().ok()
        }

        pub fn revision(
            text: &str,
            start: Option<usize>,
            next: Option<usize>,
            file: bool,
        ) -> String {
            let starts: Vec<usize> = std::iter::once(0)
                .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
                .collect();
            let source = text.as_bytes();
            if file {
                return sha256_hex(&[source]);
            }
            let offset = |line: usize| {
                starts
                    .get(line.saturating_sub(1))
                    .copied()
                    .unwrap_or(source.len())
            };
            let start = offset(start.unwrap_or(1).max(1));
            let end = next.map(offset).unwrap_or(source.len()).max(start);
            sha256_hex(&[&source[start..end]])
        }

        /// Revisions per node, as `annotate` derived them before the helper.
        pub fn revisions(graph: &Graph, root: &Path) -> Vec<Option<String>> {
            let count = graph.node_count();
            let mut relpaths = Vec::with_capacity(count);
            let mut starts = Vec::with_capacity(count);
            let mut files = Vec::with_capacity(count);
            for node in graph.nodes() {
                let path = text(&node.attributes, "source_file")
                    .map(|f| relpath(&f, None))
                    .unwrap_or_default();
                starts.push(start_line(&node.attributes));
                files.push(is_file_node(&node.attributes, &path));
                relpaths.push(path);
            }
            let mut per_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
            for index in 0..count {
                if !files[index] && starts[index].is_some() && !relpaths[index].is_empty() {
                    per_file.entry(&relpaths[index]).or_default().push(index);
                }
            }
            let mut next_start: Vec<Option<usize>> = vec![None; count];
            for members in per_file.values() {
                let mut lines: Vec<usize> = members.iter().filter_map(|&i| starts[i]).collect();
                lines.sort_unstable();
                lines.dedup();
                for &member in members {
                    let start = starts[member].unwrap_or(0);
                    next_start[member] = lines
                        .get(lines.partition_point(|&line| line <= start))
                        .copied();
                }
            }
            (0..count)
                .map(|index| {
                    if relpaths[index].is_empty() {
                        return None;
                    }
                    let bytes = std::fs::read(root.join(&relpaths[index])).ok()?;
                    let source = String::from_utf8_lossy(&bytes);
                    Some(revision(
                        &source,
                        starts[index],
                        next_start[index],
                        files[index],
                    ))
                })
                .collect()
        }
    }

    #[test]
    fn revisions_match_the_pre_helper_derivation_on_the_worked_examples() {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../upstream-assets");
        let mut compared = 0;
        let mut sources = BTreeSet::new();
        for name in ["httpx", "rsl-siege-manager", "mixed-corpus"] {
            let path = assets.join("worked").join(name).join("graph.json");
            if !path.exists() {
                // The public export leaves the worked graphs out.
                continue;
            }
            let mut graph = crate::graph::affected::load_graph(&path).unwrap();
            let expected = frozen::revisions(&graph, &assets);
            for node in graph.nodes() {
                if let Some(file) = text(&node.attributes, "source_file") {
                    sources.insert(file);
                }
            }
            let options = IdentityOptions {
                repo: name.into(),
                root: Some(assets.clone()),
                ..Default::default()
            };
            annotate(&mut graph, &options).unwrap();
            for (node, want) in graph.nodes().iter().zip(&expected) {
                let got = node.attributes.get("revision").and_then(Value::as_str);
                assert_eq!(got, want.as_deref(), "{name}: {}", string(&node.id));
                compared += usize::from(want.is_some());
            }
        }
        if compared == 0 {
            eprintln!("skipped: no worked-example graph with sources in this tree");
            return;
        }
        assert!(compared > 100, "{compared}");
        // Every start line of every shipped source against the next starts a
        // symbol can have, including starts and ends past the last line,
        // missing ones and inverted ranges.
        let mut pairs = 0;
        for file in sources {
            let Ok(bytes) = std::fs::read(assets.join(&file)) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let source = SourceText::new(std::borrow::Cow::Borrowed(&text));
            let lines = text.matches('\n').count() + 1;
            // About 40 start lines per file, plus the edges.
            let step = (lines / 40).max(1);
            let starts = (0..=lines + 2).step_by(step).chain([1, lines, lines + 1]);
            for start in starts.map(Some).chain([None]) {
                let at = start.unwrap_or(1);
                let nexts = [
                    None,
                    Some(0),
                    Some(at.saturating_sub(1)),
                    Some(at),
                    Some(at + 1),
                    Some(at + 2),
                    Some(at + 9),
                    Some(lines),
                    Some(lines + 2),
                ];
                for next in nexts {
                    for whole in [false, true] {
                        assert_eq!(
                            source.revision(start, next, whole),
                            frozen::revision(&text, start, next, whole),
                            "{file} {start:?} {next:?}"
                        );
                        pairs += 1;
                    }
                }
            }
        }
        assert!(pairs > 1_000, "{pairs}");
    }
}
