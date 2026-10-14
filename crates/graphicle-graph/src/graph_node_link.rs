// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py
//! Streaming node-link deserialization straight into a [`Graph`].
//!
//! `Graph::from_node_link_owned` first materialises the whole document as a
//! dynamic JSON value and then dismantles every node and edge object again.
//! For large graphs that double handling dominates request latency. This
//! module reads the same document once, keeping only the attribute maps and
//! the `id` / `source` / `target` / `key` fields the graph needs.
//!
//! The fast path is strict: anything it cannot represent identically to the
//! dynamic loader (unhashable endpoints, missing arrays, a custom edge field
//! that aliases `nodes`, ...) fails and the caller falls back to the dynamic
//! loader so error messages and edge cases stay byte-for-byte identical.
use crate::graph::Graph;
use crate::record::{Attributes, Field};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::fmt;

/// Seed selecting the edge array field (`links` or `edges`).
pub struct NodeLinkSeed<'a> {
    pub edges_key: &'a str,
}

struct RawNode {
    id: Option<Value>,
    attributes: Attributes,
}

struct RawEdge {
    source: Value,
    target: Value,
    /// The `key` field and the attribute position it occupied, so simple
    /// graphs can put it back exactly where the dynamic loader leaves it.
    key: Option<(usize, Value)>,
    attributes: Attributes,
}

/// Node-link document being streamed into a graph.
///
/// Nodes and edges are added the moment they are read, so the graph must
/// already know `directed` / `multigraph` when the node array starts and the
/// edge array must follow the node array. That is the order NetworkX and
/// Graphify emit; anything else is declined and handled dynamically.
struct Document {
    directed: Option<Value>,
    multigraph: Option<Value>,
    attributes: Option<Value>,
    graph: Option<Graph>,
    edges_seen: bool,
}

fn declined<E: serde::de::Error>(reason: &str) -> E {
    E::custom(reason)
}

impl Document {
    fn start_graph<E: serde::de::Error>(&mut self) -> Result<&mut Graph, E> {
        if self.graph.is_some() {
            return Err(declined("repeated node array"));
        }
        let directed = self
            .directed
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let multigraph = self
            .multigraph
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok(self.graph.insert(Graph::new(directed, multigraph)))
    }

    fn finish(self) -> crate::Result<Graph> {
        if self.attributes.as_ref().is_some_and(|a| !a.is_object()) {
            return Err(crate::Error::graph(
                "Graph and graph attributes must be objects",
            ));
        }
        let mut graph = self.graph.ok_or("Expected graph nodes")?;
        if !self.edges_seen {
            return Err("Expected graph edges".into());
        }
        graph.attributes = self.attributes.unwrap_or_else(|| json!({}));
        Ok(graph)
    }
}

fn add_raw_edge(graph: &mut Graph, edge: RawEdge) -> crate::Result<()> {
    let RawEdge {
        source,
        target,
        key,
        mut attributes,
    } = edge;
    for value in [&source, &target] {
        if value
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.is_array() || v.is_object()))
        {
            return Err("TypeError: unhashable endpoint".into());
        }
    }
    let key = match key {
        Some((_, key)) if graph.multigraph => Some(key),
        Some((position, key)) => {
            attributes.shift_insert(position.min(attributes.len()), "key".into(), key);
            None
        }
        None => None,
    };
    graph.add_edge(source, target, key, attributes)?;
    Ok(())
}

impl<'de> DeserializeSeed<'de> for NodeLinkSeed<'_> {
    type Value = Graph;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Graph, D::Error> {
        if self.edges_key == "nodes" {
            return Err(declined("edge field aliases nodes"));
        }
        let document = deserializer.deserialize_map(DocumentVisitor {
            edges_key: self.edges_key,
        })?;
        document.finish().map_err(serde::de::Error::custom)
    }
}

struct DocumentVisitor<'a> {
    edges_key: &'a str,
}

enum DocumentField {
    Directed,
    Multigraph,
    Attributes,
    Nodes,
    Edges,
    Other,
}

struct DocumentKey<'a> {
    edges_key: &'a str,
}

impl<'de> DeserializeSeed<'de> for DocumentKey<'_> {
    type Value = DocumentField;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<DocumentField, D::Error> {
        d.deserialize_str(self)
    }
}

impl Visitor<'_> for DocumentKey<'_> {
    type Value = DocumentField;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a node-link field name")
    }
    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<DocumentField, E> {
        Ok(match key {
            _ if key == self.edges_key => DocumentField::Edges,
            "directed" => DocumentField::Directed,
            "multigraph" => DocumentField::Multigraph,
            "graph" => DocumentField::Attributes,
            "nodes" => DocumentField::Nodes,
            _ => DocumentField::Other,
        })
    }
}

impl<'de> Visitor<'de> for DocumentVisitor<'_> {
    type Value = Document;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a node-link graph object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Document, A::Error> {
        let mut document = Document {
            directed: None,
            multigraph: None,
            attributes: None,
            graph: None,
            edges_seen: false,
        };
        // Later duplicates win, exactly as they do in a dynamic JSON object;
        // a duplicate that would change an already-built graph is declined.
        while let Some(field) = map.next_key_seed(DocumentKey {
            edges_key: self.edges_key,
        })? {
            match field {
                DocumentField::Directed | DocumentField::Multigraph if document.graph.is_some() => {
                    return Err(declined("graph flags after nodes"));
                }
                DocumentField::Directed => document.directed = Some(map.next_value()?),
                DocumentField::Multigraph => document.multigraph = Some(map.next_value()?),
                DocumentField::Attributes => document.attributes = Some(map.next_value()?),
                DocumentField::Nodes => {
                    let graph = document.start_graph()?;
                    map.next_value_seed(Nodes(graph))?;
                }
                DocumentField::Edges => {
                    if document.edges_seen {
                        return Err(declined("repeated edge array"));
                    }
                    let graph = document
                        .graph
                        .as_mut()
                        .ok_or_else(|| declined("edges before nodes"))?;
                    map.next_value_seed(Edges(graph))?;
                    document.edges_seen = true;
                }
                DocumentField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(document)
    }
}

struct Nodes<'g>(&'g mut Graph);

impl<'de> DeserializeSeed<'de> for Nodes<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Nodes<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array of node objects")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        while let Some(node) = seq.next_element_seed(NodeSeed)? {
            let id = node.id.unwrap_or_else(|| json!(index));
            self.0
                .add_node(id, node.attributes)
                .map_err(serde::de::Error::custom)?;
            index += 1;
        }
        Ok(())
    }
}

struct NodeSeed;

impl<'de> DeserializeSeed<'de> for NodeSeed {
    type Value = RawNode;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<RawNode, D::Error> {
        d.deserialize_map(self)
    }
}

/// Attribute key that recognises one reserved field without allocating.
struct Reserved<const N: usize>([&'static str; N]);

enum AttributeKey {
    Reserved(usize),
    Attribute(Field),
}

impl<'de, const N: usize> DeserializeSeed<'de> for Reserved<N> {
    type Value = AttributeKey;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<AttributeKey, D::Error> {
        d.deserialize_string(self)
    }
}

impl<const N: usize> Visitor<'_> for Reserved<N> {
    type Value = AttributeKey;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an attribute name")
    }
    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<AttributeKey, E> {
        Ok(match self.0.iter().position(|r| *r == key) {
            Some(index) => AttributeKey::Reserved(index),
            None => AttributeKey::Attribute(Field::borrowed(key)),
        })
    }
    fn visit_string<E: serde::de::Error>(self, key: String) -> Result<AttributeKey, E> {
        Ok(match self.0.iter().position(|r| *r == key) {
            Some(index) => AttributeKey::Reserved(index),
            None => AttributeKey::Attribute(Field::new(key)),
        })
    }
}

impl<'de> Visitor<'de> for NodeSeed {
    type Value = RawNode;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a node object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawNode, A::Error> {
        let mut node = RawNode {
            id: None,
            attributes: Attributes::new(),
        };
        while let Some(key) = map.next_key_seed(Reserved(["id"]))? {
            match key {
                AttributeKey::Reserved(_) => node.id = Some(map.next_value()?),
                AttributeKey::Attribute(name) => {
                    let value = map.next_value()?;
                    node.attributes.insert_field(name, value);
                }
            }
        }
        node.attributes.compact();
        Ok(node)
    }
}

struct Edges<'g>(&'g mut Graph);

impl<'de> DeserializeSeed<'de> for Edges<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Edges<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array of edge objects")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(edge) = seq.next_element_seed(EdgeSeed)? {
            add_raw_edge(self.0, edge).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

struct EdgeSeed;

impl<'de> DeserializeSeed<'de> for EdgeSeed {
    type Value = RawEdge;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<RawEdge, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for EdgeSeed {
    type Value = RawEdge;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an edge object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawEdge, A::Error> {
        let (mut source, mut target, mut key) = (None, None, None::<(usize, Value)>);
        let mut attributes = Attributes::new();
        while let Some(field) = map.next_key_seed(Reserved(["source", "target", "key"]))? {
            match field {
                AttributeKey::Reserved(0) => source = Some(map.next_value()?),
                AttributeKey::Reserved(1) => target = Some(map.next_value()?),
                AttributeKey::Reserved(_) => {
                    let value = map.next_value()?;
                    // A repeated key keeps its first position, like a map insert.
                    let position = key.as_ref().map_or(attributes.len(), |(p, _)| *p);
                    key = Some((position, value));
                }
                AttributeKey::Attribute(name) => {
                    let value = map.next_value()?;
                    attributes.insert_field(name, value);
                }
            }
        }
        // Reuse spare allocation space while loading, instead of retaining it
        // on every edge until the whole graph becomes resident.
        attributes.compact();
        Ok(RawEdge {
            source: source.ok_or_else(|| serde::de::Error::missing_field("source"))?,
            target: target.ok_or_else(|| serde::de::Error::missing_field("target"))?,
            key,
            attributes,
        })
    }
}

impl Graph {
    /// Load a node-link document directly from JSON text.
    ///
    /// Returns `Ok(None)` when the strict streaming loader declines the input;
    /// callers should then parse the document dynamically so results and
    /// error messages match `from_node_link_owned` exactly.
    pub fn from_node_link_json(text: &str, edges_key: &str) -> Option<Graph> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let graph = NodeLinkSeed { edges_key }
            .deserialize(&mut deserializer)
            .ok()?;
        deserializer.end().ok()?;
        Some(graph)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dynamic(text: &str, edges_key: &str) -> Graph {
        Graph::from_node_link_owned(serde_json::from_str(text).unwrap(), edges_key).unwrap()
    }

    fn same(text: &str, edges_key: &str) {
        let streamed = Graph::from_node_link_json(text, edges_key).unwrap();
        let expected = dynamic(text, edges_key);
        assert_eq!(
            streamed.to_node_link(edges_key),
            expected.to_node_link(edges_key),
            "{text}"
        );
    }

    #[test]
    fn matches_dynamic_loader_on_ordinary_documents() {
        same(
            r#"{"directed":true,"multigraph":true,"graph":{"name":"g"},
            "nodes":[{"id":"a","label":"A","n":1.5},{"label":"anon"},{"id":3,"x":[1,{"y":null}]}],
            "links":[{"source":"a","target":3,"key":0,"relation":"calls","w":0.5},
                     {"source":3,"target":"a","relation":"uses"},
                     {"relation":"first","source":"a","target":"a","key":"k","z":true}]}"#,
            "links",
        );
    }

    #[test]
    fn keeps_key_attribute_position_for_simple_graphs() {
        same(
            r#"{"directed":false,"multigraph":false,"nodes":[{"id":"a"},{"id":"b"}],
            "edges":[{"source":"a","before":1,"key":"kept","after":2,"target":"b"},
                     {"key":"first","source":"b","target":"b"},
                     {"source":"a","target":"b","other":1,"key":"last"}]}"#,
            "edges",
        );
    }

    #[test]
    fn repeated_fields_follow_last_wins_semantics() {
        same(
            r#"{"directed":1,"multigraph":true,"multigraph":true,"ignored":[1,2,3],
            "nodes":[{"id":"x","id":"y","p":1,"p":2},{"id":"z","q":1,"p":2,"q":3}],
            "links":[{"source":"z","target":"y","key":1,"key":2,"a":1,"a":{"b":2}}],
            "graph":{"k":1},"graph":{"k":2},"also_ignored":null}"#,
            "links",
        );
    }

    #[test]
    fn declines_inputs_the_dynamic_loader_handles_specially() {
        for (text, edges_key) in [
            (r#"{"nodes":[{"id":"a"}]}"#, "links"),
            (
                r#"{"nodes":[{"id":"a"}],"links":[{"source":"a"}]}"#,
                "links",
            ),
            (
                r#"{"nodes":[{"id":"a"}],"links":[{"source":[[1]],"target":"a"}]}"#,
                "links",
            ),
            (r#"{"nodes":[{"id":"a"}],"links":[]}"#, "nodes"),
            (r#"{"graph":1,"nodes":[],"links":[]}"#, "links"),
            (r#"{"nodes":[1],"links":[]}"#, "links"),
            (r#"{"nodes":[],"links":[]} trailing"#, "links"),
            (r#"[]"#, "links"),
            (r#"{"links":[],"nodes":[]}"#, "links"),
            (r#"{"nodes":[],"directed":true,"links":[]}"#, "links"),
            (r#"{"nodes":[],"links":[],"links":[]}"#, "links"),
            (r#"{"nodes":[],"nodes":[],"links":[]}"#, "links"),
        ] {
            assert!(
                Graph::from_node_link_json(text, edges_key).is_none(),
                "{text}"
            );
        }
    }
}
