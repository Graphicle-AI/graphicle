// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__init__.py, graphify/__main__.py
//! Graphicle graph engine: code and document knowledge graphs. It ports a
//! pinned Graphify release (see NOTICE.md); migration coverage is explicit.
use serde_json::{Map, Value, json};
pub mod error;
pub mod model;
pub mod protocol;
pub use error::{Error, ErrorKind};
pub mod affected;
pub mod analyze;
mod analyze_data;
pub mod api;
pub mod archived;
pub mod benchmark;
pub mod build;
mod build_api;
mod build_global;
mod build_labels;
mod build_merge;
mod build_normalize;
pub mod cache;
pub mod cache_api;
mod cache_paths;
mod callflow_data;
pub mod callflow_html;
pub mod cargo_introspect;
pub mod cli;
mod cli_cluster;
mod cli_commands;
mod cli_export;
mod cli_extract;
mod cli_guard;
mod cli_merge;
mod cli_state;
pub mod cluster;
pub mod cross_repo_calls;
pub mod cross_repo_types;
pub mod csharp_dispatch;
mod decimal_data;
pub mod dedup;
mod dedup_api;
mod dedup_strings;
pub mod detect;
mod detect_documents;
mod detect_ignore;
mod detect_manifest;
mod detect_types;
pub mod diagnostics;
pub mod diagnostics_report;
pub mod export;
mod export_api;
mod export_formats;
mod export_obsidian;
pub mod exporters;
pub mod extract;
pub mod extract_languages;
pub mod extract_members;
pub mod extract_merge;
pub mod extract_normalize;
pub mod extract_projects;
pub mod extract_python;
pub mod extractors;
pub mod file_slice;
mod fsutil;
pub mod global_graph;
pub mod google_workspace;
/// Long-tail grammars loaded from shared-library packs in slim builds.
pub mod grammar_packs;
pub mod graph;
mod graph_adjacency;
mod graph_algorithms;
mod graph_node_link;
pub mod hooks;
mod hooks_runtime;
mod html_compat;
mod hyperedge_members;
pub mod ids;
pub mod incremental;
pub mod ingest;
mod ingest_api;
pub mod install;
mod install_fs;
mod install_platforms;
pub mod interface_dispatch;
mod io_compat;
mod ip_policy_data;
pub mod llm;
mod llm_api;
#[cfg(feature = "bedrock")]
mod llm_bedrock;
mod llm_config;
mod llm_extract;
mod llm_labels;
mod llm_parse;
mod llm_source;
mod llm_transport;
pub mod manifest;
pub mod manifest_ingest;
pub mod markdown_resolution;
pub mod mcp_ingest;
pub mod minhash;
pub mod multigraph_compat;
pub mod par;
pub mod pascal_resolution;
mod path_syntax;
pub mod paths;
pub mod pg_introspect;
mod process;
pub mod profile;
pub mod prs;
mod prs_api;
mod prs_io;
pub mod prs_render;
mod prs_triage;
// Internal helpers shared with first-party extension crates. They are not
// part of the documented API and may change without notice.
#[doc(hidden)]
pub mod python_compat;
mod python_datetime;
mod python_json;
mod python_repr_data;
pub mod query_terms;
pub mod querylog;
pub mod rcfile;
pub mod record;
pub mod reflect;
mod regex_compat;
pub mod report;
pub mod resolver_registry;
/// Opt-in agent retrieval (`retrieve`, `retrieve_context`).
pub mod retrieve;
pub mod ruby_resolution;
pub mod scip_ingest;
pub mod security;
pub mod semantic_cleanup;
pub mod serve;
mod serve_api;
mod serve_find;
pub mod serve_http;
/// Opt-in `serve --watch`: resident graph updated in memory on file changes.
pub mod serve_live;
mod serve_prs;
// Internal search/traversal stages shared with first-party extension crates;
// not part of the documented API.
#[doc(hidden)]
pub mod serve_search;
pub mod serve_socket;
#[doc(hidden)]
pub mod serve_text;
pub mod session;
/// Saved query snapshots of built graphs and search indexes.
pub mod snapshot;
pub mod swift_dispatch;
pub mod symbol_resolution;
pub mod transcribe;
mod transcribe_api;
mod transcribe_native;
pub mod tree_html;
mod unicode15;
pub mod validate;
pub mod vtab;
pub mod watch;
mod watch_paths;
mod watch_reconcile;
pub mod wiki;
#[cfg(windows)]
mod win;

#[cfg(test)]
mod hardening_tests;

// The original package exposes these operations at its top-level API.
// Native linking replaces its lazy Python import mechanism.
pub use analyze::{god_nodes, suggest_questions, surprising_connections};
pub use build::build_from_json;
pub use cluster::{cluster, cohesion_score, score_all};
pub use export::{to_canvas, to_html, to_json, to_svg};
pub use extract::{collect_files, extract};
pub use ingest::save_query_result;
pub use reflect::reflect;
pub use report::generate;
pub use wiki::to_wiki;

/// This package's version: `--version`, the MCP server version and the
/// stamp beside installed skills.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UPSTREAM_COMMIT: &str = "10130e79ed9caf8f60d998073e097531eaa1a395";
pub const UPSTREAM_VERSION: &str = "0.9.91";
pub type Result<T> = std::result::Result<T, Error>;

pub fn dispatch(request: &Value) -> Result<Value> {
    if !request.is_object() {
        return Err(Error::invalid("Expected a request object"));
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch_inner(request)))
        .unwrap_or_else(|_| {
            Err(Error::Internal(
                "Native operation failed; no result accepted".into(),
            ))
        })
}

/// Dispatch an owned request, moving graph payloads into their native storage.
/// The borrowed entry point remains available to callers retaining their JSON.
pub fn dispatch_owned(mut request: Value) -> Result<Value> {
    if !request.is_object() {
        return Err(Error::invalid("Expected a request object"));
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || match request["operation"].as_str() {
            Some(operation) if streamed_edges_key(operation).is_some() => {
                let edges_key = streamed_edges_key(operation).unwrap_or("links");
                let data = request
                    .get_mut("graph")
                    .map(Value::take)
                    .unwrap_or(Value::Null);
                let source = match session::referenced(&request, !data.is_null())? {
                    Some(handle) => GraphSource::Shared(handle),
                    None => {
                        GraphSource::owned(graph::Graph::from_node_link_owned(data, edges_key)?)
                    }
                };
                run_graph_operation(&request, edges_key, source)
            }
            Some("graph.open") => {
                let data = request.get_mut("graph").map(Value::take);
                let loaded = match data {
                    Some(Value::Null) | None => None,
                    Some(data) => Some(graph::Graph::from_node_link_owned(
                        data,
                        session::edges_key(&request)?,
                    )?),
                };
                session::dispatch(&request, loaded)
            }
            Some(operation) if operation.starts_with("analyze.") => {
                analyze::dispatch_owned(request)
            }
            _ => dispatch_inner(&request),
        },
    ))
    .unwrap_or_else(|_| {
        Err(Error::Internal(
            "Native operation failed; no result accepted".into(),
        ))
    })
}

/// Dispatch one JSON request straight from its bytes.
///
/// Requests whose operation consumes a node-link `graph` payload stream that
/// payload directly into native graph storage instead of building and then
/// dismantling a dynamic JSON tree. Everything else, including malformed
/// input, takes the ordinary parse-then-dispatch path so behaviour and error
/// text are unchanged.
pub fn dispatch_slice(bytes: &[u8]) -> Result<Value> {
    if let Some(result) = dispatch_streamed(bytes) {
        return result;
    }
    serde_json::from_slice(bytes)
        .map_err(Error::from)
        .and_then(dispatch_owned)
}

/// Edge array field used by the graph payload of an operation that can take a
/// pre-built graph, or `None` when the operation must see the dynamic value.
fn streamed_edges_key(operation: &str) -> Option<&'static str> {
    match operation {
        "affected.resolve" | "affected.hits" | "affected.format" | "affected.prefer"
        | "affected.label" => Some("links"),
        "cluster.run" | "cluster.partition" | "cluster.native" | "cluster.split"
        | "cluster.cohesion" | "cluster.labels" | "cluster.scores" => Some("edges"),
        _ => None,
    }
}

fn dispatch_streamed(bytes: &[u8]) -> Option<Result<Value>> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let StreamedRequest {
        mut fields,
        graph,
        streamed_with,
    } = serde::Deserialize::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    let operation = fields.get("operation").and_then(Value::as_str);
    if operation == Some("graph.open") {
        // The edge array name is a sibling field, so the payload was kept raw
        // and is streamed now that the whole request has been seen.
        let request = Value::Object(fields);
        let edges_key = match session::edges_key(&request) {
            Ok(edges_key) => edges_key,
            Err(error) => return Some(Err(error)),
        };
        let loaded = match graph {
            GraphSlot::Missing => None,
            GraphSlot::Raw(raw) => match graph::Graph::from_node_link_json(raw.get(), edges_key) {
                Some(graph) => Some(graph),
                None => return None,
            },
            GraphSlot::Dynamic(_) | GraphSlot::Streamed(_) => return None,
        };
        return Some(session::dispatch(&request, loaded));
    }
    let Some(edges_key) = operation.and_then(streamed_edges_key) else {
        // Not a graph-consuming operation: the request is already the value
        // the dynamic path would have parsed, so hand it over without rework.
        match graph {
            GraphSlot::Missing => {}
            GraphSlot::Dynamic(value) => {
                fields.insert("graph".into(), value);
            }
            GraphSlot::Raw(raw) => {
                fields.insert("graph".into(), serde_json::from_str(raw.get()).ok()?);
            }
            GraphSlot::Streamed(_) => return None,
        }
        return Some(dispatch_owned(Value::Object(fields)));
    };
    let has_payload = !matches!(graph, GraphSlot::Missing);
    let request = Value::Object(fields);
    let source = match session::referenced(&request, has_payload) {
        Err(error) => return Some(Err(error)),
        Ok(Some(handle)) => Ok(GraphSource::Shared(handle)),
        Ok(None) => match graph {
            GraphSlot::Streamed(graph) if streamed_with == Some(edges_key) => {
                Ok(GraphSource::owned(graph))
            }
            GraphSlot::Streamed(_) => return None,
            GraphSlot::Raw(raw) => Ok(GraphSource::owned(graph::Graph::from_node_link_json(
                raw.get(),
                edges_key,
            )?)),
            GraphSlot::Dynamic(value) => {
                graph::Graph::from_node_link_owned(value, edges_key).map(GraphSource::owned)
            }
            GraphSlot::Missing => {
                graph::Graph::from_node_link_owned(Value::Null, edges_key).map(GraphSource::owned)
            }
        },
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_graph_operation(&request, edges_key, source?)
    }));
    Some(outcome.unwrap_or_else(|_| {
        Err(Error::Internal(
            "Native operation failed; no result accepted".into(),
        ))
    }))
}

/// A graph a request runs against: its own freshly loaded payload, or a
/// resident graph shared through a [`session::GraphHandle`].
enum GraphSource {
    Owned(Box<graph::Graph>),
    Shared(session::GraphHandle),
}

impl GraphSource {
    fn owned(graph: graph::Graph) -> Self {
        Self::Owned(Box::new(graph))
    }
}

/// Borrowed-request entry: load the inline payload or resolve `graph_ref`.
fn run_borrowed_graph_operation(request: &Value, edges_key: &'static str) -> Result<Value> {
    let payload = request.get("graph").filter(|graph| !graph.is_null());
    let source = match session::referenced(request, payload.is_some())? {
        Some(handle) => GraphSource::Shared(handle),
        None => GraphSource::owned(graph::Graph::from_node_link(
            payload.unwrap_or(&Value::Null),
            edges_key,
        )?),
    };
    run_graph_operation(request, edges_key, source)
}

/// Run one `affected.*` (`edges_key == "links"`) or `cluster.*` request.
///
/// Owned graphs are released off-thread afterwards. Shared graphs are never
/// mutated: a request that asks for mutations works on its own copy.
fn run_graph_operation(
    request: &Value,
    edges_key: &'static str,
    source: GraphSource,
) -> Result<Value> {
    if edges_key != "links" {
        let result = match &source {
            GraphSource::Owned(graph) => dispatch_cluster_graph(request, graph),
            GraphSource::Shared(handle) => dispatch_cluster_graph(request, handle),
        };
        if let GraphSource::Owned(graph) = source {
            release_graph(*graph);
        }
        return result;
    }
    match source {
        GraphSource::Owned(mut graph) => {
            let result = affected::dispatch_graph(request, &mut graph);
            release_graph(*graph);
            result
        }
        GraphSource::Shared(handle) if affected::mutates(request) => {
            let mut graph = (*handle).clone();
            let result = affected::dispatch_graph(request, &mut graph);
            release_graph(graph);
            result
        }
        GraphSource::Shared(handle) => affected::query_graph(request, &handle),
    }
}

/// Free a large graph off the request thread.
///
/// Tearing down tens of thousands of attribute maps is a measurable share of
/// a graph request, and nothing in the response depends on it. A single
/// helper thread takes at most a couple of graphs at a time; when it is busy
/// the graph is dropped inline, so memory stays bounded either way.
fn release_graph(graph: graph::Graph) {
    use std::sync::{LazyLock, Mutex, mpsc};
    static REAPER: LazyLock<Option<Mutex<mpsc::SyncSender<graph::Graph>>>> = LazyLock::new(|| {
        let (sender, receiver) = mpsc::sync_channel::<graph::Graph>(2);
        std::thread::Builder::new()
            .name("graphicle-release".into())
            .spawn(move || {
                for graph in receiver {
                    drop(graph);
                }
            })
            .ok()?;
        Some(Mutex::new(sender))
    });
    if graph.node_count() < 4096 {
        return;
    }
    if let Some(sender) = REAPER.as_ref()
        && let Ok(sender) = sender.lock()
        && let Err(mpsc::TrySendError::Full(graph) | mpsc::TrySendError::Disconnected(graph)) =
            sender.try_send(graph)
    {
        drop(graph);
    }
}

/// Where a request's `graph` payload ended up during the single streaming pass.
enum GraphSlot<'de> {
    Missing,
    /// Seen before the operation was known; parsed once the operation is.
    Raw(&'de serde_json::value::RawValue),
    /// Operation known and not graph-consuming: parsed like any other field.
    Dynamic(Value),
    /// Operation known and graph-consuming: loaded straight into a graph.
    Streamed(graph::Graph),
}

/// A request parsed in one pass. `fields` holds every top-level field except
/// the graph payload with the same last-wins, first-position semantics as the
/// dynamic parser; `graph` is left as a `Null` placeholder there, mirroring the
/// dynamic path which takes the payload out before dispatching.
struct StreamedRequest<'de> {
    fields: Map<String, Value>,
    graph: GraphSlot<'de>,
    streamed_with: Option<&'static str>,
}

/// How much the streaming visitor knows about the request's operation when a
/// `graph` field arrives.
#[derive(Clone, Copy)]
enum Operation {
    Unknown,
    /// Graph-consuming with a fixed edge array name.
    Streamed(&'static str),
    /// Graph-consuming, edge array name given by another field.
    Deferred,
    /// Not graph-consuming.
    Dynamic,
}

impl<'de> serde::Deserialize<'de> for StreamedRequest<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct RequestVisitor;

        impl<'de> serde::de::Visitor<'de> for RequestVisitor {
            type Value = StreamedRequest<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a request object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = Map::new();
                let mut graph = GraphSlot::Missing;
                let mut streamed_with = None;
                let mut operation = Operation::Unknown;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "graph" {
                        graph =
                            match operation {
                                // Also `graph.open`: its edge array name is a
                                // sibling field, so the payload waits as raw text.
                                Operation::Unknown | Operation::Deferred => {
                                    GraphSlot::Raw(map.next_value()?)
                                }
                                Operation::Streamed(edges_key) => {
                                    streamed_with = Some(edges_key);
                                    GraphSlot::Streamed(map.next_value_seed(
                                        graph_node_link::NodeLinkSeed { edges_key },
                                    )?)
                                }
                                Operation::Dynamic => GraphSlot::Dynamic(map.next_value()?),
                            };
                        fields.insert(key, Value::Null);
                        continue;
                    }
                    let value: Value = map.next_value()?;
                    if key == "operation" {
                        operation = match value.as_str() {
                            Some("graph.open") => Operation::Deferred,
                            Some(name) => streamed_edges_key(name)
                                .map_or(Operation::Dynamic, Operation::Streamed),
                            None => Operation::Dynamic,
                        };
                    }
                    fields.insert(key, value);
                }
                Ok(StreamedRequest {
                    fields,
                    graph,
                    streamed_with,
                })
            }
        }

        deserializer.deserialize_map(RequestVisitor)
    }
}

fn dispatch_inner(request: &Value) -> Result<Value> {
    match request.get("operation").and_then(Value::as_str) {
        Some("resolution.js") => extractors::resolution_js::dispatch(request),
        Some("resolution") => extractors::resolution::dispatch(request),
        Some("extract") => extract::dispatch(request),
        Some("watch") => watch::dispatch(request),
        Some("serve") => serve_api::dispatch(request),
        Some("hooks") => hooks::dispatch(request),
        Some("install") => install::dispatch(request),
        Some("cli") => cli::dispatch(request),
        Some("extract.project") => extract_projects::dispatch(request),
        Some("extract.language") => extract_languages::dispatch(request),
        Some("engine") => extractors::engine::dispatch(request),
        Some("ingest") => ingest_api::dispatch(request),
        Some("transcribe") => transcribe_api::dispatch(request),
        Some("prs") => prs_api::dispatch(request),
        Some("benchmark") => benchmark::dispatch(request),
        Some("diagnostics") => diagnostics::dispatch(request),
        Some("llm") => llm_api::dispatch(request),
        Some("robot") => extractors::robot::dispatch(request),
        Some("export") => export_api::dispatch(request),
        Some("exporters.base") => Ok(json!(exporters::base::COMMUNITY_COLORS)),
        Some("exporters.html") => exporters::html::dispatch(request),
        Some("exporters.graphdb") => exporters::graphdb::dispatch(request),
        Some("build") => build_api::dispatch(request),
        Some("dedup") => dedup_api::dispatch(request),
        Some("manifest") => manifest::dispatch(request),
        Some("file_slice") => file_slice::dispatch(request),
        Some("detect") => detect::dispatch(request),
        Some("rcfile") => rcfile::dispatch(request),
        Some("google_workspace") => google_workspace::dispatch(request),
        Some("models") => extractors::models::dispatch(request),
        Some("extractors.registry") => Ok(json!(extractors::registered_languages())),
        Some("extractors.extract") => extractors::extract(
            request["language"].as_str().ok_or("Expected language")?,
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        ),
        Some("cache") => cache_api::dispatch(request),
        Some("callflow") => callflow_html::dispatch(request),
        Some("extract.dart") => extractors::dart::extract_dart(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("extract.sql") => {
            let content = match request.get("content") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.as_bytes().to_vec()),
                Some(Value::Array(values)) => Some(
                    values
                        .iter()
                        .map(|v| {
                            v.as_u64()
                                .and_then(|n| u8::try_from(n).ok())
                                .ok_or_else(|| crate::Error::invalid("Expected SQL source bytes"))
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                _ => return Err("Expected SQL source string or byte array".into()),
            };
            extractors::sql::extract_sql(
                std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
                content.as_deref(),
            )
        }
        Some("sql.scan") => Ok(json!(extractors::sql::scan_sql(
            request["text"].as_str().ok_or("Expected text")?
        ))),
        Some("sql.mask") => Ok(json!(
            extractors::sql::scan_sql(request["text"].as_str().ok_or("Expected text")?).0
        )),
        Some("sql.norm") => Ok(json!(extractors::sql::norm_ident(
            request["text"].as_str().ok_or("Expected text")?
        ))),
        Some("scip.ingest") => Ok(scip_ingest::ingest_scip_json(
            &request["doc"],
            request.get("source_file").unwrap_or(&json!("")),
            request.get("language").unwrap_or(&json!("python")),
        )),
        Some("scip.node_id") => Ok(json!(scip_ingest::make_scip_node_id(
            request["symbol"].as_str().ok_or("Expected symbol")?,
            request["source_file"]
                .as_str()
                .ok_or("Expected source_file")?
        ))),
        Some("scip.metadata") => Ok(scip_ingest::build_scip_metadata(
            request["symbol"].as_str().ok_or("Expected symbol")?,
            request["kind"].as_str().ok_or("Expected kind")?,
            request["description"]
                .as_str()
                .ok_or("Expected description")?,
        )),
        Some("scip.kind") => Ok(json!(scip_ingest::scip_kind_to_file_type(
            request["kind"].as_str().ok_or("Expected kind")?
        ))),
        Some("scip.first_line") => Ok(scip_ingest::first_occurrence_line(
            request.get("occurrences"),
        )),
        Some("scip.relation") => Ok(json!(scip_ingest::relation_for(&request["relationship"]))),
        Some("scip.coerce") => Ok(json!(scip_ingest::coerce_str(
            request.get("value"),
            request.get("default")
        ))),
        Some("scip.is_true") => Ok(json!(scip_ingest::is_true(request.get("value")))),
        Some("scip.resolve") => {
            let per_doc = request["per_doc"]
                .as_array()
                .ok_or("Expected per_doc entries")?
                .iter()
                .map(|r| {
                    Ok((
                        (
                            r[0].as_str().ok_or("Expected symbol")?.to_string(),
                            r[1].as_str().ok_or("Expected path")?.to_string(),
                        ),
                        r[2].as_str().ok_or("Expected ID")?.to_string(),
                    ))
                })
                .collect::<Result<scip_ingest::PerDocumentIndex>>()?;
            let global = request["global"]
                .as_object()
                .ok_or("Expected global index")?
                .iter()
                .map(|(k, v)| {
                    Ok((
                        k.clone(),
                        v.as_array()
                            .ok_or("Expected ID array")?
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_string)
                                    .ok_or_else(|| crate::Error::invalid("Expected ID"))
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ))
                })
                .collect::<Result<scip_ingest::GlobalIndex>>()?;
            Ok(json!(scip_ingest::resolve_relationship_target(
                request["target"].as_str().ok_or("Expected target")?,
                request["source_file"]
                    .as_str()
                    .ok_or("Expected source file")?,
                &per_doc,
                &global
            )))
        }
        Some("mcp_config.extract") => Ok(mcp_ingest::extract_mcp_config(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("mcp_config.is_path") => Ok(json!(mcp_ingest::is_mcp_config_path(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?)
        ))),
        Some("mcp_config.filenames") => Ok(json!(mcp_ingest::MCP_CONFIG_FILENAMES)),
        Some("mcp_config.package") => Ok(json!(mcp_ingest::detect_package_from_args(
            request["args"].as_array().ok_or("Expected args")?
        ))),
        Some("mcp_config.strip_version") => Ok(json!(mcp_ingest::strip_version(
            request["package"].as_str().ok_or("Expected package")?
        ))),
        Some("manifest.extract") => manifest_ingest::extract_package_manifest(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        ),
        Some("manifest.is_path") => Ok(json!(manifest_ingest::is_package_manifest_path(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?)
        ))),
        Some("manifest.names") => Ok(json!(
            manifest_ingest::PACKAGE_MANIFEST_NAMES
                .iter()
                .map(|(name, ecosystem)| (name.to_string(), json!(ecosystem)))
                .collect::<serde_json::Map<_, _>>()
        )),
        Some("manifest.parse") => manifest_ingest::parse_manifest(
            request["ecosystem"].as_str().ok_or("Expected ecosystem")?,
            request["text"].as_str().ok_or("Expected text")?,
        ),
        Some("manifest.coerce_deps") => Ok(json!(manifest_ingest::coerce_deps(&request["value"]))),
        Some("manifest.pep508_name") => Ok(json!(manifest_ingest::pep508_name(
            request["spec"].as_str().ok_or("Expected spec")?
        ))),
        Some("manifest.pkg_id") => Ok(json!(manifest_ingest::pkg_id(
            request["name"].as_str().ok_or("Expected name")?
        ))),
        Some(
            "cluster.run" | "cluster.partition" | "cluster.native" | "cluster.split"
            | "cluster.cohesion" | "cluster.labels" | "cluster.scores",
        ) => run_borrowed_graph_operation(request, "edges"),
        Some("cluster.sigs") => Ok(cluster::community_member_sigs(
            &cluster::communities_from_json(&request["communities"])?,
        )),
        Some("cluster.remap") => {
            let previous: cluster::Partition =
                serde_json::from_value(request["previous"].clone()).map_err(crate::Error::from)?;
            Ok(cluster::communities_to_json(
                &cluster::remap_communities_to_previous(
                    &cluster::communities_from_json(&request["communities"])?,
                    &previous,
                )?,
            ))
        }
        Some("cluster.label_still_applies") => {
            // previous: {node id: community} before re-clustering (#2298).
            let previous: std::collections::BTreeMap<String, i64> =
                serde_json::from_value(request["previous"].clone()).map_err(crate::Error::from)?;
            Ok(json!(cluster::label_still_applies(
                request["cid"].as_i64().ok_or("Expected cid")?,
                request["members"].as_array().ok_or("Expected members")?,
                &previous,
                request["saved_sig"].as_str(),
                request["current_sig"].as_str(),
            )))
        }
        Some(operation) if operation.starts_with("analyze.") => analyze::dispatch(request),
        Some(operation) if operation.starts_with("reflect.") => reflect::dispatch(request),
        Some(operation) if operation.starts_with("report.") => report::dispatch(request),
        Some(operation) if operation.starts_with("pascal.") => {
            extractors::pascal::dispatch(request)
        }
        Some(operation) if operation.starts_with("tree.") => tree_html::dispatch(request),
        Some(operation) if operation.starts_with("wiki.") => wiki::dispatch(request),
        Some(operation) if operation.starts_with("postgres.") => pg_introspect::dispatch(request),
        Some("info") => Ok(json!({
            "implementation": "rust",
            "build_features": {
                "bedrock": cfg!(feature = "bedrock"),
                "chinese": cfg!(feature = "chinese"),
                "transcription": cfg!(feature = "transcription"),
                "postgres": cfg!(feature = "postgres")
            },
            // Attribution: the pinned release this engine ports (NOTICE).
            "based_on": {
                "upstream_commit": UPSTREAM_COMMIT,
                "upstream_version": UPSTREAM_VERSION,
                "complete_port": true,
                "application_modules": 95
            }
        })),
        Some("ids") => {
            let values = request["values"]
                .as_array()
                .ok_or("Expected values array")?;
            let normalized: Vec<String> = values
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(ids::normalize_id)
                        .ok_or_else(|| crate::Error::invalid("ID must be a string"))
                })
                .collect::<Result<_>>()?;
            let parts = request.get("parts").and_then(Value::as_array);
            let mut made = Vec::new();
            for parts in parts.into_iter().flatten() {
                let parts: Vec<&str> = parts
                    .as_array()
                    .ok_or("Expected ID parts array")?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| crate::Error::invalid("ID parts must be strings"))
                    })
                    .collect::<Result<_>>()?;
                made.push(ids::make_id(parts));
            }
            Ok(json!({"normalized":normalized,"made":made}))
        }
        Some("validate") => {
            let errors =
                validate::validate_extraction(request.get("data").unwrap_or(&Value::Null))?;
            Ok(json!({"valid":errors.is_empty(),"errors":errors}))
        }
        Some("assert_valid") => {
            validate::assert_valid(request.get("data").unwrap_or(&Value::Null))?;
            Ok(Value::Null)
        }
        Some("minhash") => {
            let num_perm = request
                .get("num_perm")
                .and_then(Value::as_u64)
                .unwrap_or(128) as usize;
            let mut sketch = minhash::MinHash::new(num_perm);
            for chunk in request
                .get("chunks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let bytes: Vec<u8> =
                    serde_json::from_value(chunk.clone()).map_err(|_| "Expected byte array")?;
                sketch.update(&bytes);
            }
            let coefficients = minhash::coefficients(num_perm);
            Ok(json!({"hashvalues":sketch.hashvalues,"a":coefficients.0,"b":coefficients.1}))
        }
        Some("minhash.params") => {
            let threshold = request
                .get("threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.5);
            let num_perm = request
                .get("num_perm")
                .and_then(Value::as_u64)
                .unwrap_or(128) as usize;
            Ok(json!(minhash::optimal_lsh_params(threshold, num_perm)))
        }
        Some("minhash.lsh") => {
            let threshold = request
                .get("threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.5);
            let num_perm = request
                .get("num_perm")
                .and_then(Value::as_u64)
                .unwrap_or(128) as usize;
            let mut lsh = minhash::MinHashLsh::new(threshold, num_perm);
            let mut errors = Vec::new();
            for entry in request
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let key = entry["key"].as_str().ok_or("Expected LSH key")?;
                let values: Vec<u64> = serde_json::from_value(entry["hashvalues"].clone())
                    .map_err(|_| "Expected hash values")?;
                let mut sketch = minhash::MinHash::new(values.len());
                sketch.hashvalues = values;
                if let Err(error) = lsh.insert(key, &sketch) {
                    errors.push(error);
                }
            }
            let mut results = Vec::new();
            for values in request
                .get("queries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let values: Vec<u64> =
                    serde_json::from_value(values.clone()).map_err(|_| "Expected hash values")?;
                let mut sketch = minhash::MinHash::new(values.len());
                sketch.hashvalues = values;
                results.push(lsh.query(&sketch));
            }
            Ok(json!({"b":lsh.b,"r":lsh.r,"errors":errors,"results":results}))
        }
        Some("cargo.introspect") => cargo_introspect::introspect_cargo(std::path::Path::new(
            request["root"].as_str().ok_or("Expected root path")?,
        )),
        Some("extractor.base") => {
            let paths = request
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|path| {
                    path.as_str()
                        .map(|path| extractors::base::file_stem(std::path::Path::new(path)))
                        .ok_or_else(|| crate::Error::invalid("Expected path string"))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut texts = Vec::new();
            for span in request
                .get("spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let bytes: Vec<u8> = serde_json::from_value(span["source"].clone())
                    .map_err(|_| "Expected byte array")?;
                let start = span["start"].as_u64().ok_or("Expected start offset")? as usize;
                let end = span["end"].as_u64().ok_or("Expected end offset")? as usize;
                texts.push(extractors::base::read_text_range(start, end, &bytes));
            }
            Ok(
                json!({"stems":paths,"texts":texts,"builtins":extractors::base::LANGUAGE_BUILTIN_GLOBALS}),
            )
        }
        Some("extract.sln") => Ok(extractors::sln::extract_sln(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some(operation) if operation.starts_with("csharp.") => {
            extractors::csharp::dispatch(request)
        }
        Some("extract.blade") => Ok(extractors::blade::extract_blade(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.lazarus_form") => Ok(extractors::pascal_forms::extract_lazarus_form(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        )),
        Some("extract.delphi_form") => Ok(extractors::pascal_forms::extract_delphi_form(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        )),
        Some("extract.json") => Ok(extractors::json_config::extract_json(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("security.ip") => {
            let ips = request["ips"].as_array().ok_or("Expected IP array")?;
            Ok(Value::Array(
                ips.iter()
                    .map(|ip| {
                        ip.as_str()
                            .ok_or_else(|| crate::Error::invalid("Expected IP string"))?
                            .parse()
                            .map(|ip| Value::Bool(security::ip_is_blocked(ip)))
                            .map_err(|error| crate::Error::invalid(format!("Invalid IP: {error}")))
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        Some("security.label") => Ok(json!(security::sanitize_label(&request["value"]))),
        Some("security.out_name") => Ok(json!(security::out_dir_name(
            request["value"].as_str().ok_or("Expected output path")?
        ))),
        Some("security.metadata_string") => {
            Ok(json!(security::sanitize_metadata_string(&request["value"])))
        }
        Some("security.metadata_value") => Ok(security::sanitize_metadata_value(&request["value"])),
        Some("security.metadata") => Ok(json!(security::sanitize_metadata(request.get("value")))),
        Some("security.graph_cap") => Ok(json!(security::max_graph_file_bytes_from(
            request["raw"].as_str().unwrap_or(""),
            request["default"]
                .as_u64()
                .unwrap_or(security::MAX_GRAPH_FILE_BYTES)
        ))),
        Some("security.check_graph_size") => {
            security::check_graph_file_size_cap_with(
                std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
                request["cap"]
                    .as_u64()
                    .unwrap_or_else(security::max_graph_file_bytes),
            )?;
            Ok(Value::Null)
        }
        Some("security.graph_path") => {
            let output = request["output"].as_str().unwrap_or(&security::OUT_DIR);
            let output_name = security::out_dir_name(output);
            let path = security::validate_graph_path_with(
                std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
                request["base"].as_str().map(std::path::Path::new),
                output,
                request["output_name"].as_str().unwrap_or(&output_name),
            )?;
            Ok(json!(path))
        }
        Some("security.validate_url") => Ok(json!(security::validate_url(
            request["url"].as_str().ok_or("Expected URL")?
        )?)),
        Some("security.fetch") => Ok(json!(security::safe_fetch(
            request["url"].as_str().ok_or("Expected URL")?,
            request["max_bytes"]
                .as_u64()
                .unwrap_or(security::MAX_FETCH_BYTES as u64) as usize,
            std::time::Duration::from_secs(request["timeout"].as_u64().unwrap_or(30))
        )?)),
        Some("extract.razor") => Ok(extractors::razor::extract_razor(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.zig") => Ok(extractors::zig::extract_zig(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.cobol") => Ok(extractors::cobol::extract_cobol(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.r") => Ok(extractors::r::extract_r(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.solidity") => Ok(extractors::solidity::extract_solidity(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        )),
        Some("extract.vbnet") => Ok(extractors::vbnet::extract_vbnet(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.erlang") => extractors::erlang::extract_erlang(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some(
            operation @ ("r.resolve" | "solidity.resolve" | "vbnet.resolve" | "erlang.resolve"),
        ) => {
            // The corpus resolvers mutate edges in place, as upstream's do.
            let per = request["per_file"].as_array().cloned().unwrap_or_default();
            let nodes = request["nodes"].as_array().cloned().unwrap_or_default();
            let mut edges = request["edges"].as_array().cloned().unwrap_or_default();
            match operation {
                "r.resolve" => extractors::r::resolve_r_sourced_calls(&per, &nodes, &mut edges),
                "solidity.resolve" => {
                    extractors::solidity::resolve_solidity_type_references(&per, &nodes, &mut edges)
                }
                "vbnet.resolve" => {
                    extractors::vbnet::resolve_vbnet_partial_calls(&per, &nodes, &mut edges)
                }
                _ => extractors::erlang::resolve_erlang_remote_calls(&per, &nodes, &mut edges),
            }
            Ok(json!({"edges":edges}))
        }
        Some("extract.apex") => extractors::apex::extract_apex(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("extract.elixir") => Ok(extractors::elixir::extract_elixir(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.julia") => Ok(extractors::julia::extract_julia(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.commonlisp") => Ok(extractors::commonlisp::extract_commonlisp(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        )),
        Some("extract.dm") => extractors::dm::extract_dm(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("extract.dmi") => extractors::dm::extract_dmi(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("extract.dmm") => Ok(extractors::dm::extract_dmm(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.dmf") => Ok(extractors::dm::extract_dmf(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some(operation) if operation.starts_with("dm.") => extractors::dm::dispatch(request),
        Some("extract.terraform") => extractors::terraform::extract_terraform(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        ),
        Some(operation) if operation.starts_with("terraform.") => {
            extractors::terraform::dispatch(request)
        }
        Some("extract.objc") => extractors::objc::extract_objc(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("objc.category_stem") => Ok(json!(extractors::objc::category_base_stem(
            request["stem"].as_str().ok_or("Expected stem")?
        ))),
        Some("resolution.c_include") => Ok(json!(
            extractors::resolution_paths::resolve_c_include_path(
                request["raw"].as_str().ok_or("Expected raw")?,
                request["source"].as_str().ok_or("Expected source")?
            )
            .map_err(crate::Error::from)?
        )),
        Some("resolution.clear_paths") => {
            extractors::resolution_paths::clear_paths();
            Ok(Value::Null)
        }
        Some("extract.rust") => Ok(extractors::rust::extract_rust(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.go") => Ok(extractors::go::extract_go(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.ocaml") => Ok(extractors::ocaml::extract_ocaml(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.powershell") => Ok(extractors::powershell::extract_powershell(
            std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
        )),
        Some("extract.powershell_manifest") => {
            Ok(extractors::powershell::extract_powershell_manifest(
                std::path::Path::new(request["path"].as_str().ok_or("Expected path")?),
            ))
        }
        Some("powershell.module_name") => Ok(json!(extractors::powershell::psd1_module_name(
            request["raw"].as_str().ok_or("Expected raw")?
        ))),
        Some("extract.bash") => extractors::bash::extract_bash(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("extract.fortran") => Ok(extractors::fortran::extract_fortran(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        ))),
        Some("extract.verilog") => extractors::verilog::extract_verilog(std::path::Path::new(
            request["path"].as_str().ok_or("Expected path")?,
        )),
        Some("verilog.strip_comments") => Ok(json!(extractors::verilog::strip_comments(
            request["text"].as_str().ok_or("Expected text")?
        ))),
        Some("verilog.split_types") => Ok(json!(extractors::verilog::split_type_list(
            request["text"].as_str().ok_or("Expected text")?
        ))),
        Some("verilog.type_refs") => {
            let skip: std::collections::HashSet<String> =
                serde_json::from_value(request.get("skip").cloned().unwrap_or(json!([])))
                    .map_err(crate::Error::from)?;
            Ok(json!(extractors::verilog::collect_type_refs(
                request["text"].as_str().ok_or("Expected text")?,
                request["generic"].as_bool().unwrap_or(false),
                &skip
            )))
        }
        Some("fortran.cpp_arguments") => Ok(json!(
            extractors::fortran::cpp_arguments(std::path::Path::new(
                request["path"].as_str().ok_or("Expected path")?
            ))
            .map_err(crate::Error::from)?
        )),
        Some("fortran.preprocess") => Ok(json!(
            extractors::fortran::cpp_preprocess(std::path::Path::new(
                request["path"].as_str().ok_or("Expected path")?
            ))
            .map_err(crate::Error::from)?
        )),
        Some("bash.suffix") => Ok(json!(extractors::bash::bash_source_suffix(
            request["raw"].as_str().ok_or("Expected raw path")?,
            request["allow_dotdot"].as_bool().unwrap_or(false)
        ))),
        Some("bash.within") => Ok(json!(extractors::bash::within_tree(
            std::path::Path::new(request["ceiling"].as_str().ok_or("Expected ceiling")?),
            std::path::Path::new(request["target"].as_str().ok_or("Expected target")?)
        ))),
        Some("bash.assignment_base") => Ok(json!(
            extractors::bash::bash_assignment_base(
                request["value"]
                    .as_str()
                    .ok_or("Expected assignment value")?,
                std::path::Path::new(request["directory"].as_str().ok_or("Expected directory")?)
            )
            .as_deref()
            .map(fsutil::path_string)
        )),
        Some("engine.reference_edge") => extractors::engine_semantics::semantic_reference_edge(
            request["source"].as_str().ok_or("Expected source")?,
            request["target"].as_str().ok_or("Expected target")?,
            request["context"].as_str().ok_or("Expected context")?,
            request["source_file"]
                .as_str()
                .ok_or("Expected source file")?,
            &request["line"],
        ),
        Some("engine.location") => Ok(extractors::engine_semantics::source_location(
            &request["line"],
        )),
        Some("engine.namespace_id") => {
            Ok(json!(extractors::engine_semantics::csharp_namespace_id(
                request["name"].as_str().ok_or("Expected name")?
            )))
        }
        Some("extract.markdown") => {
            let _root = extractors::markdown::enter_scan_root(
                request["scan_root"].as_str().map(std::path::Path::new),
            );
            extractors::markdown::extract_markdown(std::path::Path::new(
                request["path"].as_str().ok_or("Expected path")?,
            ))
        }
        Some("markdown.mention") => Ok(json!(extractors::markdown::code_span_mention(
            request["span"].as_str().ok_or("Expected code span")?
        ))),
        Some("markdown.clear_cache") => {
            extractors::markdown::clear_link_index_cache();
            Ok(Value::Null)
        }
        Some("markdown.resolve") => {
            let _root = extractors::markdown::enter_scan_root(
                request["scan_root"].as_str().map(std::path::Path::new),
            );
            Ok(json!(extractors::markdown::resolve_markdown_link(
                request["raw"].as_str().ok_or("Expected link")?,
                std::path::Path::new(request["dir"].as_str().ok_or("Expected source directory")?),
                request["wikilink"].as_bool().unwrap_or(false)
            )?))
        }
        Some("markdown.frontmatter") => {
            let lines: Vec<String> = serde_json::from_value(request["lines"].clone())
                .map_err(|_| "Expected frontmatter lines")?;
            let lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
            let fm = if request["fallback"].as_bool().unwrap_or(false) {
                extractors::markdown::parse_frontmatter_fallback(&lines)
            } else {
                extractors::markdown::parse_frontmatter(&lines)
            };
            Ok(json!(security::sanitize_metadata(Some(&Value::Object(fm)))))
        }
        Some("markdown.match_cited_file") => {
            let sources: std::collections::HashSet<String> =
                serde_json::from_value(request["source_files"].clone())
                    .map_err(|_| "Expected source files")?;
            Ok(json!(markdown_resolution::match_cited_file(
                request["cited"].as_str().ok_or("Expected cited file")?,
                request["doc_file"]
                    .as_str()
                    .ok_or("Expected document file")?,
                &sources
            )))
        }
        Some("markdown.resolve_mentions") => {
            let per_file = request["per_file"]
                .as_array()
                .ok_or("Expected extraction results")?;
            let nodes = request["nodes"].as_array().ok_or("Expected nodes")?;
            let mut edges = request["edges"].as_array().ok_or("Expected edges")?.clone();
            markdown_resolution::resolve_markdown_mentions(per_file, nodes, &mut edges);
            Ok(json!({"nodes":nodes,"edges":edges}))
        }
        Some("resolvers.check_scenario") => resolver_registry::check_scenario(request),
        Some(operation) if operation.starts_with("querylog.") => querylog::dispatch(request),
        Some(operation) if operation.starts_with("global.") => global_graph::dispatch(request),
        Some(operation) if operation.starts_with("ruby.") => ruby_resolution::dispatch(request),
        Some(operation) if operation.starts_with("affected.") => affected::dispatch(request),
        Some(operation) if operation.starts_with("csharp_dispatch.") => {
            csharp_dispatch::dispatch(request)
        }
        Some(operation) if operation.starts_with("interface_dispatch.") => {
            interface_dispatch::dispatch(request)
        }
        Some(operation) if operation.starts_with("swift_dispatch.") => {
            swift_dispatch::dispatch(request)
        }
        Some(operation) if operation.starts_with("paths.") => paths::dispatch(request),
        Some(operation) if operation.starts_with("pascal_resolution.") => {
            pascal_resolution::dispatch(request)
        }
        Some(operation) if operation.starts_with("symbols.") => {
            symbol_resolution::dispatch(request)
        }
        Some(operation) if operation.starts_with("semantic.") => {
            semantic_cleanup::dispatch(request)
        }
        Some("graph.check_scenario") => graph::check_scenario(request),
        Some("graph.link_shared_types") => cross_repo_types::dispatch(request),
        Some(operation) if operation.starts_with("cross_calls.") => {
            cross_repo_calls::dispatch(request)
        }
        Some("graph.capabilities") => {
            Ok(multigraph_compat::require_multigraph_capabilities()?.json())
        }
        Some("graph.open" | "graph.close" | "graph.list") => session::dispatch(request, None),
        Some("graph.capability_message") => {
            let result: multigraph_compat::MultigraphCapabilityResult =
                serde_json::from_value(request["result"].clone()).map_err(crate::Error::from)?;
            Ok(result.json())
        }
        _ => Err(Error::UnsupportedOperation(
            "Unsupported operation; no Python fallback was executed".into(),
        )),
    }
}

fn dispatch_cluster_graph(request: &Value, graph: &graph::Graph) -> Result<Value> {
    let resolution = request
        .get("resolution")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let backend = request
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    match request["operation"].as_str().unwrap() {
        "cluster.run" => Ok(cluster::communities_to_json(&cluster::cluster_with(
            graph,
            resolution,
            request
                .get("exclude_hubs_percentile")
                .and_then(Value::as_f64),
            backend,
            request
                .get("ambiguous_scale")
                .and_then(Value::as_f64)
                .unwrap_or(cluster::DEFAULT_AMBIGUOUS_SCALE),
        )?)),
        "cluster.partition" => Ok(json!(cluster::partition(graph, resolution, backend)?)),
        "cluster.native" => Ok(json!(cluster::native_leiden(graph, resolution)?)),
        "cluster.split" => Ok(json!(cluster::split_community(
            graph,
            request["nodes"].as_array().ok_or("Expected nodes")?,
            backend
        )?)),
        "cluster.cohesion" => Ok(json!(cluster::cohesion_score(
            graph,
            request["nodes"].as_array().ok_or("Expected nodes")?
        ))),
        "cluster.labels" => Ok(cluster::label_communities_by_hub(
            graph,
            &cluster::communities_from_json(&request["communities"])?,
        )),
        "cluster.scores" => Ok(cluster::score_all(
            graph,
            &cluster::communities_from_json(&request["communities"])?,
        )),
        _ => unreachable!(),
    }
}
