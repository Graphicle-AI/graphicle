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
