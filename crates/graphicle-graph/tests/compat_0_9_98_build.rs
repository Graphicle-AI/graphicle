// Adapted from Graphify 0.9.98 (d5895eae), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_csharp_method_owner_labels.py
//! C# method nodes display their owning type (Graphify 0.9.98, #4883): the
//! build, the --no-cluster writers' node-list form, the method-ghost and
//! old-stem alias indexes and the analyze method-stub predicate. Extraction
//! keeps the bare `.Name()` form (compat_0_9_98_extract.rs).
use graphicle::api::{ExtractOptions, extract};
use graphicle::build::{self, Diagnostics};
use graphicle::extractors::csharp::{bare_csharp_method_label, csharp_method_label_reassignments};
use graphicle::graph::Graph;
use serde_json::{Value, json};
use std::path::PathBuf;

const ISSUE_REPRO: &str = "public interface IReport { string Build(); }\n\
public class PdfReport : IReport { public string Build() { return \"pdf\"; } }\n\
public class CsvReport : IReport { public string Build() { return \"csv\"; } }\n\
public class Exporter {\n\
  private readonly IReport _report;\n\
  public Exporter(IReport report) { _report = report; }\n\
  public string Run() { return _report.Build(); }\n\
}\n";

fn extract_files(files: &[(&str, &str)]) -> (tempfile::TempDir, Value) {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(rel, text)| {
            let p = root.join(rel);
            std::fs::write(&p, text).unwrap();
            p
        })
        .collect();
    let out = extract(
        &paths,
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    (dir, out)
}

fn build_graph(mut extraction: Value) -> Graph {
    let mut d = Diagnostics::default();
    build::build_from_json(&mut extraction, false, None, &mut d).unwrap()
}

fn label(g: &Graph, id: &str) -> Option<Value> {
    let i = g.node_index(&json!(id))?;
    g.nodes()[i].attributes.get("label").cloned()
}

#[test]
fn issue_repro_methods_display_their_owning_type() {
    let (_dir, extraction) = extract_files(&[("Report.cs", ISSUE_REPRO)]);
    // Extraction keeps the bare resolver label.
    assert!(
        extraction["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "report_pdfreport_build" && n["label"] == ".Build()"),
        "{extraction:#}"
    );
    let g = build_graph(extraction.clone());
    assert_eq!(
        label(&g, "report_pdfreport_build"),
        Some(json!("PdfReport.Build()"))
    );
    assert_eq!(
        label(&g, "report_csvreport_build"),
        Some(json!("CsvReport.Build()"))
    );
    assert_eq!(
        label(&g, "report_ireport_build"),
        Some(json!("IReport.Build()"))
    );
    assert_eq!(
        label(&g, "report_exporter_run"),
        Some(json!("Exporter.Run()"))
    );
    assert_eq!(label(&g, "report_ireport"), Some(json!("IReport")));
    assert_eq!(label(&g, "report"), Some(json!("Report.cs")));
    // The --no-cluster form gives the same labels.
    let mut nodes = extraction["nodes"].as_array().unwrap().clone();
    build::qualify_csharp_method_labels_in_nodes(
        &mut nodes,
        extraction["edges"].as_array().unwrap(),
    );
    let raw = |id: &str| {
        nodes
            .iter()
            .find(|n| n["id"] == id)
            .map(|n| n["label"].clone())
    };
    assert_eq!(
        raw("report_pdfreport_build"),
        Some(json!("PdfReport.Build()"))
    );
    assert_eq!(raw("report_exporter_run"), Some(json!("Exporter.Run()")));
}

#[test]
fn ids_and_edges_match_the_raw_extraction() {
    let (_dir, extraction) = extract_files(&[("Report.cs", ISSUE_REPRO)]);
    let g = build_graph(extraction.clone());
    let mut built: Vec<String> = g
        .nodes()
        .iter()
        .map(|n| n.id.as_str().unwrap().to_owned())
        .collect();
    built.sort();
    let mut raw: Vec<String> = extraction["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    raw.sort();
    raw.dedup();
    assert_eq!(built, raw);
    assert_eq!(
        g.number_of_edges(
            &json!("report_exporter_run"),
            &json!("report_ireport_build")
        ),
        1
    );
}

#[test]
fn rebuilding_from_qualified_labels_does_not_stack_the_owner() {
    let edges = json!([
        {"source": "r_pdfreport", "target": "r_pdfreport_build", "relation": "method",
         "confidence": "EXTRACTED", "source_file": "Report.cs"}
    ]);
    let extraction = json!({
        "nodes": [
            {"id": "r_pdfreport", "label": "PdfReport", "file_type": "code",
             "source_file": "Report.cs", "source_location": "L2", "_origin": "ast"},
            {"id": "r_pdfreport_build", "label": "PdfReport.Build()", "file_type": "code",
             "source_file": "Report.cs", "source_location": "L2", "_origin": "ast"}
        ],
        "edges": edges,
    });
    let g = build_graph(extraction);
    assert_eq!(
        label(&g, "r_pdfreport_build"),
        Some(json!("PdfReport.Build()"))
    );
    let again = json!({"nodes": g.to_node_link("edges")["nodes"].clone(), "edges": edges});
    let g2 = build_graph(again);
    assert_eq!(
        label(&g2, "r_pdfreport_build"),
        Some(json!("PdfReport.Build()"))
    );
}

#[test]
fn qualified_methods_stay_out_of_god_nodes() {
    let (_dir, extraction) = extract_files(&[("Report.cs", ISSUE_REPRO)]);
    let g = build_graph(extraction);
    assert_eq!(
        label(&g, "report_pdfreport_build"),
        Some(json!("PdfReport.Build()"))
    );
    let method = g.node_index(&json!("report_pdfreport_build")).unwrap();
    let class = g.node_index(&json!("report_pdfreport")).unwrap();
    assert!(graphicle::analyze::is_file_node(&g, method).unwrap());
    assert!(!graphicle::analyze::is_file_node(&g, class).unwrap());
}

#[test]
fn semantic_method_ghost_still_collapses_onto_a_qualified_method() {
    // #3705 parity: an LLM ghost `Build()` in the same file remaps onto the
    // one AST method even when its persisted label is `PdfReport.Build()`.
    let extraction = json!({
        "nodes": [
            {"id": "report_pdfreport_build", "label": "PdfReport.Build()", "file_type": "code",
             "source_file": "Report.cs", "source_location": "L2", "_origin": "ast"},
            {"id": "report_build", "label": "Build()", "file_type": "code",
             "source_file": "Report.cs"},
            {"id": "doc", "label": "Doc", "file_type": "document", "source_file": "d.md"}
        ],
        "edges": [
            {"source": "doc", "target": "report_build", "relation": "references",
             "confidence": "INFERRED", "confidence_score": 0.85, "source_file": "d.md"}
        ],
    });
    let g = build_graph(extraction);
    assert!(g.node_index(&json!("report_build")).is_none());
    assert_eq!(
        g.number_of_edges(&json!("doc"), &json!("report_pdfreport_build")),
        1
    );
}

#[test]
fn class_segment_less_method_alias_still_resolves_a_qualified_method() {
    // The raw --no-cluster writer finalizes endpoints after relabeling, so
    // the `<file stem>_<method>` alias is still read off `PdfReport.Build()`.
    let mut data = json!({
        "nodes": [
            {"id": "report_pdfreport_build", "label": "PdfReport.Build()", "file_type": "code",
             "source_file": "Report.cs", "source_location": "L2", "_origin": "ast"},
            {"id": "doc", "label": "Doc", "file_type": "document", "source_file": "d.md"}
        ],
        "edges": [{"source": "doc", "target": "report_build", "relation": "references"}],
    });
    build::finalize_raw_graph_endpoints(&mut data);
    let pairs: Vec<(Value, Value)> = data["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["source"].clone(), e["target"].clone()))
        .collect();
    assert_eq!(pairs, vec![(json!("doc"), json!("report_pdfreport_build"))]);
}

#[test]
fn other_languages_keep_bare_method_labels() {
    let (_dir, extraction) = extract_files(&[
        (
            "Report.java",
            "class PdfReport { String build() { return \"pdf\"; } }\n",
        ),
        (
            "Report.swift",
            "class PdfReport { func build() -> String { return \"pdf\" } }\n",
        ),
    ]);
    let g = build_graph(extraction);
    let labels: std::collections::BTreeSet<String> = g
        .nodes()
        .iter()
        .filter_map(|n| {
            n.attributes
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|l| l.ends_with("()"))
        .collect();
    assert_eq!(labels, [".build()".to_string()].into_iter().collect());
}

#[test]
fn reassignment_fails_closed_on_missing_or_conflicting_owners() {
    let nodes = [
        ("a", json!({"label": "A", "source_file": "x.cs"})),
        ("b", json!({"label": "B", "source_file": "x.cs"})),
        (
            "ns",
            json!({"label": "Acme.Reports", "source_file": "x.cs"}),
        ),
        (
            "m_two_owners",
            json!({"label": ".Two()", "source_file": "x.cs"}),
        ),
        (
            "m_no_owner",
            json!({"label": ".None()", "source_file": "x.cs"}),
        ),
        (
            "m_dotted_owner",
            json!({"label": ".Dotted()", "source_file": "x.cs"}),
        ),
        ("m_ok", json!({"label": ".Ok()", "source_file": "x.cs"})),
        ("m_java", json!({"label": ".Ok()", "source_file": "x.java"})),
    ];
    let edges = [
        ("a", "m_two_owners"),
        ("b", "m_two_owners"),
        ("ns", "m_dotted_owner"),
        ("a", "m_ok"),
        ("a", "m_java"),
    ];
    let got = csharp_method_label_reassignments(nodes.iter().map(|(id, n)| (*id, n)), edges);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got.get("m_ok").map(String::as_str), Some("A.Ok()"));
}

#[test]
fn bare_label_recognizes_only_method_shapes() {
    let cases = [
        (".Build()", "Report.cs", Some(".Build()")),
        ("PdfReport.Build()", "Report.cs", Some(".Build()")),
        ("PdfReport.Build()", "Report.java", None),
        ("PdfReport", "Report.cs", None),
        ("Build()", "Report.cs", None),
        ("Acme.PdfReport.Build()", "Report.cs", None),
        ("Report.cs", "Report.cs", None),
    ];
    for (label, source_file, expected) in cases {
        assert_eq!(
            bare_csharp_method_label(&json!(label), &json!(source_file)).as_deref(),
            expected,
            "{label} in {source_file}"
        );
    }
}
