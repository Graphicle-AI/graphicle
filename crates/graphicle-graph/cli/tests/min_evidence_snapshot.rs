//! `--min-evidence` (beyond the upstream port, opt-in) reads query snapshots of its
//! own: the filtered graph is a different built graph, so a filtered load
//! and an unfiltered one never share a snapshot, and answers equal those of
//! a load straight from graph.json.
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn graph_cli(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn filtered_loads_have_their_own_snapshots_and_the_same_answers() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    std::fs::create_dir_all(out.join("cache")).unwrap();
    let node = |id: &str, label: &str| {
        json!({"id": id, "label": label, "source_file": format!("{id}.py"),
               "source_location": "L1", "file_type": "code", "community": 0})
    };
    let call = |target: &str, evidence: &str| {
        json!({"source": "alpha", "target": target, "relation": "calls",
               "confidence": "EXTRACTED", "evidence": evidence, "weight": 1.0,
               "source_file": "alpha.py", "source_location": "L2"})
    };
    let graph = json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": [node("alpha", "alpha()"), node("beta", "beta()"), node("gamma", "gamma()")],
        "links": [call("beta", "name"), call("gamma", "type")],
    });
    std::fs::write(out.join("graph.json"), graph.to_string()).unwrap();
    let explain = |level: Option<&str>, snapshots: bool| {
        let mut args = vec!["explain", "alpha()", "--graph", ".graphicle/graph.json"];
        if let Some(level) = level {
            args.extend(["--min-evidence", level]);
        }
        let mut env = vec![("GRAPHICLE_QUERY_TIMING", "1")];
        if !snapshots {
            env.push(("GRAPHICLE_QUERY_SNAPSHOT", "0"));
        }
        graph_cli(dir.path(), &args, &env)
    };
    let direct_all = explain(None, false).0;
    let direct_typed = explain(Some("type"), false).0;
    assert!(
        direct_all.contains("beta") && direct_all.contains("gamma"),
        "{direct_all}"
    );
    assert!(
        !direct_typed.contains("beta") && direct_typed.contains("gamma"),
        "{direct_typed}"
    );
    for level in [None, Some("type")] {
        let (first, first_err) = explain(level, true);
        let (second, second_err) = explain(level, true);
        assert!(!first_err.contains("snapshot load"), "{first_err}");
        assert!(second_err.contains("snapshot load"), "{second_err}");
        let direct = if level.is_some() {
            &direct_typed
        } else {
            &direct_all
        };
        assert_eq!((&first, &second), (direct, direct));
    }
    let snaps = std::fs::read_dir(out.join("cache/query"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(snaps.contains("graph.json.explain.snap"), "{snaps:?}");
    assert!(snaps.contains("graph.json.explain-e2.snap"), "{snaps:?}");
}
