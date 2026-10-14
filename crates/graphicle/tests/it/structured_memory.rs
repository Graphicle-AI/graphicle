use graphicle_runtime::{
    api::{self, App},
    model, query,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let w = store.create_workspace("Structured memory").unwrap();
        let workspace = s(&w, "id").into();
        let key = s(&w, "capability").into();
        let r = store
            .register(
                s(&w, "id"),
                s(&w, "capability"),
                &json!({"project_key":"memory","name":"Memory"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: s(&r, "id").into(),
        }
    }
    fn args(&self, mut value: Value) -> Value {
        value["repository_id"] = json!(self.repo);
        value
    }
    fn note(&self, body: &str, extra: Value) -> Value {
        let mut a=self.args(json!({"title":"Rotation","body":body,"recall":{"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"]}}));
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.store.note(&self.workspace, &self.key, &a).unwrap()
    }
    fn search(&self, extra: Value) -> Value {
        let mut a = self.args(json!({"question":"duty owner","scopes":["project:larch"]}));
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.store
            .memory_search(&self.workspace, &self.key, &a)
            .unwrap()
    }
}

#[test]
fn replacements_are_atomic_dated_and_available_over_the_public_command() {
    let f = Fixture::new();
    let old=f.note("Mira owns the pager.",json!({"recall":{"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"],"valid_from":"2026-01-01T00:00:00Z"}}));
    let new=f.note("Otto owns the pager.",json!({"recall":{"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"],"valid_from":"2026-06-01T00:00:00Z"},"supersedes":{"memory_id":old["id"],"expected_revision":old["revision"]}}));
    for (at, expected) in [
        ("2026-05-31T23:59:59Z", &old),
        ("2026-06-01T00:00:00Z", &new),
    ] {
        let result = f.search(json!({"as_of":at}));
        assert_eq!(result["memories"].as_array().unwrap().len(), 1);
        assert_eq!(result["memories"][0]["id"], expected["id"]);
    }
    let app = App::new(
        f.store.clone(),
        model::Config::default(),
        vec!["localhost".into()],
    )
    .unwrap();
    let result = api::command(
        &app,
        &f.workspace,
        &f.key,
        "memory.search",
        &f.args(json!({"question":"rotation"})),
    )
    .unwrap();
    assert_eq!(result["memories"][0]["id"], new["id"]);
    assert_eq!(result["model_calls"], 0);
    let invalid=f.args(json!({"title":"Invalid","body":"Not written","recall":{"name":"rotation","scopes":["project:cedar"],"valid_from":"2026-08-01T00:00:00Z"},"supersedes":{"memory_id":new["id"],"expected_revision":new["revision"]}}));
    assert!(f.store.note(&f.workspace, &f.key, &invalid).is_err());
    assert_eq!(f.search(json!({}))["memories"][0]["id"], new["id"]);
    assert_eq!(
        f.store
            .project_read(
                &f.workspace,
                &f.key,
                "memories",
                &f.args(json!({"include_retired":true}))
            )
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn edits_preserve_scope_and_context_uses_the_same_validity_gate() {
    let f = Fixture::new();
    let original = f.note("Mira owns the pager.", json!({}));
    let edited=f.store.note(&f.workspace,&f.key,&f.args(json!({"memory_id":original["id"],"expected_revision":1,"title":"Rotation","body":"Mira owns pager 8."}))).unwrap();
    assert_eq!(edited["data"]["recall"], original["data"]["recall"]);
    assert!(
        f.search(json!({"scopes":["project:cedar"]}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut expired = edited.clone();
    expired["data"]["recall"]["valid_to"] = json!("2026-01-01T00:00:00Z");
    let snap = query::Snapshot {
        repository: json!({"id":f.repo}),
        documents: vec![],
        memories: vec![expired],
        graph: json!({"edges":[]}),
        state: "fixture".into(),
    };
    let result = query::context(&snap, &json!({"question":"rotation pager"}), json!({})).unwrap();
    assert!(result["memories"].as_array().unwrap().is_empty());
}

#[test]
fn a_filtered_parent_memory_cannot_reenter_through_an_evidence_chain() {
    let source = "Cedar's rotation uses the old policy.";
    let hash = graphicle_memory::text::sha256(source.as_bytes());
    let snapshot = query::Snapshot {
        repository: json!({"id":"scoped-evidence"}),
        documents: vec![json!({"id":"file:policy.txt","path":"policy.txt",
            "source_kind":"repository_file","title":"Policy","content":source,"sha256":hash})],
        memories: vec![
            json!({"id":"current","revision":1,"state":"reviewed","stale":false,
                "title":"Rotation","body":"The rotation procedure cites the earlier policy.",
                "data":{"recall":{"name":"rotation","scopes":["project:larch"]},"notes":{"parent":1}}}),
            json!({"id":"parent","revision":1,"state":"reviewed","stale":false,
                "title":"Policy","body":"Cedar's policy.",
                "data":{"recall":{"name":"policy","scopes":["project:cedar"]},
                    "dependencies":{"policy.txt":hash},"evidence":[{"id":"file:policy.txt","quote":source}]}}),
        ],
        graph: json!({"edges":[]}),
        state: "scoped-evidence".into(),
    };
    let packet = query::context(
        &snapshot,
        &json!({"question":"rotation procedure", "memory_scopes":["project:larch"]}),
        json!({}),
    )
    .unwrap();
    assert_eq!(packet["mechanisms"]["reviewed_memory_available"], 1);
    let support = packet["memory_support"]["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "current")
        .unwrap();
    assert_eq!(support["unavailable_citations"], 1);
    assert_eq!(support["located_citations"], 0);
    assert_eq!(support["all_cited_quotes_present"], false);
}

#[test]
fn memory_search_applies_scope_to_indirect_evidence_and_reports_the_gap() {
    let f = Fixture::new();
    let connector = f.store.connector_add(&f.workspace, &f.key,
        &f.args(json!({"name":"Policies","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}}))).unwrap();
    let quote = "Cedar retains the old policy.";
    f.store.import_documents(&f.workspace, &f.key,
        &f.args(json!({"connector_id":connector["id"],"mode":"delta","documents":[{"id":"policy","title":"Policy","content":quote}]}))).unwrap();
    let source = format!("connector:{}:policy", s(&connector, "id"));
    let parent = f.note(
        quote,
        json!({"recall":{"name":"policy","scopes":["project:cedar"]},
        "evidence":[{"id":source,"quote":quote}]}),
    );
    let db = f.store.connect().unwrap();
    let child = graphicle_runtime::memory::save(
        &db,
        &f.repo,
        None,
        None,
        "note",
        "Rotation",
        "The rotation refers to a separate policy.",
        "reviewed",
        &json!({"recall":{"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"]},
            "notes":{s(&parent,"id"):parent["revision"]}}),
    )
    .unwrap();
    let scoped = f.search(json!({}));
    assert_eq!(scoped["memories"][0]["id"], child["id"]);
    assert!(scoped["evidence"].as_array().unwrap().is_empty());
    assert_eq!(
        scoped["memory_support"]["memories"][0]["unavailable_citations"],
        1
    );
    let all = f.search(json!({"scopes":null}));
    assert_eq!(all["evidence"][0]["text"], quote);
    assert_eq!(
        all["memory_support"]["memories"][0]["all_cited_quotes_present"],
        true
    );
}

#[test]
fn knowledge_time_uses_the_reviewed_revision_then_known() {
    let f = Fixture::new();
    let old = f.note("Mira owns the pager.", json!({}));
    f.store.note(&f.workspace,&f.key,&f.args(json!({"memory_id":old["id"],"expected_revision":1,"title":"Rotation","body":"Otto owns the pager."}))).unwrap();
    let db = f.store.connect().unwrap();
    db.execute("UPDATE repo_memory_revisions SET created_at=CASE revision WHEN 1 THEN '2026-01-01T00:00:00Z' ELSE '2026-03-01T00:00:00Z' END WHERE memory_id=?",[s(&old,"id")]).unwrap();
    let before = f.search(json!({"known_at":"2026-02-01T00:00:00Z"}));
    assert_eq!(before["memories"][0]["body"], "Mira owns the pager.");
    assert_eq!(before["memories"][0]["revision"], 1);
    assert_eq!(
        f.search(json!({}))["memories"][0]["body"],
        "Otto owns the pager."
    );
    assert!(
        f.search(json!({"known_at":"2025-01-01T00:00:00Z"}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unreviewed_and_retired_memories_are_never_promoted_by_search() {
    let f = Fixture::new();
    let m = f.note("Mira owns the pager.", json!({}));
    let db = f.store.connect().unwrap();
    for state in ["draft", "retired"] {
        db.execute(
            "UPDATE repo_memories SET state=? WHERE id=?",
            [state, s(&m, "id")],
        )
        .unwrap();
        assert!(
            f.search(json!({"include_history":true}))["memories"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // The journal still contains the formerly reviewed revision. A
        // knowledge-time query must respect today's withdrawal of that note.
        assert!(
            f.search(json!({"include_history":true,"known_at":graphicle_runtime::now()}))["memories"]
                .as_array()
                .unwrap()
                .is_empty(),
            "Historical lookup restored a currently {state} memory"
        );
    }
    let mut data = m["data"].clone();
    data["dependencies"] = json!({"missing.txt":"a changed source"});
    db.execute(
        "UPDATE repo_memories SET state='reviewed',data=? WHERE id=?",
        [data.to_string(), s(&m, "id").to_owned()],
    )
    .unwrap();
    assert!(
        f.search(json!({"include_history":true,"known_at":graphicle_runtime::now()}))["memories"]
            .as_array()
            .unwrap()
            .is_empty(),
        "Historical lookup restored a currently stale memory"
    );
}

#[test]
fn memory_ids_cannot_overwrite_a_different_repository() {
    let f = Fixture::new();
    let m = f.note("Mira owns the pager.", json!({}));
    let other = f
        .store
        .register(
            &f.workspace,
            &f.key,
            &json!({"project_key":"other","name":"Other"}),
        )
        .unwrap();
    let result=f.store.note(&f.workspace,&f.key,&json!({"repository_id":other["id"],"memory_id":m["id"],"title":"Foreign","body":"A conflicting identifier."}));
    assert!(result.is_err());
    assert_eq!(
        f.search(json!({}))["memories"][0]["body"],
        "Mira owns the pager."
    );
}

#[test]
fn invalid_metadata_is_rejected_before_a_memory_is_written() {
    let f = Fixture::new();
    for recall in [
        json!({"name":"rotation","scopes":"wrong"}),
        json!({"name":"rotation","unknown":true}),
        json!({"name":"rotation","valid_from":"yesterday"}),
        json!({"name":"rotation","valid_from":"2026-01-02T00:00:00Z","valid_to":"2026-01-01T00:00:00Z"}),
    ] {
        assert!(
            f.store
                .note(
                    &f.workspace,
                    &f.key,
                    &f.args(
                        json!({"title":"Rotation","body":"Mira owns the pager.","recall":recall})
                    )
                )
                .is_err()
        );
    }
    assert!(
        f.search(json!({}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scoped_recall_keeps_source_quotes_and_withholds_permission_or_content_changes() {
    let f = Fixture::new();
    let connector = f.store.connector_add(&f.workspace, &f.key,
        &f.args(json!({"name":"Originals","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}}))).unwrap();
    let quote = "Mira owns the Larch pager.";
    let import = |body: &str| {
        f.store.import_documents(&f.workspace, &f.key,
        &f.args(json!({"connector_id":connector["id"],"mode":"delta","documents":[{"id":"rotation","title":"Roster","content":body}]}))).unwrap()
    };
    import(quote);
    let source = format!("connector:{}:rotation", s(&connector, "id"));
    let m = f.note(quote, json!({"evidence":[{"id":source,"quote":quote}]}));
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"Reader"}),
        )
        .unwrap();
    let search = || {
        f.store
            .memory_search(
                &f.workspace,
                s(&reader, "capability"),
                &f.args(json!({"question":"duty owner","scopes":["project:larch"]})),
            )
            .unwrap()
    };
    let result = search();
    assert_eq!(result["memories"][0]["id"], m["id"]);
    assert_eq!(result["evidence"][0]["text"], quote);
    f.store.connector_sharing(&f.workspace,&f.key,&f.args(json!({"connector_id":connector["id"],"acl":{"workspace_readers":false,"credentials":[]}}))).unwrap();
    assert!(search()["memories"].as_array().unwrap().is_empty());
    assert!(search()["evidence"].as_array().unwrap().is_empty());
    // Owners still need fresh evidence: source access cannot make an old
    // quotation current after the source changes.
    import("Otto owns the Larch pager.");
    assert!(
        f.search(json!({}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        f.search(json!({"known_at":"2026-12-01T00:00:00Z"}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn structured_proposals_remain_drafts_and_metadata_is_part_of_idempotency() {
    let f = Fixture::new();
    let connector=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"name":"Originals","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}}))).unwrap();
    let quote = "Mira owns the Larch pager.";
    f.store.import_documents(&f.workspace,&f.key,&f.args(json!({"connector_id":connector["id"],"mode":"delta","documents":[{"id":"rotation","title":"Roster","content":quote}]}))).unwrap();
    let mut proposal=f.args(json!({"kind":"note","title":"Rotation","body":quote,"idempotency_key":"draft-1",
        "recall":{"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"]},
        "evidence":[{"source_id":format!("connector:{}:rotation",s(&connector,"id")),"quote":quote}]}));
    let proposed = f
        .store
        .project_mutate(&f.workspace, &f.key, "memory.propose", &proposal)
        .unwrap();
    assert_eq!(proposed["state"], "draft");
    assert!(
        f.search(json!({}))["memories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.store
            .project_mutate(&f.workspace, &f.key, "memory.propose", &proposal)
            .unwrap()["replayed"],
        true
    );
    proposal["recall"]["scopes"] = json!(["project:cedar"]);
    assert!(
        f.store
            .project_mutate(&f.workspace, &f.key, "memory.propose", &proposal)
            .is_err()
    );
    let tools = api::mcp_tools(true);
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "search_project_memory")
    );
}
