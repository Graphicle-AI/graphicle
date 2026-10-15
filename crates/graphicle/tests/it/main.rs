//! The runtime's integration tests, compiled into one test binary (`it`)
//! instead of one statically linked executable per file. The files moved here
//! from `tests/` as modules of this crate (their `include_str!` paths now
//! start one folder further up), and every test keeps its name under its
//! module: `cargo test --test it workflows::` runs one file's tests.
//!
//! Two suites stay binaries of their own, because they change state the
//! whole process shares: `tests/extension.rs` installs a probe extension in
//! the process-wide registry, and `tests/embedding_cache.rs` sets
//! `GRAPHICLE_EMBEDDING_CACHE` for the process.

/// Installs the edition under test before a fixture is built: nothing in
/// Graphicle Core. An extension crate can compile these suites into its own
/// test binary, whose `edition_setup` installs its extensions; the suites
/// call `crate::edition_setup()` and `crate::fork_guard`.
pub fn edition_setup() {}

/// Child processes versus tests that rely on flock exclusivity.
mod fork_guard;

/// The fixture-home environment of the command-line suites' children.
mod child_env;

mod archived;
mod ask;
mod async_requests;
mod auto_observe;
mod backends;
mod cache_review;
mod capture;
mod code_relationships;
mod core_limits;
mod cross_repo;
mod deletion;
mod doc_links;
mod easy_flow;
mod easy_review;
mod easy_setup;
mod embedding_batch;
mod embedding_workflow;
mod evidence_windows;
mod expansion;
mod fault_isolation;
mod forgetting;
mod freshness;
mod graph_always;
mod graph_command;
mod graph_history;
mod graph_projection;
mod grounded_v2;
mod grounded_v3;
mod invalidation;
mod jobs_bounds;
mod lean_replies;
mod local_inference;
mod memory_handoffs;
mod memory_symbols;
mod observer;
mod one_server;
mod permissions_refresh;
mod product_workspace;
mod project_identity;
mod provider_http_hosts;
mod query_code_graph_arm;
mod query_graph_source;
mod query_variants;
mod receipts;
mod release_0_4;
mod rerank_depth;
mod retention;
mod retrieval_defaults;
mod revocation;
mod scale_bounds;
mod selective_integration;
mod service_gaps;
mod store_compat;
mod structured_memory;
mod temporal_graph;
mod ux_terminal;
mod workflows;
mod workspace_scope;
