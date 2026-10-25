// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__init__.py
//! Validated records for native integrations. Unknown metadata stays extensible.
//!
//! The legacy NetworkX adapter also supports numeric and tuple identifiers.
//! Native symbol records deliberately use nonempty string identifiers and
//! require every edge endpoint to have a declared node.
use crate::{Error, Result, graph::Graph};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct NodeId(String);

impl Serialize for NodeId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl NodeId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty() || value.contains('\0') {
            return Err(Error::graph(
                "A node identifier must be nonempty and contain no NUL",
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for NodeId {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}
impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Code,
    Document,
    Paper,
    Image,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Confidence {
    Ambiguous,
    Inferred,
    Extracted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SpanWire", into = "SpanWire")]
pub struct SourceSpan {
    start_line: u32,
    end_line: u32,
}
#[derive(Serialize, Deserialize)]
struct SpanWire {
    start_line: u32,
    end_line: u32,
}
impl SourceSpan {
    pub fn new(start_line: u32, end_line: u32) -> Result<Self> {
        if start_line == 0 || end_line < start_line {
            return Err(Error::graph(
                "Source spans use ordered, one-based line numbers",
            ));
        }
        Ok(Self {
            start_line,
            end_line,
        })
    }
    pub fn start_line(self) -> u32 {
        self.start_line
    }
    pub fn end_line(self) -> u32 {
        self.end_line
    }
}
impl TryFrom<SpanWire> for SourceSpan {
    type Error = Error;
    fn try_from(span: SpanWire) -> Result<Self> {
        Self::new(span.start_line, span.end_line)
    }
}
impl From<SourceSpan> for SpanWire {
    fn from(span: SourceSpan) -> Self {
        Self {
            start_line: span.start_line,
            end_line: span.end_line,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<FileKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    #[serde(flatten)]
    pub attributes: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    #[serde(flatten)]
    pub attributes: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphDocument {
    #[serde(default)]
    pub directed: bool,
    #[serde(default)]
    pub multigraph: bool,
    #[serde(default)]
    pub graph: Map<String, Value>,
    pub nodes: Vec<NodeRecord>,
    #[serde(alias = "edges")]
    pub links: Vec<EdgeRecord>,
}

fn reserved(attributes: &Map<String, Value>, names: &[&str]) -> Result<()> {
    if let Some(name) = names.iter().find(|name| attributes.contains_key(**name)) {
        return Err(Error::graph(format!(
            "Metadata cannot shadow the typed field '{name}'"
        )));
    }
    Ok(())
}

impl GraphDocument {
    /// Consumes the records; attributes move into the graph without a JSON clone.
    pub fn into_graph(self) -> Result<Graph> {
        let mut graph = Graph::new(self.directed, self.multigraph);
        graph.attributes = Value::Object(self.graph);
        for node in self.nodes {
            reserved(
                &node.attributes,
                &["id", "label", "file_type", "source_file", "span"],
            )?;
            let id = Value::String(node.id.into());
            if graph.node_index(&id).is_some() {
                return Err(Error::graph(format!("Duplicate node identifier: {id}")));
            }
            let mut attributes = node.attributes;
            attributes.insert("label".into(), Value::String(node.label));
            if let Some(kind) = node.file_type {
                attributes.insert("file_type".into(), serde_json::to_value(kind)?);
            }
            if let Some(source) = node.source_file {
                attributes.insert("source_file".into(), Value::String(source));
            }
            if let Some(span) = node.span {
                attributes.insert("span".into(), serde_json::to_value(span)?);
            }
            graph.add_node(id, attributes)?;
        }
        for edge in self.links {
            reserved(
                &edge.attributes,
                &[
                    "source",
                    "target",
                    "relation",
                    "confidence",
                    "source_file",
                    "span",
                    "key",
                ],
            )?;
            if edge.relation.trim().is_empty() {
                return Err(Error::graph("An edge relation must be nonempty"));
            }
            let source = Value::String(edge.source.into());
            let target = Value::String(edge.target.into());
            if graph.node_index(&source).is_none() || graph.node_index(&target).is_none() {
                return Err(Error::graph(format!(
                    "Edge endpoint is not declared: {source} -> {target}"
                )));
            }
            let mut attributes = edge.attributes;
            attributes.insert("relation".into(), Value::String(edge.relation));
            if let Some(confidence) = edge.confidence {
                attributes.insert("confidence".into(), serde_json::to_value(confidence)?);
            }
            if let Some(source) = edge.source_file {
                attributes.insert("source_file".into(), Value::String(source));
            }
            if let Some(span) = edge.span {
                attributes.insert("span".into(), serde_json::to_value(span)?);
            }
            graph.add_edge(source, target, edge.key, attributes)?;
        }
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({"directed":true,"multigraph":true,"graph":{},"nodes":[
            {"id":"a","label":"Caller","file_type":"code","span":{"start_line":1,"end_line":3},"custom":{"owner":"team"}},
            {"id":"b","label":"Callee"}],"links":[
            {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED","key":0},
            {"source":"a","target":"b","relation":"references","confidence":"INFERRED","key":1}]})
    }

    #[test]
    fn typed_records_roundtrip_and_preserve_parallel_provenance() {
        let record: GraphDocument = serde_json::from_value(document()).unwrap();
        assert_eq!(record.nodes[0].span.unwrap().end_line(), 3);
        let graph = record.into_graph().unwrap();
        assert_eq!(graph.edge_count(), 2);
        assert_eq!(graph.nodes[0].attributes["custom"]["owner"], "team");
        let result: GraphDocument = serde_json::from_value(graph.to_node_link("links")).unwrap();
        assert_eq!(result.links[0].confidence, Some(Confidence::Extracted));
        assert_eq!(result.links[1].relation, "references");
    }

    #[test]
    fn invalid_records_are_rejected_at_the_boundary() {
        for id in ["", " ", "bad\0id"] {
            assert!(NodeId::new(id).is_err());
        }
        for (start, end) in [(0, 1), (3, 2)] {
            assert!(SourceSpan::new(start, end).is_err());
        }
        let mut invalid = document();
        invalid["nodes"][0]["span"]["end_line"] = json!(0);
        assert!(serde_json::from_value::<GraphDocument>(invalid).is_err());
        let mut missing = document();
        missing["links"][0]["target"] = json!("absent");
        assert!(
            serde_json::from_value::<GraphDocument>(missing)
                .unwrap()
                .into_graph()
                .is_err()
        );
        let mut duplicate = document();
        duplicate["nodes"][1]["id"] = json!("a");
        assert!(
            serde_json::from_value::<GraphDocument>(duplicate)
                .unwrap()
                .into_graph()
                .is_err()
        );
        let mut record: GraphDocument = serde_json::from_value(document()).unwrap();
        record.nodes[0]
            .attributes
            .insert("id".into(), json!("hidden-id"));
        assert!(record.into_graph().is_err());
    }
}
