// Adapted from Graphify 0.9.102 (v0.9.102), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_dedup.py, tests/test_build.py,
// tests/test_build_merge_ghost_fold_4955.py,
// tests/test_shrink_guard_dedup_merge_4940.py, tests/test_edge_also_seen.py,
// tests/test_lost_saved_semantic_nodes.py
//! Build, dedup and build write guard changes ported from Graphify
//! 0.9.96-0.9.102 (port part b). Cases follow the upstream tests named in
//! each comment.
use graphicle::dedup::{DedupOptions, deduplicate_entities};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn dedup(nodes: &[Value], edges: &[Value]) -> graphicle::dedup::DedupOutput {
    deduplicate_entities(nodes, edges, &DedupOptions::default(), None, None).unwrap()
}

fn stub(id: &str, label: &str, source_file: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "document", "source_file": source_file,
           "metadata": {"graphicle_reference_stub": true}})
}

#[test]
fn semantic_reference_stubs_are_namespaced() {
    // test_dedup.py (Graphify 0.9.96, #4738): citing documents keep their
    // reference stubs; a defining path keeps definer-wins.
    let logging = "specifications_20_architecture_operational_logging_document";
    let runtime = "functional_agent_runtime_doc";
    let license = "company_license_schedule_c";
    let nodes = [
        stub(logging, "Logging", "specifications/20-architecture/delivery/change-policy.md"),
        stub(logging, "Logging specification", ".github/instructions/rust.instructions.md"),
        stub(runtime, "Agent runtime", "functional/agents/agent-operator.md"),
        stub(runtime, "Agent Runtime and Subagent Contract (referenced)", "delivery/developer-interaction.md"),
        stub(license, "Schedule C - Support", "company/license.md"),
        json!({"id": license, "label": "Schedule C - Support", "file_type": "document", "source_file": "company/license-schedule-c.md"}),
    ];
    let kept: BTreeSet<(String, String)> = dedup(&nodes, &[])
        .nodes
        .iter()
        .map(|n| (n["id"].as_str().unwrap().into(), n["source_file"].as_str().unwrap().into()))
        .collect();
    let expected: BTreeSet<(String, String)> = [
        (format!("specifications_20_architecture_delivery_change_policy__ref__{logging}"), "specifications/20-architecture/delivery/change-policy.md"),
        (format!("github_instructions_rust_instructions__ref__{logging}"), ".github/instructions/rust.instructions.md"),
        (format!("functional_agents_agent_operator__ref__{runtime}"), "functional/agents/agent-operator.md"),
        (format!("delivery_developer_interaction__ref__{runtime}"), "delivery/developer-interaction.md"),
        (format!("company_license__ref__{license}"), "company/license.md"),
        (license.to_string(), "company/license-schedule-c.md"),
    ]
    .into_iter()
    .map(|(a, b)| (a, b.to_string()))
    .collect();
    assert_eq!(kept, expected);
}

#[test]
fn semantic_reference_stub_rewires_its_own_file_edges() {
    // test_dedup.py (Graphify 0.9.96, #4738).
    let nid = "functional_agent_runtime_doc";
    let nodes = [
        json!({"id": "functional_agents_agent_operator", "label": "Agent operator", "file_type": "document", "source_file": "functional/agents/agent-operator.md", "_origin": "semantic"}),
        stub(nid, "Agent runtime", "functional/agents/agent-operator.md"),
        json!({"id": "delivery_developer_interaction", "label": "Developer interaction", "file_type": "document", "source_file": "delivery/developer-interaction.md", "_origin": "semantic"}),
        stub(nid, "Agent Runtime and Subagent Contract (referenced)", "delivery/developer-interaction.md"),
    ];
    let edges = [
        json!({"source": "functional_agents_agent_operator", "target": nid, "relation": "references", "source_file": "functional/agents/agent-operator.md"}),
        json!({"source": "delivery_developer_interaction", "target": nid, "relation": "references", "source_file": "delivery/developer-interaction.md"}),
    ];
    let rewired: BTreeSet<(String, String)> = dedup(&nodes, &edges)
        .edges
        .iter()
        .map(|e| (e["source_file"].as_str().unwrap().into(), e["target"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        rewired,
        [
            ("functional/agents/agent-operator.md".to_string(), format!("functional_agents_agent_operator__ref__{nid}")),
            ("delivery/developer-interaction.md".to_string(), format!("delivery_developer_interaction__ref__{nid}")),
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn unmarked_nested_document_collision_keeps_existing_survivor() {
    // test_dedup.py (Graphify 0.9.96): path depth alone is no stub marker.
    let nid = "docs_protocol_firmware_protocol";
    let nodes = [
        json!({"id": nid, "label": "Firmware Protocol", "file_type": "document", "source_file": "docs/protocol.md"}),
        json!({"id": nid, "label": "Firmware protocol notes", "file_type": "document", "source_file": "docs/protocol/firmware.md"}),
    ];
    let kept: Vec<(String, String)> = dedup(&nodes, &[])
        .nodes
        .iter()
        .map(|n| (n["id"].as_str().unwrap().into(), n["source_file"].as_str().unwrap().into()))
        .collect();
    assert_eq!(kept, [(nid.to_string(), "docs/protocol.md".to_string())]);
}

#[test]
fn shared_module_nodes_are_no_id_collision() {
    // test_swift_import_resolution.py (Graphify 0.9.96): every importing file
    // mints the same module node; that is no collision warning.
    let module = |file: &str| json!({"id": "foundation", "label": "Foundation", "type": "module", "file_type": "code", "source_file": file});
    let out = dedup(&[module("a.swift"), module("b.swift")], &[]);
    assert!(!out.stderr.iter().any(|l| l.contains("minted by two different files")), "{:?}", out.stderr);
    let other = json!({"id": "foundation", "label": "Foundation", "type": "namespace", "file_type": "code", "source_file": "c.cs"});
    let out = dedup(&[module("a.swift"), other], &[]);
    assert!(out.stderr.iter().any(|l| l.contains("minted by two different files")), "{:?}", out.stderr);
}

#[test]
fn edges_missing_relation_are_dropped() {
    // test_build.py and test_no_cluster_undeclared_endpoints.py (Graphify
    // 0.9.97, #4809): an edge with no relation after alias folding is
    // dropped and reported, on the clustered and on the raw path; a `type`
    // alias still counts as a relation.
    let extraction = json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "a.py"}
        ],
        "edges": [
            {"source": "a", "target": "b", "confidence": "EXTRACTED", "source_file": "a.py"},
            {"source": "b", "target": "a", "type": "calls", "confidence": "EXTRACTED", "source_file": "a.py"}
        ]
    });
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build_from_json(&mut extraction.clone(), true, None, &mut d).unwrap();
    assert_eq!(g.edge_count(), 1);
    assert!(d.stderr.iter().any(|l| l.contains("dropped 1 schema-invalid edge(s) missing required field 'relation'")), "{:?}", d.stderr);
    let mut raw = extraction;
    assert_eq!(graphicle::build::finalize_raw_graph_endpoints(&mut raw), 1);
    assert_eq!(raw["edges"].as_array().unwrap().len(), 1);
    assert_eq!(raw["edges"][0]["relation"], "calls");
}

#[test]
fn two_file_import_cycle_survives_the_undirected_build() {
    // test_undirected_two_file_import_cycle.py (Graphify 0.9.98, #4839): the
    // dropped reverse import is a stamp on the kept edge, and
    // find_import_cycles reads it back.
    let node = |id: &str, file: &str| json!({"id": id, "label": file, "file_type": "code", "source_file": file});
    let import = |s: &str, t: &str, file: &str| json!({"source": s, "target": t, "relation": "imports_from", "confidence": "EXTRACTED", "source_file": file});
    let mut extraction = json!({
        "nodes": [node("a", "src/a.ts"), node("b", "src/b.ts")],
        "edges": [import("a", "b", "src/a.ts"), import("b", "a", "src/b.ts")],
    });
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build_from_json(&mut extraction, false, None, &mut d).unwrap();
    assert_eq!(g.edge_count(), 1);
    let cycles = graphicle::analyze::find_import_cycles(&g, 10, 10).unwrap();
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    // A one-way import is no cycle.
    let mut extraction = json!({
        "nodes": [node("a", "src/a.ts"), node("b", "src/b.ts")],
        "edges": [import("a", "b", "src/a.ts")],
    });
    let g = graphicle::build::build_from_json(&mut extraction, false, None, &mut d).unwrap();
    assert!(graphicle::analyze::find_import_cycles(&g, 10, 10).unwrap().is_empty());
}

#[test]
fn collapsed_edge_keeps_weaker_evidence_in_also_seen() {
    // test_edge_also_seen.py (Graphify 0.9.98, #4881): one relation found
    // twice keeps one edge; the weaker copy is listed in also_seen, and the
    // result is the same whichever order the copies arrive in.
    let a = json!({"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py", "source_location": "L3"});
    let b = json!({"source": "a", "target": "b", "relation": "calls", "confidence": "INFERRED", "confidence_score": 0.6, "source_file": "b.py"});
    for edges in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
        let mut extraction = json!({
            "nodes": [
                {"id": "a", "label": "a", "file_type": "code", "source_file": "a.py"},
                {"id": "b", "label": "b", "file_type": "code", "source_file": "b.py"}
            ],
            "edges": edges,
        });
        let mut d = graphicle::build::Diagnostics::default();
        let g = graphicle::build::build_from_json(&mut extraction, true, None, &mut d).unwrap();
        let attrs = graphicle::build::edge_data(&g, &json!("a"), &json!("b")).unwrap();
        assert_eq!(attrs.get("confidence"), Some(&json!("EXTRACTED")));
        assert_eq!(attrs.get("confidence_score"), None);
        assert_eq!(
            attrs.get("also_seen"),
            Some(&json!([{"confidence": "INFERRED", "confidence_score": 0.6, "source_file": "b.py"}]))
        );
    }
}

// --- test_shrink_guard_dedup_merge_4940.py (Graphify 0.9.101, #4940) and
// test_build_merge_ghost_fold_4955.py (0.9.102, #4955) ---

fn concept(id: &str, label: &str, source_file: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "concept", "source_file": source_file, "_origin": "semantic"})
}

#[test]
fn dedup_reports_each_id_it_merged_away() {
    let nodes = [
        concept("glossary_the_nudge", "The nudge", "GLOSSARY.md"),
        concept("glossary_the_nudge_2", "The nudge", "GLOSSARY.md"),
        concept("claude_a", "Alpha concept", "CLAUDE.md"),
    ];
    let out = dedup(&nodes, &[]);
    assert_eq!(out.remap.len(), 1, "{:?}", out.remap);
    let (loser, winner) = out.remap.iter().next().unwrap();
    let survivors: BTreeSet<&str> = out.nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert!(survivors.contains(winner.as_str()) && !survivors.contains(loser.as_str()));
    let out = dedup(
        &[
            concept("claude_a", "Alpha concept", "CLAUDE.md"),
            concept("glossary_b", "Bravo concept", "GLOSSARY.md"),
        ],
        &[],
    );
    assert!(out.remap.is_empty());
}

#[test]
fn build_carries_the_dedup_map_but_never_writes_it() {
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build(
        &[json!({"nodes": [
            concept("glossary_the_nudge", "The nudge", "GLOSSARY.md"),
            concept("glossary_the_nudge_2", "The nudge", "GLOSSARY.md")
        ], "edges": [], "hyperedges": []})],
        &graphicle::build::BuildOptions::default(),
        &mut d,
        None,
    )
    .unwrap();
    assert_eq!(g.dedup_remap().len(), 1);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let communities: graphicle::cluster::Communities = vec![];
    assert!(graphicle::export::to_json(&g, &communities, &out, &graphicle::export::JsonOptions::default(), &mut d).unwrap());
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains("_dedup_remap"), "{text}");
}

#[test]
fn a_survivor_that_is_itself_gone_excuses_nothing() {
    let saved = [concept("loser", "The nudge", "GLOSSARY.md")];
    let remap: std::collections::BTreeMap<String, String> =
        [("loser".to_string(), "winner".to_string())].into();
    let lost = graphicle::build::lost_saved_semantic_nodes_with(&saved, &[], &[], None, Some(&remap));
    assert_eq!(lost, saved);
    let kept = [concept("winner", "The nudge", "GLOSSARY.md")];
    let lost = graphicle::build::lost_saved_semantic_nodes_with(&saved, &kept, &[], None, Some(&remap));
    assert!(lost.is_empty());
}

#[test]
fn the_allowance_counts_only_merges_whose_survivor_is_present() {
    let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<std::collections::HashSet<_>>();
    let remap: std::collections::BTreeMap<String, String> = [("a", "w"), ("b", "w"), ("c", "gone")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let allowance = graphicle::build::dedup_shrink_allowance;
    assert_eq!(allowance(&set(&["a", "b", "c"]), &set(&["w"]), &remap), 1);
    assert_eq!(allowance(&set(&["a", "b", "w"]), &set(&["w"]), &remap), 2);
    assert_eq!(allowance(&set(&["a"]), &set(&[]), &remap), 0);
}

#[test]
fn semantic_record_with_its_own_evidence_is_not_folded_into_an_ast_twin() {
    // test_lost_saved_semantic_nodes.py (Graphify 0.9.101, #4896): a mention
    // with a rationale or source URL stays its own node; one with only
    // metadata still folds.
    let ast = json!({"id": "auth_py", "label": "auth.py", "file_type": "code", "source_file": "auth.py", "_origin": "ast", "source_location": "L1"});
    for (extra, folds) in [
        (json!({"metadata": {"k": "v"}}), true),
        (json!({"rationale": "why auth matters"}), false),
        (json!({"source_url": "https://example.org/spec"}), false),
    ] {
        let mut mention = json!({"id": "notes_auth", "label": "auth.py", "file_type": "document", "source_file": "notes.md"});
        for (k, v) in extra.as_object().unwrap() {
            mention[k] = v.clone();
        }
        let mut extraction = json!({"nodes": [ast.clone(), mention], "edges": []});
        let mut d = graphicle::build::Diagnostics::default();
        let g = graphicle::build::build_from_json(&mut extraction, true, None, &mut d).unwrap();
        let present = g.node_index(&json!("notes_auth")).is_some();
        assert_eq!(present, !folds, "{extra}");
        assert_eq!(g.dedup_remap().contains_key("notes_auth"), folds, "{extra}");
    }
}
