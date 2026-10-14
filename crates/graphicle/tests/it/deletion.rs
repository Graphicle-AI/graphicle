//! Verified deletion receipts: a connector or project deletion proves what it
//! removed and what remains, reports residue it does not own, tracks external
//! cleanup, and chains its receipts so rewriting is detectable.
use graphicle_runtime::{
    deletion, memory,
    store::{Store, s},
};
use rusqlite::Params;
use serde_json::{Value, json};
use tempfile::TempDir;

const SECRET: &str =
    "The quarterly migration window opens on the third Friday at 09:00 UTC for all regions.";
const MEMO: &str = "Board memo: the acquisition of Northwind closes in March pending final review.";

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Deletion receipts").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"deletion","name":"Deletion"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        };
        let files = [("README.md", "Public readme for the project.\n")];
        let begin=f.store.sync_prepare(&f.workspace,&f.key,&f.args(json!({"expected_snapshot":f.snapshot(),"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
        f.store.sync_upload(&f.workspace,&f.key,&f.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        f.store
            .sync_commit(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn snapshot(&self) -> Value {
        self.store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap()["repository"]["snapshot"]
            .clone()
    }
    fn connector(&self, documents: Value) -> String {
        let connector = self.store.connector_add(&self.workspace,&self.key,&self.args(json!({"kind":"jsonl","name":"Private","config":{"acl":{"workspace_readers":false,"credentials":[]}}}))).unwrap();
        self.store
            .import_documents(
                &self.workspace,
                &self.key,
                &self.args(json!({"connector_id":connector["id"],"documents":documents})),
            )
            .unwrap();
        s(&connector, "id").to_string()
    }
    fn sql(&self, sql: &str, arguments: impl Params) {
        self.store
            .transaction(|db| {
                db.execute(sql, arguments)?;
                Ok(())
            })
            .unwrap();
    }
    fn count(&self, sql: &str, arguments: impl Params) -> i64 {
        self.store
            .connect()
            .unwrap()
            .query_row(sql, arguments, |r| r.get(0))
            .unwrap()
    }
    fn delete_connector(&self, connector: &str) -> Value {
        self.store
            .maintain(
                &self.workspace,
                &self.key,
                "connector.delete",
                &self.args(json!({"connector_id":connector})),
            )
            .unwrap()
    }
    fn verify(&self, receipt: &Value) -> Value {
        deletion::verify(
            &self.store,
            &self.workspace,
            &self.key,
            &json!({"receipt_id":receipt["id"]}),
        )
        .unwrap()
    }
}

/// Derived state of every kind that can hold a source's id or text, written
/// after the source was imported (as the runtime would).
fn derive(f: &Fixture, source: &str) {
    let sha = graphicle_memory::text::sha256(SECRET.as_bytes());
    let (ws, repo) = (f.workspace.as_str(), f.repo.as_str());
    let t = graphicle_runtime::now();
    f.store
        .authorized(ws, &f.key, "approve", |db, _| {
            memory::save(db, repo, None, None, "note", "Migration window", SECRET, "reviewed",
                &json!({"dependencies":{},"evidence_links":{},"notes":{},"native_sources":{source:sha}}))
        })
        .unwrap();
    let notes = json!([{"text":"Migration window is the third Friday.","evidence":[{"quote":SECRET,"byte_start":0,"byte_end":SECRET.len()}]}]).to_string();
    f.sql("INSERT INTO am_observer_notes(repository_id,source_id,source_sha256,source_version,step_key,model_key,principal_hash,job_id,source_order,session_date,notes,dropped,created_at) VALUES(?1,?2,?3,'1','step','model','principal','job','',NULL,?4,0,?5)", rusqlite::params![repo, source, sha, notes, t]);
    // A reflection over that note and a kept source's: it consumed the
    // source, so it goes whole (no reflection workflow is run here; stores
    // can hold one).
    let note = format!("{repo}:{source}:0");
    let items = json!([{"section":"timeline","text":"Migration window is the third Friday.","date":"","notes":[note]}]).to_string();
    let inputs = json!({ note: "note-digest" }).to_string();
    let consumed = json!([source, "file:README.md"]).to_string();
    f.sql("INSERT INTO am_observer_reflections(repository_id,step_key,model_key,principal_hash,job_id,items,inputs,sources,dropped,created_at) VALUES(?1,'step','model','principal','job',?2,?3,?4,0,?5)", rusqlite::params![repo, items, inputs, consumed, t]);
    let attributes = json!({"document_id":source,"excerpt":SECRET}).to_string();
    f.sql("INSERT INTO gf_anchor_edges(repository_id,episode,chunk_id,node_id,rung,confidence,attributes,created_at) VALUES(?1,'e1','chunk-plan',NULL,'lexical',0.5,?2,?3)", rusqlite::params![repo, attributes, t]);
    let passage = format!("{source}:0");
    f.sql(
        "INSERT INTO am_vectors VALUES(?1,?2,?3,'embedding-model',3,'[0.1,0.2,0.3]')",
        rusqlite::params![repo, passage, sha],
    );
    for (kind, dependent) in [
        ("anchor", format!("{repo}:chunk-plan")),
        ("embedding", format!("{repo}:{passage}")),
    ] {
        f.sql(
            "INSERT INTO gf_dependencies VALUES(?1,?2,?3,'document',?4,?5)",
            rusqlite::params![ws, kind, dependent, source, t],
        );
        f.sql(
            "INSERT INTO gf_stale VALUES(?1,?2,?3,'source_changed',?4)",
            rusqlite::params![ws, kind, dependent, t],
        );
    }
    let answer = json!({"answer":SECRET}).to_string();
    f.sql(
        "INSERT INTO am_model_cache VALUES(?1,'request','state',?2,?3)",
        rusqlite::params![repo, answer, t],
    );
    f.sql("INSERT INTO gf_hints(id,workspace,repository_id,scope,target,weight,state,evidence,created_at,updated_at) VALUES('h1',?1,?2,'query',?3,1.0,'candidate','{}',?4,?4)", rusqlite::params![ws, repo, source, t]);
    let served = json!([source, "file:README.md"]).to_string();
    f.sql(
        "INSERT INTO gf_served VALUES('r1',?1,?2,'When is the window?',?3,?4)",
        rusqlite::params![ws, repo, served, t],
    );
    let targets = json!([source]).to_string();
    f.sql("INSERT INTO gf_outcomes(id,workspace,repository_id,request_hash,outcome,targets,note,created_at) VALUES('o1',?1,?2,'r1','helpful',?3,'',?4)", rusqlite::params![ws, repo, targets, t]);
}

#[test]
fn connector_deletion_returns_a_verified_receipt_covering_derived_state() {
    let f = Fixture::new();
    let connector = f.connector(json!([
        {"id":"plan","title":"Launch plan","content":SECRET},
        {"id":"memo","title":"Memo","content":MEMO}
    ]));
    let source = format!("connector:{connector}:plan");
    derive(&f, &source);
    let deleted = f.delete_connector(&connector);
    assert_eq!(deleted["derived_records_removed"], 1);
    assert!(
        deleted["storage"]["wal_checkpoint"].is_string(),
        "{deleted}"
    );
    let receipt = &deleted["receipt"];
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["external_pending"], false);
    assert_eq!(receipt["complete"], true);
    assert_eq!(receipt["external_cleanup"], json!([]));
    let removed = &receipt["removed"]["rows"];
    // Edges: the anchor's, the embedding's and the saved memory's. Only the
    // embedding's stale marker names the source; the anchor's is cleared by id.
    for (kind, expected) in [
        ("documents", 2),
        ("memories", 1),
        ("observer_notes", 1),
        ("observer_reflections", 1),
        ("anchors", 1),
        ("vectors", 1),
        ("hints", 1),
        ("dependency_edges", 3),
        ("stale_markers", 1),
        ("connector", 1),
    ] {
        assert_eq!(removed[kind], expected, "{kind}: {removed}");
    }
    assert_eq!(f.count("SELECT count(*) FROM gf_stale", []), 0);
    assert_eq!(f.count("SELECT count(*) FROM gf_dependencies WHERE dependent_kind IN ('anchor','embedding','memory')", []), 0);
    // Computed while the documents were visible, so removed whether or not
    // they name them: the cached answer, served record, outcome and hint.
    let since = &receipt["removed"]["since_visible"];
    for kind in ["model_cache", "served", "outcomes", "hints", "anchors"] {
        assert_eq!(since[kind], 1, "{kind}: {since}");
    }
    let recomputed: Vec<&str> = receipt["recomputed_on_demand"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for kind in ["model_cache", "served", "outcomes", "hints", "anchors"] {
        assert!(recomputed.contains(&kind), "{recomputed:?}");
    }
    for part in ["rows", "since_visible"] {
        for (kind, remaining) in receipt["remaining"][part].as_object().unwrap() {
            assert_eq!(remaining, 0, "{part}.{kind}");
        }
    }
    let residue = &receipt["residue"];
    assert_eq!(residue["source_ids"]["matches"], json!([]));
    assert!(residue["source_ids"]["tables_scanned"].as_u64().unwrap() > 20);
    assert_eq!(residue["verbatim_text"]["sampled"], 2);
    assert_eq!(residue["verbatim_text"]["matches"], json!([]));
    assert_eq!(receipt["serving_probe"]["visible_documents"], 0);
    assert_eq!(receipt["sources"]["count"], 2);
    assert_eq!(receipt["storage"]["secure_delete"], true);
    // The receipt proves coverage without keeping what it deleted.
    let text = receipt.to_string();
    for private in [
        source.as_str(),
        "migration window",
        "Launch plan",
        "Northwind",
    ] {
        assert!(!text.contains(private), "receipt retains {private}");
    }
    // Unrelated sources survive; records computed while the documents were
    // visible do not, even the parts that named other sources.
    assert_eq!(f.count("SELECT count(*) FROM gf_served", []), 0);
    assert_eq!(f.count("SELECT count(*) FROM gf_outcomes", []), 0);
    assert_eq!(
        f.store
            .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
            .unwrap()["total"],
        1
    );
    let checked = f.verify(receipt);
    assert_eq!(checked["verified_local"], true, "{checked:#}");
    assert_eq!(checked["complete"], true);
    assert_eq!(
        checked["chain"],
        json!({"length":1,"intact":true,"head":receipt["digest"]})
    );
    // Rewriting a stored receipt breaks the chain.
    f.sql(
        "UPDATE am_deletion_receipts SET body=replace(body,'\"verified_local\":true','\"verified_local\":false')",
        [],
    );
    let tampered = f.verify(receipt);
    assert_eq!(tampered["chain"]["intact"], false);
    assert_eq!(tampered["complete"], false);
}

#[test]
fn residue_the_deletion_does_not_own_is_reported_not_hidden() {
    let f = Fixture::new();
    let connector = f.connector(json!([{"id":"plan","title":"Launch plan","content":SECRET}]));
    let source = format!("connector:{connector}:plan");
    let (ws, repo) = (f.workspace.as_str(), f.repo.as_str());
    // A held-out question naming the source, and a user-written observation
    // quoting it verbatim: neither is owned by the deleted connector.
    let question = format!("What does {source} say?");
    f.sql(
        "INSERT INTO gf_heldout VALUES('q1',?1,?2,?3,'x','2026-01-01')",
        rusqlite::params![ws, repo, question],
    );
    let observation = format!("Noted: {SECRET}");
    f.sql("INSERT INTO gf_observations(id,workspace,repository_id,sequence,scope,kind,text,grounding,source,created_at,retracted_at) VALUES('ob1',?1,?2,1,'project','fact',?3,'[]','user','2026-01-01',NULL)", rusqlite::params![ws, repo, observation]);
    let receipt = f.delete_connector(&connector)["receipt"].clone();
    assert_eq!(receipt["verified_local"], false, "{receipt:#}");
    assert_eq!(receipt["complete"], false);
    assert_eq!(
        receipt["residue"]["source_ids"]["matches"],
        json!([{"table":"gf_heldout","rows":1}])
    );
    assert_eq!(
        receipt["residue"]["verbatim_text"]["matches"],
        json!([{"table":"gf_observations","rows":1}])
    );
    assert_eq!(
        f.count("SELECT count(*) FROM gf_observations", []),
        1,
        "user-written records are not silently destroyed"
    );
    let checked = f.verify(&receipt);
    assert_eq!(checked["verified_local"], false);
    assert_eq!(
        checked["current"]["source_ids"]["matches"],
        json!([{"table":"gf_heldout","rows":1}])
    );
    f.sql("DELETE FROM gf_heldout", []);
    assert_eq!(
        f.verify(&receipt)["verified_local"],
        false,
        "the known verbatim residue remains even after its source-id reference is removed"
    );
}

#[test]
fn source_reverification_requires_tombstones_and_checks_historical_rows() {
    let f = Fixture::new();
    let connector = f.connector(json!([{"id":"plan","title":"Plan","content":SECRET}]));
    let receipt = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "source.forget",
            &f.args(json!({"connector_id":connector,"document_ids":["plan"]})),
        )
        .unwrap()["receipt"]
        .clone();
    assert_eq!(f.verify(&receipt)["verified_local"], true);
    f.sql(
        "INSERT INTO am_connector_versions VALUES(?1,'plan','old',?2,'{}','2026-01-01')",
        rusqlite::params![connector, SECRET],
    );
    assert_eq!(
        f.verify(&receipt)["verified_local"],
        false,
        "a historical copy still retains the forgotten source"
    );
    f.sql(
        "DELETE FROM am_connector_versions WHERE connector_id=?",
        [&connector],
    );
    assert_eq!(f.verify(&receipt)["verified_local"], true);
    f.sql(
        "DELETE FROM am_forgotten_sources WHERE connector_id=?",
        [&connector],
    );
    assert_eq!(
        f.verify(&receipt)["verified_local"],
        false,
        "without its tombstone a provider can reimport the document"
    );
}

#[test]
fn external_cleanup_keeps_a_receipt_incomplete_until_it_finishes() {
    let f = Fixture::new();
    f.sql(
        "INSERT INTO am_backends VALUES(?1,'qdrant','{\"url\":\"http://127.0.0.1:9\",\"collection\":\"memory\"}')",
        [&f.repo],
    );
    let connector = f.connector(json!([{"id":"plan","title":"Launch plan","content":SECRET}]));
    let receipt = f.delete_connector(&connector)["receipt"].clone();
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["external_pending"], true);
    assert_eq!(receipt["complete"], false);
    assert_eq!(receipt["external_cleanup"][0]["kind"], "qdrant");
    assert_eq!(f.verify(&receipt)["external_pending"], true);
    // The projection worker marks the cleanup complete once Qdrant confirms.
    f.sql("UPDATE am_projection_cleanup SET state='complete'", []);
    let checked = f.verify(&receipt);
    assert_eq!(checked["external_pending"], false, "{checked:#}");
    assert_eq!(checked["complete"], true);
}

#[test]
fn project_deletion_receipt_outlives_the_project_and_extends_the_chain() {
    let f = Fixture::new();
    let first = f.connector(json!([{"id":"plan","title":"Launch plan","content":SECRET}]));
    let connector_receipt = f.delete_connector(&first)["receipt"].clone();
    let second = f.connector(json!([{"id":"memo","title":"Memo","content":MEMO}]));
    let memo = format!("connector:{second}:memo");
    let file = format!("{}:README.md", f.repo);
    f.sql(
        "INSERT INTO gf_dependencies VALUES(?1,'memory','m1','file',?2,'2026-01-01')",
        rusqlite::params![f.workspace, file],
    );
    f.sql(
        "INSERT INTO gf_dependencies VALUES(?1,'memory','m2','document',?2,'2026-01-01')",
        rusqlite::params![f.workspace, memo],
    );
    let deleted = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "project.delete",
            &f.args(json!({"expected_snapshot":f.snapshot()})),
        )
        .unwrap();
    let receipt = &deleted["receipt"];
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["remaining"]["project_rows"]["matches"], json!([]));
    // The two edges added here plus the project's own; none survive.
    assert!(
        receipt["removed"]["references"]["dependency_edges"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert_eq!(f.count("SELECT count(*) FROM gf_dependencies", []), 0);
    assert_eq!(receipt["sequence"], 2);
    assert_eq!(receipt["previous"], connector_receipt["digest"]);
    assert_eq!(f.count("SELECT count(*) FROM repo_projects", []), 0);
    let listed = deletion::list(&f.store, &f.workspace, &f.key).unwrap();
    assert_eq!(listed["receipts"].as_array().unwrap().len(), 2);
    assert_eq!(listed["receipts"][0]["operation"], "project.delete");
    assert_eq!(listed["receipts"][0]["verified_local"], true);
    assert_eq!(listed["chain"]["intact"], true);
    let checked = f.verify(receipt);
    assert_eq!(checked["verified_local"], true, "{checked:#}");
    assert_eq!(checked["chain"]["length"], 2);
    let earlier = f.verify(&connector_receipt);
    assert_eq!(earlier["verified_local"], true, "{earlier:#}");
    assert_eq!(earlier["current"]["serving_probe"], "project_deleted");
}
