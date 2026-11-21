// Adapted from Graphify 0.9.108, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_sql_comma_joins_and_dblink.py
//! Extraction changes ported from Graphify 0.9.108. Cases follow the
//! upstream tests named in each comment.
use serde_json::Value;
use std::collections::BTreeSet;

fn sql_edges(name: &str, text: &str) -> (BTreeSet<(String, String, String)>, BTreeSet<String>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    let r = graphicle::extractors::sql::extract_sql(&path, None).unwrap();
    let label = |id: &Value| {
        r["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["label"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| id.as_str().unwrap_or("").to_string())
    };
    let edges = r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] != "contains")
        .map(|e| {
            (
                label(&e["source"]),
                e["relation"].as_str().unwrap().to_string(),
                label(&e["target"]),
            )
        })
        .collect();
    let labels = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect();
    (edges, labels)
}

fn has(edges: &BTreeSet<(String, String, String)>, s: &str, r: &str, t: &str) -> bool {
    edges.contains(&(s.to_string(), r.to_string(), t.to_string()))
}

#[test]
fn sql_comma_join_reads_every_table() {
    // test_issue_5169_repro_comma_join (#5169)
    let (edges, _) = sql_edges(
        "t3c.sql",
        "CREATE OR REPLACE PROCEDURE p3c IS\nBEGIN\n  INSERT INTO hr.t_out SELECT a.id FROM hr.emp a, hr.dept d WHERE a.d = d.id;\nEND;\n/\n",
    );
    assert!(has(&edges, "p3c()", "reads_from", "hr.emp"), "{edges:?}");
    assert!(has(&edges, "p3c()", "reads_from", "hr.dept"), "{edges:?}");
}

#[test]
fn sql_dblink_suffix_is_kept() {
    // test_issue_5169_repro_dblink (#5169)
    let (edges, labels) = sql_edges(
        "t3a.sql",
        "CREATE OR REPLACE PROCEDURE p3a IS\nBEGIN\n  INSERT INTO hr.emp_copy SELECT * FROM hr.emp@remote_db;\nEND;\n/\n",
    );
    assert!(labels.contains("hr.emp@remote_db"), "{labels:?}");
    assert!(
        has(&edges, "p3a()", "reads_from", "hr.emp@remote_db"),
        "{edges:?}"
    );
}

#[test]
fn sql_unparsed_derived_table_alias_is_not_read_as_a_table() {
    // test_multilang.py (#5147)
    let (edges, labels) = sql_edges(
        "schema.sql",
        "CREATE TABLE items (val INT);\nCREATE VIEW v AS\n  SELECT AVG(val) FROM (SELECT val FROM items\n  LIMIT 2 - MOD((SELECT COUNT(*) FROM items), 2)\n  OFFSET 1) q2;\n",
    );
    assert!(!labels.contains("q2"), "{labels:?}");
    assert!(has(&edges, "v", "reads_from", "items"), "{edges:?}");
}

#[test]
fn sql_edition_keyword_matches_plain_ddl() {
    // test_sql_oracle_editionable.py (#5167)
    let templates = [
        (
            "CREATE OR REPLACE {kw}PROCEDURE hr.load_emp IS\nBEGIN\n  INSERT INTO hr.emp_copy SELECT * FROM hr.emp;\nEND;\n/\n",
            "hr.load_emp()",
        ),
        (
            "CREATE OR REPLACE {kw}FUNCTION hr.f RETURN NUMBER IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM hr.emp;\n  RETURN n;\nEND;\n/\n",
            "hr.f()",
        ),
        (
            "CREATE OR REPLACE {kw}TRIGGER hr.trg_emp BEFORE INSERT ON hr.emp FOR EACH ROW\nBEGIN\n  INSERT INTO hr.audit_log (id) VALUES (:new.id);\nEND;\n/\n",
            "hr.trg_emp",
        ),
    ];
    for kw in ["EDITIONABLE ", "NONEDITIONABLE ", "editionable "] {
        for (template, label) in templates {
            let (_, labels) = sql_edges("edition.sql", &template.replace("{kw}", kw));
            assert!(labels.contains(label), "{kw}{label}: {labels:?}");
        }
    }
    let (_, labels) = sql_edges(
        "t.sql",
        "CREATE TABLE editionable (id INT);\nSELECT editionable FROM editionable;\n",
    );
    assert!(!labels.iter().any(|l| l.ends_with("()")), "{labels:?}");
}
