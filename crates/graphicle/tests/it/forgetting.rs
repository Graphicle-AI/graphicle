//! Certified forgetting. A store is forked before some documents arrive; one
//! copy ingests them, does ordinary work, forgets them and reruns the work;
//! the other never sees them and does the same work. Both must then hold the
//! same state (graphicle.state.v2 digest) and answer every probe identically.
use axum::{Json, Router, routing::post};
use graphicle_runtime::{
    deletion,
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;

const README: &str = "# Handbook\n\nuser: Deploys happen on Tuesdays after the change review.\n";
const S0: &[(&str, &str)] = &[
    (
        "s0-a",
        "Conversation date: 2024/03/02\n\nuser: I adopted a greyhound named Comet from the rescue in Leeds.\nassistant: Congratulations on Comet!\n",
    ),
    (
        "s0-b",
        "Conversation date: 2024/03/09\n\nuser: My sister Priya is visiting from Pune next month.\nassistant: That sounds lovely.\n",
    ),
];
const S1: &[(&str, &str)] = &[
    (
        "s1-a",
        "Conversation date: 2024/04/01\n\nuser: I switched my running shoes to the Pegasus 41 for marathon training.\nassistant: Good choice for mileage.\n",
    ),
    (
        "s1-b",
        "Conversation date: 2024/04/05\n\nuser: Comet now sleeps on the blue sofa in the study.\nassistant: Greyhounds love sofas.\n",
    ),
];
/// The documents to forget: one private fact each, plus an id that is a
/// prefix of a kept document's id (`s1-a` vs `s1-a:2`) to test exact scoping.
const X: &[(&str, &str)] = &[
    (
        "x-medical",
        "Conversation date: 2024/04/03\n\nuser: My cardiologist in Harrogate changed my beta blocker dose to 50 mg.\nassistant: Noted, follow the new dose.\n",
    ),
    (
        "s1-a:2",
        "Conversation date: 2024/04/04\n\nuser: My new salary at Orbital Freight is 91,500 pounds.\nassistant: Congratulations on the raise.\n",
    ),
];
const PROBES: &[&str] = &[
    "What dose did my cardiologist prescribe?",
    "What is my salary at Orbital Freight?",
    "Where does Comet sleep?",
    "Which running shoes do I use?",
];

struct Provider {
    config: model::Config,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("user: "))
        .map(str::to_string)
        .collect()
}

/// A deterministic stand-in for the model: the same request always gets the
/// same answer, so recomputation after forgetting is reproducible.
async fn responses(Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let result = if let Some(session) = input["session"].as_str() {
        json!({"notes":user_lines(session).iter().map(|l|json!({"text":format!("User said: {l}"),"quotes":[l]})).collect::<Vec<_>>()})
    } else if let Some(excerpt) = input["primary_excerpt"].as_str() {
        // Memory drafting (`extract`): one claim per user line, quoting it.
        let source = &input["primary_source_id"];
        json!({"claims":user_lines(excerpt).iter().take(8).map(|l|json!({"statement":l,"subject":"user",
            "relation":"source_observation","object":l,"speaker":"user","modality":"report","conditions":[],
            "action":"ADD","target_id":null,"valid_at":null,"invalid_at":null,"time_basis":"unknown",
            "anchor_id":null,"evidence":[{"id":source,"quote":l}]})).collect::<Vec<_>>()})
    } else {
        let sources = input["sources"].as_array().cloned().unwrap_or_default();
        match sources.first() {
            Some(first) => {
                json!({"answer":s(first, "text").chars().take(80).collect::<String>(),"abstain":false,
                "evidence":[{"id":first["id"],"quote":first["text"]}],"unresolved":[]})
            }
            None => json!({"answer":"","abstain":true,"evidence":[],"unresolved":["no sources"]}),
        }
    };
    Json(
        json!({"status":"completed","model":"stub","usage":{"input_tokens":40,"output_tokens":10},"output":[{"type":"message","content":[{"type":"output_text","text":result.to_string()}]}]}),
    )
}

async fn embeddings(Json(request): Json<Value>) -> Json<Value> {
    let data: Vec<Value> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let mut vector = vec![1e-3; 16];
            for word in text
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .split_whitespace()
            {
                let slot =
                    graphicle_memory::text::sha256(word.as_bytes()).as_bytes()[0] as usize % 16;
                vector[slot] += 1.0;
            }
            json!({"index":i,"embedding":vector})
        })
        .collect();
    Json(json!({"data":data,"usage":{"prompt_tokens":5}}))
}

async fn provider() -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .route("/v1/embeddings", post(embeddings));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{address}/v1");
    let config = serde_json::from_value(json!({"default_model":"stub","embedding_model":"embed","models":{
        "stub":{"protocol":"responses","base_url":base,"model":"stub","input_limit":16000,"output_limit":2000},
        "embed":{"protocol":"chat","base_url":base,"model":"embed","input_limit":8000,"output_limit":64}}}))
    .unwrap();
    Provider { config, task }
}

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
        let ws = store.create_workspace("Forgetting").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"forgetting","name":"Forgetting"}),
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
        let files = [("README.md", README)];
        let status = f
            .store
            .project_status(&f.workspace, &f.key, &f.args(json!({})))
            .unwrap();
        let begin=f.store.sync_prepare(&f.workspace,&f.key,&f.args(json!({"expected_snapshot":status["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
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
    /// An independent copy of the store as it is now: the counterfactual
    /// world shares everything that happened before this point.
    fn fork(&self) -> Self {
        let dir = TempDir::new().unwrap();
        self.store
            .connect()
            .unwrap()
            .backup(rusqlite::MAIN_DB, dir.path().join("reviews.sqlite3"), None)
            .unwrap();
        let key = self._dir.path().join("vault.key");
        if key.exists() {
            std::fs::copy(key, dir.path().join("vault.key")).unwrap();
        }
        Self {
            store: Store::open(dir.path()).unwrap(),
            _dir: dir,
            workspace: self.workspace.clone(),
            key: self.key.clone(),
            repo: self.repo.clone(),
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn connector(&self, name: &str) -> String {
        let connector = self.store.connector_add(&self.workspace,&self.key,&self.args(json!({"kind":"jsonl","name":name,"config":{"acl":{"workspace_readers":true,"credentials":[]}}}))).unwrap();
        s(&connector, "id").to_string()
    }
    fn import(&self, connector: &str, documents: &[(&str, &str)]) -> Value {
        let documents: Vec<Value> = documents
            .iter()
            .map(|(id, content)| json!({"id":id,"title":format!("Session {id}"),"content":content}))
            .collect();
        self.store
            .import_documents(
                &self.workspace,
                &self.key,
                &self.args(json!({"connector_id":connector,"documents":documents})),
            )
            .unwrap()
    }
    async fn run(&self, m: &Provider, args: Value) -> Value {
        let queued = self
            .store
            .enqueue(&self.workspace, &self.key, &self.args(args), &m.config)
            .unwrap();
        let worker = Worker::new(self.store.clone(), m.config.clone()).unwrap();
        loop {
            let status = self
                .store
                .job_status(
                    &self.workspace,
                    &self.key,
                    &self.args(json!({"job_id":queued["id"]})),
                )
                .unwrap();
            if !["queued", "retry", "running"].contains(&s(&status, "state")) {
                assert_eq!(status["state"], "complete", "{status}");
                return status;
            }
            worker.once().await.unwrap();
        }
    }
    async fn until_complete(&self, m: &Provider, args: Value) {
        for _ in 0..40 {
            if self.run(m, args.clone()).await["result"]["complete"] == true {
                return;
            }
        }
        panic!("{args} did not complete");
    }
    fn context(&self, question: &str) -> Value {
        self.store
            .project_context(
                &self.workspace,
                &self.key,
                &self.args(json!({"question":question,"max_tokens":1500})),
            )
            .unwrap()
    }
    fn state(&self) -> Value {
        deletion::state_api(
            &self.store,
            &self.workspace,
            &self.key,
            &self.args(json!({})),
        )
        .unwrap()
    }
    fn maintain(&self, operation: &str, args: Value) -> Value {
        self.store
            .maintain(&self.workspace, &self.key, operation, &self.args(args))
            .unwrap()
    }
}

/// Ordinary work on a project: index, observe, draft memories, retrieve and
/// answer.
async fn work(f: &Fixture, m: &Provider, connectors: &[&str]) -> Vec<Value> {
    f.until_complete(
        m,
        json!({"kind":"embed","max_calls":32,"max_input_tokens":512000,"max_output_tokens":64}),
    )
    .await;
    let include: Vec<String> = connectors
        .iter()
        .map(|c| format!("connector:{c}:"))
        .collect();
    f.until_complete(
        m,
        json!({"kind":"observer_notes","include":include,"max_calls":32}),
    )
    .await;
    f.until_complete(m, json!({"kind":"extract","max_calls":16}))
        .await;
    let mut answers = Vec::new();
    for question in PROBES {
        f.context(question);
        let job = f
            .run(m, json!({"kind":"basic","question":question,"max_calls":2}))
            .await;
        answers.push(job["result"]["answer"].clone());
    }
    answers
}

/// Ids minted per job or memory differ between the two worlds by design.
fn masked(value: &Value) -> Value {
    let ids = regex::Regex::new(r"\b[0-9a-f]{32}\b").unwrap();
    let clock = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z").unwrap();
    serde_json::from_str(&clock.replace_all(&ids.replace_all(&value.to_string(), "<id>"), "<time>"))
        .unwrap()
}

fn differing(a: &Value, b: &Value) -> BTreeMap<String, (Value, Value)> {
    let mut out = BTreeMap::new();
    let (ta, tb) = (
        a["tables"].as_object().unwrap(),
        b["tables"].as_object().unwrap(),
    );
    for name in ta.keys().chain(tb.keys()) {
        if ta.get(name) != tb.get(name) {
            out.insert(
                name.clone(),
                (
                    ta.get(name).cloned().unwrap_or(Value::Null),
                    tb.get(name).cloned().unwrap_or(Value::Null),
                ),
            );
        }
    }
    out
}

fn dump(f: &Fixture, table: &str) -> Vec<String> {
    let db = f.store.connect().unwrap();
    let mut statement = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|c| c.to_string())
        .collect();
    let mut out: Vec<String> = statement
        .query_map([], |row| {
            let mut map = serde_json::Map::new();
            for (i, name) in names.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(n) => json!(n),
                    _ => Value::Null,
                };
                map.insert(name.clone(), value);
            }
            Ok(masked(&Value::Object(map)).to_string())
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out.sort();
    out
}

fn assert_same_world(a: &Fixture, b: &Fixture) {
    let (sa, sb) = (a.state(), b.state());
    if sa["digest"] != sb["digest"] {
        for table in differing(&sa, &sb).keys() {
            let (ra, rb) = (dump(a, table), dump(b, table));
            let only_a: Vec<_> = ra.iter().filter(|r| !rb.contains(r)).collect();
            let only_b: Vec<_> = rb.iter().filter(|r| !ra.contains(r)).collect();
            eprintln!(
                "{table}: only in forgetting store {only_a:#?}\nonly in never-ingested store {only_b:#?}"
            );
        }
    }
    assert!(
        sa["digest"] == sb["digest"],
        "stores differ in {:#?}",
        differing(&sa, &sb)
    );
    for question in PROBES {
        // Snapshot keys digest per-store memory ids; everything they cover
        // (sources, evidence, memories) is compared directly.
        let probe = |f: &Fixture| {
            let mut packet = masked(&f.context(question));
            packet.as_object_mut().unwrap().remove("source_state");
            packet
        };
        let (pa, pb) = (probe(a), probe(b));
        if pa != pb {
            for key in pa.as_object().unwrap().keys() {
                if pa[key] != pb[key] {
                    eprintln!(
                        "{question}: {key}\n  forgetting store: {}\n  never-ingested:   {}",
                        pa[key], pb[key]
                    );
                }
            }
        }
        assert_eq!(pa, pb, "{question}");
    }
}

#[tokio::test]
async fn forgetting_documents_leaves_the_store_a_never_ingested_fork_would_have() {
    let m = provider().await;
    let a = Fixture::new();
    let chat = a.connector("Chat");
    a.import(&chat, S0);
    work(&a, &m, &[&chat]).await;
    let b = a.fork();
    let mut both: Vec<(&str, &str)> = S1.to_vec();
    both.extend_from_slice(X);
    a.import(&chat, &both);
    b.import(&chat, S1);
    let seen = work(&a, &m, &[&chat]).await;
    work(&b, &m, &[&chat]).await;
    assert_ne!(
        a.state()["digest"],
        b.state()["digest"],
        "the documents made a difference"
    );
    assert!(
        seen[0].as_str().unwrap().contains("cardiologist"),
        "{seen:?}"
    );

    let ids: Vec<&str> = X.iter().map(|(id, _)| *id).collect();
    let forgot = a.maintain(
        "source.forget",
        json!({"connector_id":chat,"document_ids":ids}),
    );
    let receipt = &forgot["receipt"];
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["operation"], "source.forget");
    assert_eq!(receipt["scope"]["documents"], 2);
    assert_eq!(receipt["removed"]["rows"]["documents"], 2);
    assert_eq!(receipt["remaining"]["rows"]["documents"], 0);
    assert!(
        receipt["removed"]["rows"]["observer_notes"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert!(receipt["removed"]["rows"]["vectors"].as_u64().unwrap() >= 2);
    let text = receipt.to_string();
    for private in ["x-medical", "s1-a:2", "cardiologist", "Orbital"] {
        assert!(!text.contains(private), "receipt keeps {private}");
    }

    // Recompute what forgetting removed, as the next requests would.
    let answers = work(&a, &m, &[&chat]).await;
    assert_same_world(&a, &b);
    // The comparison covers the derived state, not an empty shell.
    let covered = a.state();
    for table in [
        "am_connector_docs",
        "am_connector_versions",
        "am_observer_notes",
        "am_vectors",
        "repo_memories",
        "repo_memory_revisions",
        "am_claims",
        "am_document_reads",
        "gf_dependencies",
        "repo_objects",
    ] {
        assert!(
            covered["tables"][table]["rows"].as_u64().unwrap_or(0) > 0,
            "{table}: {covered:#}"
        );
    }
    assert_eq!(answers, work(&b, &m, &[&chat]).await);
    assert!(
        !answers
            .iter()
            .any(|a| a.as_str().unwrap().contains("cardiologist")),
        "{answers:?}"
    );
    assert_same_world(&a, &b);

    // A forgotten document stays forgotten when the provider sends it again.
    let again = a.import(&chat, X);
    assert_eq!(again["forgotten_skipped"], 2);
    assert_same_world(&a, &b);
    let checked = deletion::verify(
        &a.store,
        &a.workspace,
        &a.key,
        &json!({"receipt_id":receipt["id"]}),
    )
    .unwrap();
    assert_eq!(checked["verified_local"], true, "{checked:#}");
    assert_eq!(checked["current"]["tombstones"], 2);
}

#[tokio::test]
async fn deleting_a_connector_leaves_the_store_a_never_connected_fork_would_have() {
    let m = provider().await;
    let a = Fixture::new();
    let chat = a.connector("Chat");
    a.import(&chat, S0);
    work(&a, &m, &[&chat]).await;
    let b = a.fork();
    a.import(&chat, S1);
    b.import(&chat, S1);
    let health = a.connector("Health");
    a.import(&health, X);
    work(&a, &m, &[&chat, &health]).await;
    work(&b, &m, &[&chat]).await;
    let deleted = a.maintain("connector.delete", json!({"connector_id":health}));
    assert_eq!(
        deleted["receipt"]["verified_local"], true,
        "{:#}",
        deleted["receipt"]
    );
    work(&a, &m, &[&chat]).await;
    assert_same_world(&a, &b);
}

#[tokio::test]
async fn a_record_written_from_forgotten_text_breaks_equivalence_and_is_reported() {
    let m = provider().await;
    let a = Fixture::new();
    let chat = a.connector("Chat");
    a.import(&chat, S0);
    work(&a, &m, &[&chat]).await;
    let b = a.fork();
    a.import(&chat, X);
    work(&a, &m, &[&chat]).await;
    // Someone copies the private fact into a user-written held-out question:
    // not derived by the runtime, so forgetting must not silently drop it.
    a.store
        .transaction(|db| {
            db.execute(
                "INSERT INTO gf_heldout VALUES('h1',?1,?2,'Copied from chat: My cardiologist in Harrogate changed my beta blocker dose to 50 mg. Still right?','yes',?3)",
                rusqlite::params![a.workspace, a.repo, graphicle_runtime::now()],
            )?;
            Ok(())
        })
        .unwrap();
    let ids: Vec<&str> = X.iter().map(|(id, _)| *id).collect();
    let receipt = a.maintain(
        "source.forget",
        json!({"connector_id":chat,"document_ids":ids}),
    )["receipt"]
        .clone();
    assert_eq!(receipt["verified_local"], false, "{receipt:#}");
    assert_eq!(
        receipt["residue"]["verbatim_text"]["matches"],
        json!([{"table":"gf_heldout","rows":1}])
    );
    let checked = graphicle_runtime::deletion::verify(
        &a.store,
        &a.workspace,
        &a.key,
        &json!({"receipt_id":receipt["id"]}),
    )
    .unwrap();
    assert_eq!(
        checked["verified_local"], false,
        "known text residue must not be certified away: {checked:#}"
    );
    work(&a, &m, &[&chat]).await;
    work(&b, &m, &[&chat]).await;
    let (sa, sb) = (a.state(), b.state());
    assert_eq!(
        differing(&sa, &sb).keys().collect::<Vec<_>>(),
        ["gf_heldout"]
    );
}

/// State contract v2: two independently created stores holding the same
/// content digest equally, also when one of them has another edition's
/// tables, as long as those are empty; a row in one of them is content.
#[test]
fn independent_stores_with_equal_content_digest_equally() {
    let (a, b) = (Fixture::new(), Fixture::new());
    b.store
        .connect()
        .unwrap()
        .execute_batch("CREATE TABLE IF NOT EXISTS edition_rows (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE);")
        .unwrap();
    let (sa, sb) = (a.state(), b.state());
    assert_eq!(sa["contract"], "graphicle.state.v2");
    assert_eq!(sa["digest"], sb["digest"], "{:#?}", differing(&sa, &sb));
    assert!(
        sb["tables"].get("edition_rows").is_none(),
        "empty tables are omitted"
    );
    assert!(
        sa["tables"]
            .as_object()
            .unwrap()
            .values()
            .all(|table| table["rows"].as_u64().unwrap() > 0),
        "{sa:#}"
    );
    b.store
        .connect()
        .unwrap()
        .execute("INSERT INTO edition_rows VALUES('row',?)", [&b.workspace])
        .unwrap();
    assert_ne!(a.state()["digest"], b.state()["digest"]);
}
