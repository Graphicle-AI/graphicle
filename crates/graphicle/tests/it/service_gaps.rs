//! Service operations an SDK needs: document version history, source
//! filters for retrieval and answers, and prompt job pickup.
use axum::{Json, Router, routing::post};
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::time::Duration;
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
        let ws = store.create_workspace("Gaps").unwrap();
        let (workspace, key) = (s(&ws, "id").to_string(), s(&ws, "capability").to_string());
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"gaps","name":"Gaps"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
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
    fn connector(&self, name: &str) -> String {
        let added = self
            .store
            .connector_add(
                &self.workspace,
                &self.key,
                &self.args(json!({"kind":"jsonl","name":name,
                "config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
            )
            .unwrap();
        s(&added, "id").to_string()
    }
    fn import(&self, connector: &str, documents: Value) -> Value {
        self.store
            .import_documents(
                &self.workspace,
                &self.key,
                &self.args(json!({"connector_id":connector,"documents":documents})),
            )
            .unwrap()
    }
    fn context(&self, token: &str, args: Value) -> graphicle_runtime::Result<Value> {
        self.store
            .project_context(&self.workspace, token, &self.args(args))
    }
}

fn cited(packet: &Value) -> Vec<String> {
    let mut ids: Vec<String> = packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| s(e, "id").to_string())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[test]
fn document_versions_list_history_and_hide_older_content_from_non_owners() {
    let f = Fixture::new();
    let chat = f.connector("Chat");
    f.import(
        &chat,
        json!([{"id":"s1","title":"Session 1","content":"user: My tank holds 20 gallons."}]),
    );
    std::thread::sleep(Duration::from_millis(5));
    f.import(&chat, json!([{"id":"s1","title":"Session 1 (edited)","content":"user: My tank holds 30 gallons."}]));
    let owner = f
        .store
        .document_versions(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":chat,"document_id":"s1","include_content":true})),
        )
        .unwrap();
    let versions = owner["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2, "{owner}");
    assert_eq!(versions[0]["content"], "user: My tank holds 20 gallons.");
    assert_eq!(versions[1]["title"], "Session 1 (edited)");
    assert_eq!(owner["source_id"], format!("connector:{chat}:s1"));
    // A reader sees that versions exist, not what older versions said.
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"reader"}),
        )
        .unwrap();
    let listed = f
        .store
        .document_versions(
            &f.workspace,
            s(&reader, "capability"),
            &f.args(json!({"connector_id":chat,"document_id":"s1","include_content":true})),
        )
        .unwrap();
    assert_eq!(listed["content_included"], false);
    assert!(
        listed["versions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("content").is_none())
    );
    // Forgetting removes the history with the document.
    f.store
        .maintain(
            &f.workspace,
            &f.key,
            "source.forget",
            &f.args(json!({"connector_id":chat,"document_ids":["s1"]})),
        )
        .unwrap();
    let gone = f.store.document_versions(
        &f.workspace,
        &f.key,
        &f.args(json!({"connector_id":chat,"document_id":"s1"})),
    );
    assert!(gone.unwrap_err().message.contains("unavailable"));
}

#[test]
fn context_filters_search_only_the_included_sources_or_metadata() {
    let f = Fixture::new();
    let chat = f.connector("Chat");
    let mail = f.connector("Mail");
    f.import(&chat, json!([
        {"id":"a1","title":"Agent A","content":"user: The aquarium tank holds neon tetras.","metadata":{"agent_id":"a"}},
        {"id":"b1","title":"Agent B","content":"user: The aquarium tank holds guppies.","metadata":{"agent_id":"b"}}]));
    f.import(
        &mail,
        json!([{"id":"m1","title":"Mail","content":"The aquarium tank holds angelfish."}]),
    );
    let question = "What does the aquarium tank hold?";
    let all = f.context(&f.key, json!({"question":question})).unwrap();
    assert_eq!(cited(&all).len(), 3, "{all}");
    assert!(all.get("filter").is_none());
    let by_connector = f
        .context(
            &f.key,
            json!({"question":question,"include":[format!("connector:{mail}:")]}),
        )
        .unwrap();
    assert_eq!(cited(&by_connector), [format!("connector:{mail}:m1")]);
    assert_eq!(by_connector["filter"]["documents_excluded"], 2);
    let by_metadata = f
        .context(
            &f.key,
            json!({"question":question,"metadata":{"agent_id":"b"}}),
        )
        .unwrap();
    assert_eq!(cited(&by_metadata), [format!("connector:{chat}:b1")]);
    assert_ne!(
        by_metadata["source_state"], all["source_state"],
        "the snapshot key covers the filter"
    );
    let both = f
        .context(&f.key, json!({"question":question,"include":[format!("connector:{mail}:")],"metadata":{"agent_id":"b"}}))
        .unwrap();
    assert!(cited(&both).is_empty(), "{both}");
    for bad in [
        json!({"include":[]}),
        json!({"include":[""]}),
        json!({"metadata":"b"}),
        json!({"metadata":{}}),
    ] {
        let mut args = bad.clone();
        args["question"] = json!(question);
        assert!(f.context(&f.key, args).is_err(), "{bad}");
    }
}

async fn responses(Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let source = &input["sources"][0];
    let answer = json!({"answer":s(source, "text"),"abstain":false,"unresolved":[],
        "evidence":[{"id":source["id"],"quote":source["text"]}]});
    Json(
        json!({"status":"completed","model":"stub","usage":{"input_tokens":5,"output_tokens":5},
        "output":[{"type":"message","content":[{"type":"output_text","text":answer.to_string()}]}]}),
    )
}

#[tokio::test]
async fn basic_answers_read_only_filtered_sources_and_a_queued_job_wakes_the_worker() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(responses)),
        )
        .await
        .unwrap();
    });
    let config: model::Config = serde_json::from_value(json!({"default_model":"stub","models":{
        "stub":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"stub","input_limit":16000,"output_limit":2000}}}))
    .unwrap();
    let f = Fixture::new();
    let chat = f.connector("Chat");
    f.import(&chat, json!([
        {"id":"a1","title":"A","content":"user: The aquarium tank holds neon tetras.","metadata":{"agent_id":"a"}},
        {"id":"b1","title":"B","content":"user: The aquarium tank holds guppies.","metadata":{"agent_id":"b"}}]));
    // A waiter is woken by the enqueue itself, not by a poll.
    let wake = f.store.wake.clone();
    let woken = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), wake.notified())
            .await
            .is_ok()
    });
    tokio::task::yield_now().await;
    let queued = f
        .store
        .enqueue(&f.workspace, &f.key, &f.args(json!({"kind":"basic","question":"What does the aquarium tank hold?","metadata":{"agent_id":"b"},"max_calls":1})), &config)
        .unwrap();
    assert!(
        woken.await.unwrap(),
        "enqueue did not wake a waiting worker"
    );
    let worker = Worker::new(f.store.clone(), config.clone()).unwrap();
    assert!(worker.once().await.unwrap());
    let done = f
        .store
        .job_status(
            &f.workspace,
            &f.key,
            &f.args(json!({"job_id":queued["id"]})),
        )
        .unwrap();
    assert_eq!(done["state"], "complete", "{done}");
    let result = &done["result"];
    assert_eq!(
        cited(&result["context"]),
        [format!("connector:{chat}:b1")],
        "{result}"
    );
    assert_eq!(result["context"]["filter"]["documents_excluded"], 1);
    assert!(s(result, "answer").contains("guppies"));
    // An invalid filter is rejected when queued.
    let invalid = f.store.enqueue(
        &f.workspace,
        &f.key,
        &f.args(json!({"kind":"basic","question":"x","include":[""]})),
        &config,
    );
    assert!(invalid.unwrap_err().message.contains("include"));
    server.abort();
}
