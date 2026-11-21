// Adapted from Graphify 0.9.112, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_dedup.py
//! Changes ported from Graphify 0.9.112. Cases follow the upstream tests
//! named in each comment.

#[test]
fn a_prefix_derived_word_is_not_a_variant_of_its_base() {
    // test_dedup.py (Graphify 0.9.112)
    let same_word_variant = |x: &str, y: &str| {
        graphicle::dispatch(&serde_json::json!({"operation": "dedup", "function": "_same_word_variant", "x": x, "y": y}))
            .unwrap()
            == serde_json::json!(true)
    };
    for (a, b) in [
        ("synchronous", "asynchronous"),
        ("cyclic", "acyclic"),
        ("load", "reload"),
    ] {
        assert!(!same_word_variant(a, b), "{a}/{b}");
        assert!(!same_word_variant(b, a), "{b}/{a}");
    }
    assert!(same_word_variant("handler", "handlers"));
}

#[test]
fn sql_non_ascii_table_name_makes_no_empty_id() {
    // test_sql_non_ascii_table_names.py (#5250)
    for stmt in [
        "INSERT INTO заказы_клиентов(a) VALUES (1);",
        "DELETE FROM заказы_клиентов WHERE a = 1;",
        "UPDATE заказы_клиентов z SET z.x = 'x';",
        "SELECT 1 FROM заказы_клиентов g;",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cyr.sql");
        std::fs::write(
            &path,
            format!("CREATE OR REPLACE PROCEDURE p IS\nBEGIN\n  {stmt}\nEND;\n/\n"),
        )
        .unwrap();
        let r = graphicle::extractors::sql::extract_sql(&path, None).unwrap();
        for n in r["nodes"].as_array().unwrap() {
            assert!(!n["id"].as_str().unwrap_or("").is_empty(), "{stmt}: {r:#}");
        }
        for e in r["edges"].as_array().unwrap() {
            assert!(
                !e["source"].as_str().unwrap_or("").is_empty()
                    && !e["target"].as_str().unwrap_or("").is_empty(),
                "{stmt}: {r:#}"
            );
        }
    }
}
