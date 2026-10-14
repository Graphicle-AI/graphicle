// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
//! Incremental extraction bookkeeping and conservative stale-source pruning.
use crate::{Result, build, paths};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    time::Instant,
};
/// Write `value` to `path` atomically as one line of JSON, as
/// `write_json_atomic(path, value, compact=True)` does: `json.dump` with
/// `separators=(",", ":")` and ASCII escapes. The graph.json writes of the
/// command line are compact (Graphify 0.9.86, #3413).
pub(crate) fn write_json_compact(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    paths::write_json_atomic_compact(path, value, true)
}
/// A JSON object without some of its keys, serialized in place:
/// `{k: v for k, v in data.items() if k not in keys}`.
pub(crate) struct Without<'a>(pub &'a Value, pub &'a [&'a str]);
impl serde::Serialize for Without<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let Some(object) = self.0.as_object() else {
            return self.0.serialize(s);
        };
        let mut map = s.serialize_map(None)?;
        for (key, value) in object {
            if !self.1.contains(&key.as_str()) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}
pub fn ar(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn strings(v: &Value) -> Vec<String> {
    ar(v)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}
pub fn resolve(p: &Path) -> PathBuf {
    crate::extractors::resolution::resolved(p)
}
fn npath(p: &Path) -> String {
    paths::nfc(&p.to_string_lossy())
}
pub fn stamped(
    files: &Value,
    semantic: &Value,
    root: &Path,
    partial: &[String],
    failed: &[String],
    unverified: &[String],
) -> Value {
    let res = |s: &str| resolve(&root.join(s));
    let sources = ["nodes", "hyperedges"]
        .iter()
        .flat_map(|k| ar(&semantic[*k]))
        .filter_map(|n| n["source_file"].as_str())
        .filter(|s| !s.is_empty())
        .map(res)
        .collect::<BTreeSet<_>>();
    let partial = partial
        .iter()
        .chain(unverified)
        .map(|s| res(s))
        .collect::<BTreeSet<_>>();
    let failed = failed.iter().map(|s| res(s)).collect::<BTreeSet<_>>();
    let mut out = Map::new();
    for (k, v) in files.as_object().into_iter().flatten() {
        out.insert(
            k.clone(),
            json!(
                strings(v)
                    .into_iter()
                    .filter(|s| {
                        let p = res(s);
                        !failed.contains(&p)
                            && (!["document", "paper", "image"].contains(&k.as_str())
                                || (sources.contains(&p) && !partial.contains(&p)))
                    })
                    .collect::<Vec<_>>()
            ),
        );
    }
    Value::Object(out)
}
pub fn read_graph(path: &Path) -> Result<Value> {
    crate::security::check_graph_file_size_cap(path)?;
    serde_json::from_slice(&std::fs::read(path).map_err(crate::Error::from)?)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))
        .map_err(crate::Error::from)
}
pub fn stale_sources(
    graph: &Path,
    root: &Path,
    seen: &BTreeSet<String>,
    detection: &Value,
) -> (Vec<String>, usize) {
    stale_sources_in(read_graph(graph), graph, root, seen, detection)
}
/// [`stale_sources`] given the result of reading `graph` already.
pub fn stale_sources_in(
    data: Result<Value>,
    graph: &Path,
    root: &Path,
    seen: &BTreeSet<String>,
    detection: &Value,
) -> (Vec<String>, usize) {
    let Ok(data) = data else {
        return (vec![], 0);
    };
    let root = resolve(root);
    let out = resolve(
        graph
            .parent()
            .unwrap_or(Path::new("."))
            .parent()
            .unwrap_or(Path::new(".")),
    );
    let seen = seen.iter().map(|s| paths::nfc(s)).collect::<BTreeSet<_>>();
    let basenames = seen
        .iter()
        .filter_map(|s| Path::new(s).file_name())
        .map(|s| paths::nfc(&s.to_string_lossy()))
        .collect::<BTreeSet<_>>();
    let mut exact = BTreeSet::new();
    let mut prefixes = vec![];
    for key in ["ignored", "pruned_noise_dirs"] {
        for s in strings(&detection[key]) {
            let s = paths::nfc(&s);
            if s.ends_with(std::path::MAIN_SEPARATOR) || s.ends_with('/') {
                prefixes.push(s);
            } else {
                exact.insert(s);
            }
        }
    }
    for s in strings(&detection["skipped_sensitive"]) {
        exact.insert(paths::nfc(s.split(" [").next().unwrap_or("")));
    }
    let excluded = |p: &Path| {
        let values = [npath(p), npath(&resolve(p))];
        values
            .iter()
            .any(|s| exact.contains(s) || prefixes.iter().any(|pref| s.starts_with(pref)))
            || crate::detect_types::is_sensitive(p)
    };
    let (mut checked, mut stale, mut kept) = (BTreeSet::new(), vec![], 0);
    for node in ar(&data["nodes"]) {
        let Some(sf) = node["source_file"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !checked.insert(sf) || sf.contains("://") {
            continue;
        }
        let p = Path::new(sf);
        let candidates = if p.is_absolute() {
            vec![p.into()]
        } else {
            let rel = sf.replace('\\', "/");
            let mut a = vec![crate::cache_paths::normalize(&root.join(&rel))];
            if root != out {
                a.push(crate::cache_paths::normalize(&out.join(rel)));
            }
            a
        };
        let inside = candidates
            .into_iter()
            .filter(|p| p.starts_with(&root) || resolve(p).starts_with(&root))
            .collect::<Vec<_>>();
        if inside.is_empty()
            || inside
                .iter()
                .any(|p| seen.contains(&npath(p)) || seen.contains(&npath(&resolve(p))))
        {
            continue;
        }
        let alive = inside.iter().filter(|p| p.exists()).collect::<Vec<_>>();
        if !alive.is_empty() {
            if alive.iter().all(|p| excluded(p)) {
                stale.push(sf.into());
            } else {
                kept += 1;
            }
            continue;
        }
        let rel = sf.replace('\\', "/");
        if !rel.contains('/') && basenames.contains(&paths::nfc(&rel)) {
            kept += 1;
            continue;
        }
        stale.push(sf.into());
    }
    (stale, kept)
}
pub fn zero_sources(
    graph: &Path,
    root: &Path,
    unchanged: &[String],
    semantic: bool,
) -> Vec<String> {
    let Ok(data) = read_graph(graph) else {
        return vec![];
    };
    if !data.is_object() {
        return vec![];
    }
    let root = resolve(root);
    let out = resolve(
        graph
            .parent()
            .unwrap_or(Path::new("."))
            .parent()
            .unwrap_or(Path::new(".")),
    );
    let mut present = BTreeSet::new();
    let mut items = ar(&data["nodes"]).to_vec();
    if semantic {
        items.extend_from_slice(ar(&data["hyperedges"]));
        items.extend_from_slice(ar(&data["graph"]["hyperedges"]));
    }
    for n in &items {
        let Some(sf) = n["source_file"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        present.insert(paths::nfc(sf));
        let p = Path::new(sf);
        if p.is_absolute() {
            present.insert(npath(&resolve(p)));
        } else {
            for base in [&root, &out] {
                present.insert(npath(&crate::cache_paths::normalize(
                    &base.join(sf.replace('\\', "/")),
                )));
            }
        }
    }
    unchanged
        .iter()
        .filter(|f| {
            let p = Path::new(f);
            if !semantic && crate::extract::get_extractor(p).is_none() {
                return false;
            }
            let absolute = resolve(p);
            let mut spellings = vec![npath(p), npath(&absolute)];
            if let Ok(rel) = absolute.strip_prefix(&root) {
                spellings.push(npath(rel));
            }
            !spellings.iter().any(|s| present.contains(s))
        })
        .cloned()
        .collect()
}
pub fn prune_sources(graph: &Path, stale: &[String]) -> Result<usize> {
    let Ok(mut data) = read_graph(graph) else {
        return Ok(0);
    };
    if !data.is_object() {
        return Ok(0);
    }
    let old = ar(&data["nodes"]);
    let removed = old
        .iter()
        .filter(|n| {
            n["source_file"]
                .as_str()
                .is_some_and(|s| stale.iter().any(|p| p == s))
        })
        .map(|n| n["id"].clone())
        .collect::<Vec<_>>();
    let ns = old
        .iter()
        .filter(|n| {
            n.is_object()
                && !n["source_file"]
                    .as_str()
                    .is_some_and(|s| stale.iter().any(|p| p == s))
        })
        .cloned()
        .collect::<Vec<_>>();
    let n = old.iter().filter(|n| n.is_object()).count() - ns.len();
    let key = if data.get("links").is_some() {
        "links"
    } else {
        "edges"
    };
    let es = ar(&data[key])
        .iter()
        .filter(|e| {
            e.is_object()
                && !e["source_file"]
                    .as_str()
                    .is_some_and(|s| stale.iter().any(|p| p == s))
                && !removed.contains(&e["source"])
                && !removed.contains(&e["target"])
        })
        .cloned()
        .collect::<Vec<_>>();
    let hs = ar(&data["hyperedges"])
        .iter()
        .filter(|e| {
            e.is_object()
                && !e["source_file"]
                    .as_str()
                    .is_some_and(|s| stale.iter().any(|p| p == s))
        })
        .cloned()
        .collect::<Vec<_>>();
    if n == 0 && es.len() == ar(&data[key]).len() && hs.len() == ar(&data["hyperedges"]).len() {
        return Ok(0);
    }
    data["nodes"] = json!(ns);
    data[key] = json!(es);
    if data.get("hyperedges").is_some() {
        data["hyperedges"] = json!(hs);
    }
    crate::export::backup_if_protected(
        graph.parent().unwrap_or(Path::new(".")),
        &mut build::Diagnostics::default(),
    );
    write_json_compact(graph, &data).map_err(crate::Error::from)?;
    Ok(n)
}
pub struct StageTimer {
    start: Instant,
    last: Instant,
    enabled: bool,
    cpu_start: f64,
    cpu_last: f64,
}
impl StageTimer {
    pub fn new(enabled: bool) -> Self {
        let start = Instant::now();
        let cpu = crate::profile::cpu_now();
        Self {
            start,
            last: start,
            enabled,
            cpu_start: cpu,
            cpu_last: cpu,
        }
    }
    pub fn mark(&mut self, name: &str, log: &mut Vec<String>) {
        let now = Instant::now();
        if self.enabled {
            log.push(format!(
                "[graphicle timing] {name}: {:.1}s",
                now.duration_since(self.last).as_secs_f64()
            ));
        }
        // GRAPHICLE_PROFILE=1: the same stage with its CPU time and memory.
        if crate::profile::enabled() {
            let cpu = crate::profile::cpu_now();
            crate::profile::report(
                &format!("stage.{name}"),
                now.duration_since(self.last).as_secs_f64(),
                cpu - self.cpu_last,
            );
            self.cpu_last = cpu;
        }
        self.last = now;
    }
    pub fn total(&self, log: &mut Vec<String>) {
        if self.enabled {
            log.push(format!(
                "[graphicle timing] total: {:.1}s",
                self.start.elapsed().as_secs_f64()
            ));
        }
        if crate::profile::enabled() {
            crate::profile::report(
                "stage.total",
                self.start.elapsed().as_secs_f64(),
                crate::profile::cpu_now() - self.cpu_start,
            );
        }
    }
}
