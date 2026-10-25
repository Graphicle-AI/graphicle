// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/exporters/graphdb.py
//! Native graphify/exporters/graphdb.py: identical parameterized upserts, with
//! Rust Bolt and RESP transports replacing the Python client libraries.
use super::common::*;
use crate::{Result, cluster::Communities, graph::Graph};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Statement {
    pub query: String,
    pub params: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PushCounts {
    pub nodes: usize,
    pub edges: usize,
}
pub fn safe_relation(raw: &str) -> String {
    let s = raw
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if s.is_empty() { "RELATED_TO".into() } else { s }
}
pub fn capitalize(raw: &str) -> String {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let upper = first.to_uppercase().collect::<String>();
    let mut expanded = upper.chars();
    let mut text = expanded.next().unwrap().to_string();
    text.push_str(&expanded.collect::<String>().to_lowercase());
    text.push_str(&chars.collect::<String>().to_lowercase());
    text
}
pub fn safe_label(raw: &str) -> String {
    let s = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>();
    if s.is_empty() { "Entity".into() } else { s }
}
fn props(attrs: &crate::record::Attributes) -> Map<String, Value> {
    attrs
        .iter()
        .filter(|(k, v)| !k.starts_with('_') && (v.is_string() || v.is_number() || v.is_boolean()))
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}
fn node_label(attributes: &crate::record::Attributes) -> Result<String> {
    let file_type = match attributes.get("file_type") {
        None => "Entity",
        Some(file_type) => file_type.as_str().ok_or("file_type must be a string")?,
    };
    Ok(safe_label(&capitalize(file_type)))
}
/// Distinct Cypher labels the node-upsert loop writes, sorted; derived exactly
/// as the upserts derive them so the up-front indexes cover every MERGE.
pub fn distinct_node_labels(graph: &Graph) -> Result<Vec<String>> {
    let labels = graph
        .nodes
        .iter()
        .map(|n| node_label(&n.attributes))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    Ok(labels.into_iter().collect())
}
// MERGE matches on (label, id); without an index each upsert scans the whole
// label and a push degrades to O(n^2) (#3804). Index failures are tolerated.
pub fn neo4j_index_statements(graph: &Graph) -> Result<Vec<Statement>> {
    Ok(distinct_node_labels(graph)?
        .into_iter()
        .map(|label| Statement {
            query: format!("CREATE INDEX IF NOT EXISTS FOR (n:{label}) ON (n.id)"),
            params: json!({}),
        })
        .collect())
}
/// FalkorDB has no `IF NOT EXISTS`; a re-push's "already indexed" error is
/// ignored, and no parameter header is sent (the SDK's `params=None`).
pub fn falkordb_index_statements(graph: &Graph) -> Result<Vec<Statement>> {
    Ok(distinct_node_labels(graph)?
        .into_iter()
        .map(|label| Statement {
            query: format!("CREATE INDEX FOR (n:{label}) ON (n.id)"),
            params: Value::Null,
        })
        .collect())
}
fn each_statement(
    graph: &Graph,
    communities: &Communities,
    mut send: impl FnMut(Statement) -> Result<()>,
) -> Result<PushCounts> {
    let assign = assignments(graph, communities);
    let mut counts = PushCounts { nodes: 0, edges: 0 };
    for (i, n) in graph.nodes.iter().enumerate() {
        let mut properties = props(&n.attributes);
        properties.insert("id".into(), n.id.clone());
        if let Some(cid) = assign[i] {
            properties.insert("community".into(), json!(cid));
        }
        let label = node_label(&n.attributes)?;
        send(Statement {
            query: format!("MERGE (n:{label} {{id: $id}}) SET n += $props"),
            params: json!({"id":n.id,"props":properties}),
        })?;
        counts.nodes += 1;
    }
    for (u, v, e) in graph.iter_edges() {
        let relation = e
            .attributes
            .get("relation")
            .map(|r| r.as_str().ok_or("relation must be a string"))
            .transpose()?
            .unwrap_or("RELATED_TO");
        let relation = safe_relation(relation);
        send(Statement {
            query: format!(
                "MATCH (a {{id: $src}}), (b {{id: $tgt}}) MERGE (a)-[r:{relation}]->(b) SET r += $props"
            ),
            params: json!({"src":e.attributes.get("_src").unwrap_or(&graph.nodes[u].id),"tgt":e.attributes.get("_tgt").unwrap_or(&graph.nodes[v].id),"props":props(&e.attributes)}),
        })?;
        counts.edges += 1;
    }
    Ok(counts)
}
/// Every statement [`push_to_neo4j`] sends, in order: label indexes, then upserts.
pub fn query_plan(graph: &Graph, communities: &Communities) -> Result<Vec<Statement>> {
    let mut out = neo4j_index_statements(graph)?;
    each_statement(graph, communities, |q| {
        out.push(q);
        Ok(())
    })?;
    Ok(out)
}
/// Every statement [`push_to_falkordb`] sends, in order: label indexes, then upserts.
pub fn falkordb_query_plan(graph: &Graph, communities: &Communities) -> Result<Vec<Statement>> {
    let mut out = falkordb_index_statements(graph)?;
    each_statement(graph, communities, |q| {
        out.push(q);
        Ok(())
    })?;
    Ok(out)
}
pub fn push_to_neo4j(
    graph: &Graph,
    uri: &str,
    user: &str,
    password: &str,
    communities: &Communities,
) -> Result<PushCounts> {
    // A dedicated runtime thread also works when the synchronous library is
    // called from an existing async service. RAII closes clients on every exit.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(crate::Error::from)?;
                let _entered = runtime.enter();
                let config = neo4rs::ConfigBuilder::default()
                    .uri(uri)
                    .user(user)
                    .password(password)
                    .max_connections(1)
                    .build()
                    .map_err(crate::Error::from)?;
                let client = neo4rs::Graph::connect(config).map_err(crate::Error::from)?;
                let run = |statement: &Statement| -> Result<()> {
                    let mut query = neo4rs::query(&statement.query);
                    for (key, value) in statement.params.as_object().ok_or("Expected parameters")? {
                        query = query.param(
                            key,
                            neo4rs::BoltType::try_from(value.clone())
                                .map_err(crate::Error::from)?,
                        );
                    }
                    runtime
                        .block_on(client.run(query))
                        .map(|_| ())
                        .map_err(crate::Error::from)
                };
                for statement in neo4j_index_statements(graph)? {
                    let _ = run(&statement);
                }
                each_statement(graph, communities, |statement| run(&statement))
            })
            .join()
            .map_err(|_| "Neo4j transport thread panicked".to_owned())?
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FalkorConnection {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}
pub fn falkor_connection(
    uri: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<FalkorConnection> {
    let uri = if uri.contains("://") {
        uri.to_owned()
    } else {
        format!("redis://{uri}")
    };
    let parsed = reqwest::Url::parse(&uri).map_err(crate::Error::parse)?;
    let host = parsed
        .host_str()
        .unwrap_or("localhost")
        .trim_matches(['[', ']'])
        .to_owned();
    let password = password.filter(|s| !s.is_empty());
    let username = if !parsed.username().is_empty() {
        Some(parsed.username())
    } else {
        user.filter(|_| password.is_some())
    };
    let secret = parsed.password().filter(|s| !s.is_empty()).or(password);
    Ok(FalkorConnection {
        host,
        port: parsed.port().filter(|p| *p != 0).unwrap_or(6379),
        username: username.map(str::to_owned),
        password: secret.map(str::to_owned),
    })
}
// FalkorDB SDK's MIT-licensed parameter header rules (1.7.1); strings remain
// values and map keys are quoted identifiers, never interpolated Cypher code.
pub fn stringify_param_value(value: &Value) -> Result<String> {
    Ok(match value {
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Null => "null".into(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(stringify_param_value)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        ),
        Value::Object(values) => {
            let mut parts = vec![];
            for (k, v) in values {
                if k.is_empty() {
                    return Err("Cypher map key cannot be empty".into());
                }
                if k.contains('`') {
                    return Err("Cypher map key cannot contain a backtick (FalkorDB does not support escaped backticks in identifiers)".into());
                }
                parts.push(format!("`{k}`:{}", stringify_param_value(v)?))
            }
            format!("{{{}}}", parts.join(","))
        }
        _ => string(value),
    })
}
pub fn params_header(params: &Value) -> Result<String> {
    if params.is_null() {
        return Ok(String::new());
    }
    let mut out = "CYPHER ".to_owned();
    for (k, v) in params.as_object().ok_or("params must be an object")? {
        if k.is_empty() || k.contains('`') {
            return Err(
                "Cypher parameter name must be nonempty and cannot contain a backtick".into(),
            );
        }
        out.push_str(&format!("`{k}`={} ", stringify_param_value(v)?));
    }
    Ok(out)
}
pub fn push_to_falkordb(
    graph: &Graph,
    uri: &str,
    user: Option<&str>,
    password: Option<&str>,
    communities: &Communities,
    graph_name: &str,
) -> Result<PushCounts> {
    let endpoint = falkor_connection(uri, user, password)?;
    let info = redis::ConnectionInfo {
        addr: redis::ConnectionAddr::Tcp(endpoint.host, endpoint.port),
        redis: redis::RedisConnectionInfo {
            db: 0,
            username: endpoint.username,
            password: endpoint.password,
            protocol: redis::ProtocolVersion::RESP2,
        },
    };
    let client = redis::Client::open(info).map_err(crate::Error::from)?;
    let mut connection = client
        .get_connection_with_timeout(Duration::from_secs(30))
        .map_err(crate::Error::from)?;
    connection
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(crate::Error::from)?;
    connection
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(crate::Error::from)?;
    let mut send = |statement: &Statement| -> Result<()> {
        let query = format!("{}{}", params_header(&statement.params)?, statement.query);
        redis::cmd("GRAPH.QUERY")
            .arg(graph_name)
            .arg(query)
            .arg("--compact")
            .query::<redis::Value>(&mut connection)
            .map(|_| ())
            .map_err(crate::Error::from)
    };
    for statement in falkordb_index_statements(graph)? {
        let _ = send(&statement);
    }
    each_statement(graph, communities, |statement| send(&statement))
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |key: &str| r[key].as_str().unwrap_or("");
    match s("function") {
        "falkor_connection" => {
            return Ok(json!(falkor_connection(
                s("uri"),
                r["user"].as_str(),
                r["password"].as_str()
            )?));
        }
        "params_header" => return params_header(&r["params"]).map(|s| json!(s)),
        "safe_relation" => return Ok(json!(safe_relation(s("value")))),
        "safe_label" => return Ok(json!(safe_label(s("value")))),
        _ => (),
    }
    let graph = Graph::from_node_link(&r["G"], "edges")?;
    let comm = if r["communities"].is_null() {
        vec![]
    } else {
        communities(r)?
    };
    match s("function") {
        "query_plan" => Ok(json!(query_plan(&graph, &comm)?)),
        "falkordb_query_plan" => Ok(json!(falkordb_query_plan(&graph, &comm)?)),
        "distinct_node_labels" => Ok(json!(distinct_node_labels(&graph)?)),
        "push_to_neo4j" => Ok(json!(push_to_neo4j(
            &graph,
            s("uri"),
            s("user"),
            s("password"),
            &comm
        )?)),
        "push_to_falkordb" => Ok(json!(push_to_falkordb(
            &graph,
            s("uri"),
            r["user"].as_str(),
            r["password"].as_str(),
            &comm,
            r["graph_name"].as_str().unwrap_or("graphicle")
        )?)),
        _ => Err("Unknown graph database operation".into()),
    }
}
