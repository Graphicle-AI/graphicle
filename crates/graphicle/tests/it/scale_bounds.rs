//! Past the former hard bounds: a project with more than 5,000 documents and
//! one with more reviewed memories than recall ranks still list and answer.
use graphicle_runtime::store::{Store, s};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Scale").unwrap();
        let (workspace, key) = (s(&ws, "id").to_string(), s(&ws, "capability").to_string());
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"scale","name":"Scale"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
        Self {
            dir,
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
    fn context(&self, question: &str) -> Value {
        self.store
            .project_context(
                &self.workspace,
                &self.key,
                &self.args(json!({"question":question})),
            )
            .unwrap()
    }
}

#[test]
fn context_searches_more_than_five_thousand_documents() {
    let f = Fixture::new();
    let added = f
        .store
        .connector_add(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"jsonl","name":"Chat",
            "config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
        )
        .unwrap();
    let connector = s(&added, "id").to_string();
    let total = 5100;
    for start in (0..total).step_by(1700) {
        let documents: Vec<Value> = (start..(start + 1700).min(total))
            .map(|i| {
                let content = if i == 5077 {
                    "user: The aquarium tank now holds six neon tetras.".to_string()
                } else {
                    format!("user: Note {i} about the vegetable garden.")
                };
                json!({"id":format!("s{i:05}"),"title":format!("Session {i}"),"content":content})
            })
            .collect();
        f.store
            .import_documents(
                &f.workspace,
                &f.key,
                &f.args(json!({"connector_id":connector,"documents":documents})),
            )
            .unwrap();
    }
    let packet = f.context("How many neon tetras does the aquarium tank hold?");
    let coverage = &packet["coverage"];
    assert_eq!(coverage["source_documents"], total, "{coverage}");
    assert_eq!(coverage["passage_limit_reached"], false, "{coverage}");
    assert_eq!(coverage["omitted_documents"], 0, "{coverage}");
    assert_eq!(coverage["omitted_documents_are"], "none", "{coverage}");
    let needle = format!("connector:{connector}:s05077");
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == json!(needle)),
        "{}",
        packet["evidence"]
    );
}

#[test]
fn memories_past_the_recall_bound_list_and_recall_the_newest() {
    let f = Fixture::new();
    let extra = 50;
    let total = graphicle_memory::retrieval::MAX_RECALL_MEMORIES + extra;
    assert!(total > 10_000, "past the former listing cap");
    {
        // The row shape `memory::save` writes; revision history is not read here.
        let mut db = rusqlite::Connection::open(f.dir.path().join("reviews.sqlite3")).unwrap();
        let tx = db.transaction().unwrap();
        for i in 0..total {
            let (title, body) = match i {
                0 => (
                    "Old aquarium".to_string(),
                    "The aquarium tank holds nine guppies.".to_string(),
                ),
                7321 => (
                    "Aquarium".to_string(),
                    "The aquarium tank holds six neon tetras.".to_string(),
                ),
                _ => (
                    format!("Garden note {i}"),
                    format!("Bed {i} of the vegetable garden needs compost."),
                ),
            };
            // The first `extra` are the least recently updated.
            let updated = if i < extra {
                "2020-01-01T00:00:00.000000Z"
            } else {
                "2026-09-30T00:00:00.000000Z"
            };
            tx.execute(
                "INSERT INTO repo_memories(id,repository_id,kind,revision,state,title,body,data,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![format!("m{i:05}"), f.repo, "fact", 1, "reviewed", title, body, "{}", updated, updated],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["memories"], total, "{}", status["memories"]);
    assert_eq!(status["stale_memories"], 0);
    let packet = f.context("How many fish does the aquarium tank hold?");
    assert_eq!(
        packet["coverage"]["omitted_reviewed_memories"], extra,
        "{}",
        packet["coverage"]
    );
    let text = packet.to_string();
    assert!(
        text.contains("six neon tetras"),
        "recall missed the reviewed memory: {}",
        packet["memories"]
    );
    assert!(
        !text.contains("nine guppies"),
        "an omitted (oldest) memory was recalled"
    );
}
