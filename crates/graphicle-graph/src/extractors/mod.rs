// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/__init__.py
//! Native extractor modules; unported languages are listed in the migration ledger.
pub mod apex;
pub mod base;
pub mod bash;
pub mod blade;
mod builtins_data;
pub mod cobol;
pub mod commonlisp;
pub mod csharp;
pub mod dart;
pub mod dm;
pub mod elixir;
pub mod engine;
mod engine_calls;
pub mod engine_config;
mod engine_declarations;
pub mod engine_declarator;
mod engine_extra;
pub mod engine_parser;
mod engine_ruby;
pub mod engine_scope;
pub mod engine_semantics;
pub mod engine_types;
pub mod erlang;
/// Opt-in `--precise` evidence levels on call edges.
pub mod evidence;
pub mod fortran;
pub mod go;
pub mod json_config;
pub mod julia;
pub mod markdown;
mod markdown_data;
pub mod models;
pub mod objc;
pub mod ocaml;
pub mod parse_cache;
pub mod pascal;
pub mod pascal_forms;
pub mod pascal_paths;
pub mod powershell;
/// Opt-in `--precise` Dart import resolution (upstream #2329).
pub mod precise_dart;
/// Opt-in `--precise` JavaScript/TypeScript call resolution (oxc).
#[cfg(feature = "precise-js")]
pub mod precise_js;
/// Opt-in `--precise` Python call resolution (points-to analysis).
pub mod precise_python;
/// Opt-in `--precise` call edges from SCIP indexes.
pub mod precise_scip;
pub mod r;
pub mod razor;
pub mod receiver_bindings;
pub mod receiver_resolution;
mod registry;
pub mod test_identities;
pub use registry::{Extractor, LANGUAGE_EXTRACTORS, extract, registered_languages};
pub mod resolution;
pub mod resolution_facts;
pub mod resolution_js;
pub mod resolution_paths;
pub mod resolution_types;
pub mod robot;
/// Opt-in (`--routes`) HTTP route nodes and request edges.
pub mod routes;
/// Opt-in (`--docs`) reStructuredText pages.
pub mod rst;
pub mod rust;
pub mod sln;
pub mod solidity;
pub mod sql;
pub mod terraform;
pub mod vbnet;
pub mod verilog;
pub mod yaml_config;
pub(crate) mod yaml_frontmatter;
pub mod zig;
