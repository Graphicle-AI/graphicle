use graphicle_memory::text;
use graphicle_runtime::{graph, memory, memory_evidence, query};
use serde_json::{Value, json};

fn document(path: &str, body: &str) -> Value {
    json!({"id":format!("file:{path}"),"path":path,"title":path,"content":body,
        "sha256":text::sha256(body.as_bytes()),"source_kind":"repository_file","recorded_at":null})
}

fn snapshot(documents: Vec<Value>, memories: Vec<Value>) -> query::Snapshot {
    query::Snapshot {
        repository: json!({"id":"handoff-fixture","name":"Handoff fixture"}),
        documents,
        memories,
        graph: json!({"edges":[]}),
        state: "fixture-state".into(),
    }
}

/// A store's connection: `memory::dependencies` resolves quotes in code
/// files against its live-symbol table, which is empty here.
fn store() -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = graphicle_runtime::store::Store::open(dir.path())
        .unwrap()
        .connect()
        .unwrap();
    (dir, db)
}

fn reviewed(documents: &[Value], citations: Value) -> Value {
    let (_dir, db) = store();
    let mut data = memory::dependencies(
        &db,
        "handoff-fixture",
        citations.as_array().unwrap(),
        documents,
    )
    .unwrap();
    data["claim"] = json!({"evidence":citations});
    data["notes"] = json!({});
    json!({"id":"reviewed-routing","revision":1,"kind":"observation","state":"reviewed","stale":false,
        "title":"Emergency escalation owner","body":"Emergency escalation owner is Mira.","data":data})
}

#[test]
fn a_retrieved_interpretation_fetches_its_exact_late_original() {
    let quote = "For a Sev-1, page Mira via channel 42.";
    let doc = document(
        "routing.txt",
        &format!(
            "{}\n\n{quote}\n\n",
            "Routine rotation details.\n\n".repeat(180)
        ),
    );
    let memory = reviewed(
        std::slice::from_ref(&doc),
        json!([{"id":doc["id"],"quote":quote}]),
    );
    let snap = snapshot(vec![doc.clone()], vec![memory]);
    let packet = query::context(
        &snap,
        &json!({"question":"emergency escalation owner","max_tokens":1000}),
        json!({}),
    )
    .unwrap();
    assert_eq!(packet["memories"].as_array().unwrap().len(), 1);
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| { e["id"] == doc["id"] && e["text"].as_str().unwrap().contains(quote) }),
        "A reviewed interpretation must lead back to its cited original: {packet}"
    );
    for e in packet["evidence"].as_array().unwrap() {
        assert_eq!(
            e["text"].as_str().unwrap(),
            &doc["content"].as_str().unwrap()[e["byte_start"].as_u64().unwrap() as usize
                ..e["byte_end"].as_u64().unwrap() as usize]
        );
    }
}

#[test]
fn a_shared_reviewed_entity_reaches_the_other_original() {
    let docs = vec![
        document(
            "incident.txt",
            "Emergency escalation uses the duty rotation.",
        ),
        document("rotation.txt", "Mira carries pager 42."),
    ];
    let snap = snapshot(docs.clone(), vec![]);
    let graph = json!({"nodes":[
        {"id":docs[0]["id"],"kind":"source"},{"id":docs[1]["id"],"kind":"source"},
        {"id":"claim:a","kind":"claim"},{"id":"claim:b","kind":"claim"},
        {"id":"entity:duty","kind":"entity","name":"rotation"}],
        "edges":[
            {"source":"claim:a","target":docs[0]["id"],"kind":"supported_by","properties":{"quote":"Emergency escalation uses the duty rotation."}},
            {"source":"claim:a","target":"entity:duty","kind":"subject"},
            {"source":"claim:b","target":"entity:duty","kind":"subject"},
            {"source":"claim:b","target":docs[1]["id"],"kind":"supported_by","properties":{"quote":"Mira carries pager 42."}}]});
    let result = graph::local_rankings(&snap, &graph, "emergency escalation").unwrap();
    let (candidates, _) = query::candidates(&snap).unwrap();
    assert!(
        candidates
            .iter()
            .filter(|r| r["source"]["id"] == docs[1]["id"])
            .any(|r| result["graph_local"]
                .as_array()
                .unwrap()
                .contains(&r["identity"])),
        "The source–claim–entity–claim–source path must be traversable: {result}"
    );
}

#[test]
fn an_empty_graph_does_not_double_vote_for_lexical_seeds() {
    let snap = snapshot(
        vec![document(
            "incident.txt",
            "Emergency escalation uses the duty rotation.",
        )],
        vec![],
    );
    let result = graph::local_rankings(
        &snap,
        &json!({"nodes":[],"edges":[]}),
        "emergency escalation",
    )
    .unwrap();
    assert!(
        result["graph_local"].as_array().unwrap().is_empty(),
        "An empty graph contributes no additional evidence: {result}"
    );
}

#[test]
fn source_links_work_for_each_persisted_interpretation_format() {
    let doc = document("rotation.txt", "Mira carries pager 42.");
    let citations = json!([{"id":doc["id"],"quote":"Mira carries pager 42."}]);
    for owner in [
        "evidence",
        "claim",
        "answer",
        "report",
        "procedure",
        "candidate",
    ] {
        let mut m = reviewed(std::slice::from_ref(&doc), citations.clone());
        m["data"].as_object_mut().unwrap().remove("evidence");
        m["data"].as_object_mut().unwrap().remove("claim");
        if owner == "evidence" {
            m["data"][owner] = citations.clone();
        } else {
            m["data"][owner] = json!({"evidence":citations});
        }
        let linked =
            memory_evidence::gather(&snapshot(vec![doc.clone()], vec![m.clone()]), &[m]).unwrap();
        assert_eq!(linked.hits.len(), 1, "{owner}");
        assert_eq!(linked.hits[0]["text"], "Mira carries pager 42.");
    }
}

#[test]
fn source_links_reject_denied_changed_unreviewed_and_fabricated_evidence() {
    let doc = document("rotation.txt", "Mira carries pager 42.");
    let base = reviewed(
        std::slice::from_ref(&doc),
        json!([{"id":doc["id"],"quote":"Mira carries pager 42."}]),
    );
    for state in ["draft", "retired", "stale", "denied", "changed", "invented"] {
        let mut m = base.clone();
        let mut docs = vec![doc.clone()];
        match state {
            "draft" | "retired" => m["state"] = json!(state),
            "stale" => m["stale"] = json!(true),
            "denied" => docs.clear(),
            "changed" => {
                docs[0] = document(
                    "rotation.txt",
                    "Mira carries pager 42. The rotation is now over.",
                )
            }
            "invented" => {
                m["data"]["evidence"] = json!([]);
                m["data"]["claim"]["evidence"][0]["quote"] =
                    json!("Mira has approved the release.");
            }
            _ => unreachable!(),
        }
        let linked = memory_evidence::gather(&snapshot(docs, vec![m.clone()]), &[m]).unwrap();
        assert!(linked.hits.is_empty(), "{state}");
    }
}

#[test]
fn transitive_memory_reads_are_revision_checked_and_deduplicated() {
    let doc = document("rotation.txt", "Mira carries pager 42.");
    let parent = reviewed(
        std::slice::from_ref(&doc),
        json!([{"id":doc["id"],"quote":"Mira carries pager 42."}]),
    );
    let child = json!({"id":"child","revision":1,"state":"reviewed","stale":false,"data":{"notes":{"reviewed-routing":1}}});
    let snap = snapshot(vec![doc.clone()], vec![parent.clone(), child.clone()]);
    let linked = memory_evidence::gather(&snap, &[child.clone(), parent.clone()]).unwrap();
    assert_eq!(linked.hits.len(), 1);
    let ev =
        graphicle_runtime::evidence::from_range(&doc, 0, doc["content"].as_str().unwrap().len())
            .unwrap();
    assert!(
        linked.coverage(&[ev])["memories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["all_cited_quotes_present"] == true)
    );
    let mut changed = snap;
    changed.memories[0]["revision"] = json!(2);
    let linked = memory_evidence::gather(&changed, &[child]).unwrap();
    assert!(linked.hits.is_empty());
    assert_eq!(
        linked.coverage(&[])["memories"][0]["unavailable_citations"],
        1
    );
}

#[test]
fn memory_support_is_not_reported_complete_when_the_budget_cuts_its_quote() {
    let quote = "Mira carries pager 42 during this rotation. ".repeat(250);
    let doc = document("rotation.txt", &quote);
    let m = reviewed(
        std::slice::from_ref(&doc),
        json!([{"id":doc["id"],"quote":quote}]),
    );
    let packet = query::context(
        &snapshot(vec![doc], vec![m]),
        &json!({"question":"emergency escalation owner","max_tokens":400,"max_chars":2000}),
        json!({}),
    )
    .unwrap();
    assert!(packet["tokens"].as_u64().unwrap() <= 400);
    assert_eq!(
        packet["memory_support"]["memories"][0]["all_cited_quotes_present"],
        false
    );
}

#[test]
fn entity_names_do_not_match_inside_unrelated_words() {
    let snap = snapshot(vec![document("rotation.txt", "Pager number 42.")], vec![]);
    let graph = json!({"nodes":[{"id":"entity:ann","kind":"entity","name":"Ann"}],"edges":[
        {"source":"claim:rotation","target":"entity:ann","kind":"subject"},
        {"source":"claim:rotation","target":"file:rotation.txt","kind":"supported_by","properties":{"quote":"Pager number 42."}}]});
    let wrong = graph::local_rankings(&snap, &graph, "annual expenses").unwrap();
    assert!(wrong["graph_local"].as_array().unwrap().is_empty());
    let right = graph::local_rankings(&snap, &graph, "Ann's contact").unwrap();
    assert!(!right["graph_local"].as_array().unwrap().is_empty());
}

/// Window provenance of `memory::source_dependencies` and the quote bound of
/// `memory::dependencies`.
#[test]
fn window_provenance_preserves_utf8_and_checks_originals() {
    let text = format!("{}🙂{}", "é".repeat(5999), "remaining context ".repeat(500));
    let doc = json!({"id":"file:notes.txt","path":"notes.txt","source_kind":"repository_file","sha256":graphicle_memory::text::sha256(text.as_bytes()),"content":text});
    let docs = vec![doc.clone()];
    let (_dir, db) = store();
    let dependencies = graphicle_runtime::memory::source_dependencies(
        &db,
        "handoff-fixture",
        &[json!({"id":doc["id"],"text":text})],
        &docs,
    )
    .unwrap();
    let quotes = dependencies["evidence"].as_array().unwrap();
    assert!(quotes.len() > 1);
    assert!(
        quotes
            .iter()
            .all(|q| graphicle_runtime::store::s(q, "quote").len() <= 12000)
    );
    assert_eq!(
        quotes
            .iter()
            .map(|q| graphicle_runtime::store::s(q, "quote"))
            .collect::<String>(),
        text
    );
    assert_eq!(dependencies["dependencies"]["notes.txt"], doc["sha256"]);
    assert!(
        graphicle_runtime::memory::dependencies(
            &db,
            "handoff-fixture",
            &[json!({"id":doc["id"],"quote":text})],
            &docs
        )
        .is_err()
    );
    assert!(
        graphicle_runtime::memory::source_dependencies(
            &db,
            "handoff-fixture",
            &[json!({"id":doc["id"],"text":"An invented source window."})],
            &docs
        )
        .is_err()
    );
    assert!(
        graphicle_runtime::memory::source_dependencies(
            &db,
            "handoff-fixture",
            &[json!({"id":"private","text":text})],
            &docs
        )
        .is_err()
    );
}
