// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/resolver_registry.py
//! Port of graphify/resolver_registry.py: ordered, suffix-gated mutable passes.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, RwLock},
};

pub type Resolve =
    dyn Fn(&mut Vec<Value>, &mut Vec<Value>, &mut Vec<Value>) -> Result<()> + Send + Sync;
#[derive(Clone)]
pub struct LanguageResolver {
    pub name: String,
    pub suffixes: HashSet<String>,
    pub resolve: Arc<Resolve>,
}
static REGISTRY: LazyLock<RwLock<Vec<LanguageResolver>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register(resolver: LanguageResolver) -> LanguageResolver {
    REGISTRY
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(resolver.clone());
    resolver
}
pub fn registered_resolvers() -> Vec<LanguageResolver> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner()).clone()
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolverWarning {
    pub name: String,
    pub error: String,
    pub message: String,
}

/// Returned warnings supplement the original log-and-continue behavior.
pub fn run_language_resolvers(
    paths: &[PathBuf],
    per_file: &mut Vec<Value>,
    all_nodes: &mut Vec<Value>,
    all_edges: &mut Vec<Value>,
    resolvers: Option<&[LanguageResolver]>,
) -> Vec<ResolverWarning> {
    // Project-level remaps wake the resolvers of the declared language
    // (#2961): a `.inc` declared PHP runs the PHP passes, not Pascal's.
    let suffixes: HashSet<String> = paths.iter().map(crate::rcfile::effective_suffix).collect();
    let mut warnings = Vec::new();
    let mut i = 0;
    loop {
        // Release the registry lock before the callback, which may register a
        // subsequent pass. This preserves the Python list's append semantics.
        let resolver = if let Some(resolvers) = resolvers {
            resolvers.get(i).cloned()
        } else {
            REGISTRY
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(i)
                .cloned()
        };
        let Some(resolver) = resolver else {
            break;
        };
        i += 1;
        if resolver.suffixes.is_disjoint(&suffixes) {
            continue;
        }
        let started = timing().then(std::time::Instant::now);
        let profiled = crate::profile::span_with(|| format!("assemble.resolver.{}", resolver.name));
        if let Err(error) = (resolver.resolve)(per_file, all_nodes, all_edges) {
            let message = format!("{} resolution failed, skipping: {}", resolver.name, error);
            log::warn!("{message}");
            warnings.push(ResolverWarning {
                name: resolver.name.clone(),
                error: error.to_string(),
                message,
            });
        }
        if let Some(started) = started {
            eprintln!(
                "[resolver timing] {}={:.1}ms",
                resolver.name,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        drop(profiled);
    }
    warnings
}

/// `GRAPHICLE_WATCH_TIMING=1` also times each language resolver.
fn timing() -> bool {
    std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
}

/// Native scenario boundary for differential tests of arbitrary registry passes.
pub fn check_scenario(request: &Value) -> Result<Value> {
    let trace = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut resolvers = Vec::new();
    for spec in request["resolvers"]
        .as_array()
        .ok_or("Expected resolvers")?
    {
        let name = spec["name"]
            .as_str()
            .ok_or("Expected resolver name")?
            .to_owned();
        let suffixes =
            serde_json::from_value(spec["suffixes"].clone()).map_err(|_| "Expected suffix set")?;
        let spec = spec.clone();
        let trace = Arc::clone(&trace);
        let called = name.clone();
        resolvers.push(LanguageResolver {
            name,
            suffixes,
            resolve: Arc::new(move |per_file, nodes, edges| {
                trace.lock().unwrap().push(called.clone());
                for (field, target) in [
                    ("append_per_file", per_file),
                    ("append_nodes", nodes),
                    ("append_edges", edges),
                ] {
                    if let Some(items) = spec[field].as_array() {
                        target.extend(items.iter().cloned());
                    }
                }
                if let Some(error) = spec["error"].as_str() {
                    Err((error.to_owned()).into())
                } else {
                    Ok(())
                }
            }),
        });
    }
    let paths: Vec<PathBuf> =
        serde_json::from_value(request["paths"].clone()).map_err(|_| "Expected paths")?;
    let list = |name: &str| request[name].as_array().cloned().unwrap_or_default();
    let (mut per_file, mut nodes, mut edges) = (list("per_file"), list("nodes"), list("edges"));
    let warnings = run_language_resolvers(
        &paths,
        &mut per_file,
        &mut nodes,
        &mut edges,
        Some(&resolvers),
    );
    let trace = trace.lock().unwrap().clone();
    Ok(json!({"trace":trace,"per_file":per_file,"nodes":nodes,"edges":edges,"warnings":warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordered_suffix_gating_preserves_mutations_after_failure() {
        let result=check_scenario(&json!({"paths":["a.rb"],"resolvers":[
            {"name":"skipped","suffixes":[".go"],"append_edges":[{"bad":true}]},
            {"name":"failing","suffixes":[".rb"],"append_nodes":[{"id":"n"}],"error":"boom"},
            {"name":"later","suffixes":[".rb"],"append_edges":[{"source":"x","target":"y","relation":"calls"}]}
        ]})).unwrap();
        assert_eq!(result["trace"], json!(["failing", "later"]));
        assert_eq!(result["nodes"], json!([{"id":"n"}]));
        assert_eq!(
            result["edges"],
            json!([{"source":"x","target":"y","relation":"calls"}])
        );
        assert_eq!(
            result["warnings"][0]["message"],
            "failing resolution failed, skipping: boom"
        );
    }
    #[test]
    fn register_returns_original_and_snapshots_are_independent() {
        let before = registered_resolvers().len();
        let value = register(LanguageResolver {
            name: "test_registration".into(),
            suffixes: HashSet::new(),
            resolve: Arc::new(|_, _, _| Ok(())),
        });
        let mut snapshot = registered_resolvers();
        assert_eq!(snapshot.len(), before + 1);
        assert_eq!(snapshot.last().unwrap().name, value.name);
        snapshot.clear();
        assert_eq!(registered_resolvers().len(), before + 1);
    }
}
