//! Evidence levels on call edges (opt-in, with `--precise`).
//!
//! Every `calls` edge gets `evidence`, the strongest way it was found, so an
//! agent can trade recall for precision (`--min-evidence`):
//!
//! | Level | Meaning | From |
//! |---|---|---|
//! | `name` | matched by name | Graphify's own call resolution |
//! | `binding` | a resolved binding: import, scope or points-to flow | `points_to`, `oxc_binding`, `dart_import`, receiver `module_attr` |
//! | `type` | the receiver's type or the class hierarchy | `oxc_type`, `super_call`, receiver `constructor`, `annotation`, `self_method` |
//! | `indexer` | a compiler-grade SCIP index has the call | `scip` edges, and edges a SCIP index confirms |
//! | `runtime` | the call was seen while the code ran | a trace named by `GRAPHICLE_TRACE` |
//!
//! Levels only go up, and an edge that already has a level keeps it
//! (incremental builds reuse cached edges). Default output, without
//! `--precise`, has no `evidence` attribute.
use crate::extractors::resolution::source_key;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Evidence levels, weakest first.
pub const LEVELS: [&str; 5] = ["name", "binding", "type", "indexer", "runtime"];

/// `--min-evidence` / `GRAPHICLE_MIN_EVIDENCE`: loaded graphs leave out call
/// edges below this level (0, `name`, keeps everything).
pub static MIN_LEVEL: AtomicUsize = AtomicUsize::new(0);

/// The `--min-evidence` level in force.
pub fn min_level() -> usize {
    MIN_LEVEL.load(Ordering::Relaxed)
}

/// Removes `calls` edges below [`min_level`] from node-link edges.
pub fn filter_links(links: &mut Vec<Value>) {
    let min = min_level();
    if min > 0 {
        links.retain(|e| s(e, "relation") != "calls" || level(e) >= min);
    }
}

/// (caller id, callee id).
pub type Pairs = FxHashSet<(String, String)>;

/// Position of `level` in [`LEVELS`].
pub fn rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The level implied by how an edge was found.
fn found(e: &Value) -> usize {
    match s(e, "resolution") {
        "scip" => 3,
        "oxc_type" | "super_call" => 2,
        "points_to" | "oxc_binding" | "dart_import" => 1,
        _ => match s(e, "receiver_evidence") {
            "constructor" | "annotation" | "self_method" => 2,
            "module_attr" => 1,
            _ => 0,
        },
    }
}

/// An edge's level: its recorded `evidence`, else how it was found. Edges
/// without either (default Graphify output) count as `name`.
pub fn level(e: &Value) -> usize {
    rank(s(e, "evidence")).unwrap_or(0).max(found(e))
}

/// Sets `evidence` on every `calls` edge. Pairs in `indexer` or `runtime`
/// raise their edges to that level.
pub fn annotate(edges: &mut [Value], indexer: &Pairs, runtime: &Pairs) {
    for e in edges.iter_mut() {
        if s(e, "relation") != "calls" {
            continue;
        }
        let pair = (s(e, "source").to_string(), s(e, "target").to_string());
        let mut level = level(e);
        if runtime.contains(&pair) {
            level = 4;
        } else if indexer.contains(&pair) {
            level = level.max(3);
        }
        e["evidence"] = json!(LEVELS[level]);
    }
}

/// Trace files to read: the `GRAPHICLE_TRACE` path list.
pub fn trace_paths() -> Vec<std::path::PathBuf> {
    std::env::var_os("GRAPHICLE_TRACE")
        .map(|v| std::env::split_paths(&v).filter(|p| p.is_file()).collect())
        .unwrap_or_default()
}

/// Graph nodes by source file and bare name, with their lines.
fn by_name<'g>(
    nodes: &[&'g Value],
    root: &Path,
) -> FxHashMap<(String, &'g str), Vec<(usize, &'g str)>> {
    let mut out: FxHashMap<(String, &'g str), Vec<(usize, &'g str)>> = FxHashMap::default();
    let mut keys: FxHashMap<&str, String> = FxHashMap::default();
    for n in nodes {
        let (id, label, file) = (s(n, "id"), s(n, "label"), s(n, "source_file"));
        let Some(line) = s(n, "source_location")
            .strip_prefix('L')
            .and_then(|l| l.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|l| l.parse::<usize>().ok())
        else {
            continue;
        };
        if id.is_empty() || file.is_empty() || label.is_empty() {
            continue;
        }
        let bare = label.strip_prefix('.').unwrap_or(label);
        let bare = bare.strip_suffix("()").unwrap_or(bare);
        let key = keys
            .entry(file)
            .or_insert_with(|| source_key(file, root))
            .clone();
        out.entry((key, bare)).or_default().push((line, id));
    }
    out
}

/// Caller/callee pairs seen at runtime, from `traces` (see [`trace_paths`]):
/// JSON arrays of `[[file, qualname, first_line], [file, qualname,
/// first_line]]` (files relative to the scan root), as written by
/// `bench/accuracy/trace_calls.py`. A traced function matches the node of
/// its file and name whose line is within 3 lines before or 8 after the
/// traced first line (decorators come first).
pub fn runtime_pairs(nodes: &[&Value], root: &Path, traces: &[std::path::PathBuf]) -> Pairs {
    let mut out = Pairs::default();
    if traces.is_empty() {
        return out;
    }
    let index = by_name(nodes, root);
    let node = |f: &str, q: &str, line: usize| -> Option<&str> {
        let name = q.rsplit('.').next().unwrap_or(q);
        let key = source_key(&root.join(f).to_string_lossy(), root);
        let candidates = index.get(&(key, name))?;
        let in_range = |l: usize| l >= line.saturating_sub(3) && l <= line.saturating_add(8);
        let distance = candidates
            .iter()
            .filter(|(l, _)| in_range(*l))
            .map(|(l, _)| l.abs_diff(line))
            .min()?;
        let mut closest = candidates
            .iter()
            .filter(|(l, _)| in_range(*l) && l.abs_diff(line) == distance);
        let (_, target) = closest.next()?;
        closest.all(|(_, id)| id == target).then_some(*target)
    };
    for path in traces {
        let Ok(text) = std::fs::read_to_string(path) else {
            log::warn!("trace not readable: {}", path.display());
            continue;
        };
        let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(&text) else {
            log::warn!("trace is not a JSON array: {}", path.display());
            continue;
        };
        for row in rows {
            let end = |i: usize| -> Option<(&str, &str, usize)> {
                let e = row.get(i)?;
                Some((
                    e.get(0)?.as_str()?,
                    e.get(1)?.as_str()?,
                    usize::try_from(e.get(2)?.as_u64()?).ok()?,
                ))
            };
            let (Some((cf, cq, cl)), Some((tf, tq, tl))) = (end(0), end(1)) else {
                continue;
            };
            if let (Some(a), Some(b)) = (node(cf, cq, cl), node(tf, tq, tl))
                && a != b
            {
                out.insert((a.to_string(), b.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_and_extreme_trace_locations_do_not_confirm_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let module = root.join("main.py");
        std::fs::write(&module, "").unwrap();
        let file = module.to_string_lossy();
        let trace = root.join("trace.json");
        let nodes = [
            json!({"id":"caller", "label":"caller()", "source_file":file, "source_location":"L1"}),
            json!({"id":"left", "label":"run()", "source_file":file, "source_location":"L4"}),
            json!({"id":"right", "label":"run()", "source_file":file, "source_location":"L6"}),
        ];
        std::fs::write(
            &trace,
            serde_json::to_vec(&json!([
                [["main.py", "caller", 1], ["main.py", "run", 5]],
                [["main.py", "caller", u64::MAX], ["main.py", "run", 6]],
                [["main.py", "caller", 1], ["main.py", "run", 6]],
            ]))
            .unwrap(),
        )
        .unwrap();
        let pairs = runtime_pairs(&nodes.iter().collect::<Vec<_>>(), &root, &[trace]);
        assert_eq!(pairs, Pairs::from_iter([("caller".into(), "right".into())]));
    }

    #[test]
    fn levels_come_from_resolution_and_confirmations() {
        let mut edges = vec![
            json!({"source": "a", "target": "b", "relation": "calls"}),
            json!({"source": "a", "target": "c", "relation": "calls", "resolution": "points_to"}),
            json!({"source": "a", "target": "d", "relation": "calls", "receiver_evidence": "constructor"}),
            json!({"source": "a", "target": "e", "relation": "calls", "resolution": "scip"}),
            json!({"source": "b", "target": "c", "relation": "calls"}),
            json!({"source": "c", "target": "d", "relation": "calls", "resolution": "oxc_type"}),
            json!({"source": "a", "target": "b", "relation": "contains"}),
            json!({"source": "d", "target": "e", "relation": "calls", "evidence": "type"}),
        ];
        let indexer: Pairs = [("b".into(), "c".into())].into_iter().collect();
        let runtime: Pairs = [("c".into(), "d".into())].into_iter().collect();
        annotate(&mut edges, &indexer, &runtime);
        let got = edges
            .iter()
            .map(|e| e.get("evidence").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            got,
            [
                Some("name"),
                Some("binding"),
                Some("type"),
                Some("indexer"),
                Some("indexer"),
                Some("runtime"),
                None,
                Some("type"),
            ]
        );
        assert_eq!(rank("indexer"), Some(3));
        assert_eq!(rank("nope"), None);
    }

    #[test]
    fn traces_match_nodes_by_file_name_and_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let module = root.join("m.py");
        std::fs::write(&module, "").unwrap();
        let file = module.to_string_lossy().into_owned();
        let nodes = [
            json!({"id": "m_f", "label": "f()", "source_file": file, "source_location": "L3"}),
            json!({"id": "m_g", "label": "g()", "source_file": file, "source_location": "L10"}),
            json!({"id": "m_c_h", "label": ".h()", "source_file": file, "source_location": "L20"}),
        ];
        let trace = root.join("calls.json");
        // `g` is decorated: its code object starts two lines before `def`.
        std::fs::write(
            &trace,
            r#"[[["m.py","f",3],["m.py","g",8]],[["m.py","g",8],["m.py","C.h",20]],[["m.py","f",3],["other.py","x",1]]]"#,
        )
        .unwrap();
        let refs = nodes.iter().collect::<Vec<_>>();
        let pairs = runtime_pairs(&refs, &root, &[trace]);
        let mut pairs = pairs.into_iter().collect::<Vec<_>>();
        pairs.sort();
        assert_eq!(
            pairs,
            [
                ("m_f".to_string(), "m_g".to_string()),
                ("m_g".to_string(), "m_c_h".to_string())
            ]
        );
    }
}
