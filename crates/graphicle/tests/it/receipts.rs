//! No silent deletion: `memory.delete`, `storage.prune`, graph retention and
//! time-based source retention each sign a hash-chained deletion receipt that
//! names removed rows only by one-way hashes, and `deletion.verify` re-checks
//! it. Source retention forgets exactly the previewed documents
//! (`plan_hash`), skips documents a reviewed memory cites and documents with
//! no date, and blocks their re-import.
use graphicle_runtime::{
    deletion, local,
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
        let store = Store::open(dir.path().join("store")).unwrap();
        let ws = store.create_workspace("Receipts").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"receipts","name":"Receipts"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn mutate(&self, operation: &str, args: Value) -> Value {
        self.store
            .project_mutate(&self.workspace, &self.key, operation, &self.args(args))
            .unwrap()
    }
    fn verify(&self, receipt: &Value) -> Value {
        deletion::verify(
            &self.store,
            &self.workspace,
            &self.key,
            &json!({"receipt_id": receipt["id"]}),
        )
        .unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.store
            .connect()
            .unwrap()
            .query_row(sql, [&self.repo], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn memory_delete_signs_a_receipt_without_the_memory_and_verifies() {
    let f = Fixture::new();
    let imported = f.mutate(
        "memory.import",
        json!({"memories":[
            {"kind":"decision","title":"Release trains leave on Thursdays","body":"The team ships release trains every Thursday at 15:00 UTC.","data":{}},
            {"kind":"decision","title":"Keep","body":"Unrelated memory that stays.","data":{}}
        ]}),
    );
    let doomed = s(&imported["imported"][0], "id").to_string();
    let deleted = f.mutate("memory.delete", json!({"memory_id": doomed}));
    assert_eq!(deleted["deleted"], 1, "{deleted}");
    let receipt = &deleted["receipt"];
    assert_eq!(receipt["operation"], "memory.delete");
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["removed"]["rows"]["repo_memories"], 1);
    assert_eq!(receipt["removed"]["keys"]["count"], 1);
    assert_eq!(receipt["removed"]["keys"]["identifiers_retained"], false);
    assert_eq!(receipt["remaining"]["repo_memories"], 0);
    let body = receipt.to_string();
    assert!(
        !body.contains(&doomed),
        "the receipt must not keep the memory id"
    );
    assert!(!body.contains("Release trains"), "nor its title");
    assert!(
        deleted["storage"]["wal_checkpoint"].is_string(),
        "{deleted}"
    );
    assert_eq!(
        f.count("SELECT count(*) FROM repo_memories WHERE repository_id=?"),
        1
    );

    let checked = f.verify(receipt);
    assert_eq!(checked["complete"], true, "{checked:#}");
    assert_eq!(checked["current"]["reappeared_memories"], 0);
    assert_eq!(checked["chain"]["intact"], true);
    let listed = deletion::list(&f.store, &f.workspace, &f.key).unwrap();
    assert_eq!(listed["receipts"][0]["operation"], "memory.delete");
}

#[test]
fn storage_prune_signs_a_receipt_for_removed_snapshots_and_objects() {
    let f = Fixture::new();
    let root = f._dir.path().join("folder");
    std::fs::create_dir_all(&root).unwrap();
    let mut session = local::Session {
        workspace: f.workspace.clone(),
        capability: f.key.clone(),
        project: Some(f.repo.clone()),
    };
    let first = "The on-call rotation changes every Monday at 09:00.\n";
    std::fs::write(root.join("notes.md"), first).unwrap();
    local::sync(&f.store, &mut session, &root, Some(&f.repo)).unwrap();
    std::fs::write(
        root.join("notes.md"),
        "The on-call rotation changes every Tuesday.\n",
    )
    .unwrap();
    local::sync(&f.store, &mut session, &root, Some(&f.repo)).unwrap();
    assert!(f.count("SELECT count(*) FROM repo_snapshots WHERE repository_id=?") >= 2);

    let pruned = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "storage.prune",
            &f.args(json!({"keep_snapshots": 1})),
        )
        .unwrap();
    let receipt = &pruned["receipt"];
    assert_eq!(receipt["operation"], "storage.prune", "{pruned:#}");
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert!(
        receipt["removed"]["rows"]["repo_snapshots"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(receipt["removed"]["rows"]["repo_objects"].as_u64().unwrap() >= 1);
    assert_eq!(receipt["policy"]["keep_snapshots"], 1);
    let old_sha = graphicle_memory::text::sha256(first.as_bytes());
    assert!(!receipt.to_string().contains(&old_sha), "keys are hashed");
    assert_eq!(f.verify(receipt)["complete"], true);
    // Nothing left to prune: no second receipt.
    let again = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "storage.prune",
            &f.args(json!({"keep_snapshots": 1})),
        )
        .unwrap();
    assert!(again.get("receipt").is_none(), "{again}");
}

#[test]
fn source_retention_forgets_exactly_the_previewed_documents() {
    let f = Fixture::new();
    let connector = s(
        &f.store
            .connector_add(
                &f.workspace,
                &f.key,
                &f.args(json!({"kind":"jsonl","name":"chat","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    let recent = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    let cited_text = "user: The staging database password rotates on the first of each month.\n";
    let documents = json!([
        {"id":"old","title":"Old","content":"user: In 2019 the team used the Bristol office for planning.\n","source_timestamp":"2019-05-01T10:00:00Z"},
        {"id":"cited","title":"Cited","content":cited_text,"source_timestamp":"2019-06-01T10:00:00Z"},
        {"id":"fresh","title":"Fresh","content":"user: This week the planning moved to the Leeds office.\n","source_timestamp":recent},
        {"id":"undated","title":"Undated","content":"user: A note with no date at all.\n"}
    ]);
    f.store
        .import_documents(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":connector,"documents":documents})),
        )
        .unwrap();
    // A reviewed memory cites `cited`, so retention keeps it.
    let source = format!("connector:{connector}:cited");
    let sha = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT sha256 FROM am_connector_docs WHERE connector_id=? AND id='cited'",
            [&connector],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    let imported = f.mutate(
        "memory.import",
        json!({"memories":[{"kind":"decision","title":"Password rotation","body":"The staging password rotates monthly.",
            "data":{"native_sources":{source.clone(): sha}}}]}),
    );
    let memory = &imported["imported"][0];
    f.store
        .review_memory(
            &f.workspace,
            &f.key,
            &f.args(json!({"memory_id":memory["id"],"expected_revision":memory["revision"],"decision":"approve","maintainer":"tester","reason":"cited source"})),
        )
        .unwrap();

    let policy = json!({"connector_ids":[connector],"max_age_days":365,"by":"source_timestamp"});
    let preview = f
        .store
        .retention_preview(&f.workspace, &f.key, &f.args(json!({"sources": policy})))
        .unwrap();
    assert_eq!(preview["documents"], 1, "{preview:#}");
    assert_eq!(preview["skipped"]["cited_by_reviewed"], 1);
    assert_eq!(preview["skipped"]["undated"], 1);
    assert_eq!(preview["date_range"]["oldest"], "2019-05-01T10:00:00Z");
    assert_eq!(preview["scheduled"], false);
    assert!(
        preview["connectors"][&connector]["removes"].is_object(),
        "{preview:#}"
    );
    let plan_hash = preview["plan_hash"].as_str().unwrap().to_string();

    // Apply must echo the previewed plan.
    let refused = f.store.maintain(
        &f.workspace,
        &f.key,
        "retention.apply",
        &f.args(json!({"sources": policy, "plan_hash": "0"})),
    );
    assert!(refused.is_err());
    let applied = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "retention.apply",
            &f.args(json!({"sources": policy, "plan_hash": plan_hash})),
        )
        .unwrap();
    assert_eq!(applied["forgotten_documents"], 1, "{applied:#}");
    let receipt = &applied["receipt"];
    assert_eq!(receipt["operation"], "retention.forget");
    assert_eq!(receipt["policy"]["max_age_days"], 365);
    assert!(receipt["policy_digest"].is_string());
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(f.verify(receipt)["complete"], true);
    let left: Vec<String> = f
        .store
        .connect()
        .unwrap()
        .prepare("SELECT id FROM am_connector_docs WHERE connector_id=? ORDER BY id")
        .unwrap()
        .query_map([&connector], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(left, vec!["cited", "fresh", "undated"]);

    // The forgotten document is not re-imported by a later sync.
    f.store
        .import_documents(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":connector,"documents":[{"id":"old","title":"Old","content":"user: again\n","source_timestamp":"2019-05-01T10:00:00Z"}]})),
        )
        .unwrap();
    let back: i64 = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM am_connector_docs WHERE connector_id=? AND id='old'",
            [&connector],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(back, 0);
    // A second preview selects nothing, and applying it forgets nothing.
    let empty = f
        .store
        .retention_preview(&f.workspace, &f.key, &f.args(json!({"sources": policy})))
        .unwrap();
    assert_eq!(empty["documents"], 0, "{empty:#}");
}
