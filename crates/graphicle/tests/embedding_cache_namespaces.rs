//! The opt-in passage-vector cache keeps a memory namespace's memory in its
//! own key space, per workspace: a namespace never learns from a cache hit
//! that another namespace, a namespace of the same name in another workspace
//! or store, or shared memory, embedded the same words, and it still reuses
//! its own entries. Source passages stay in the shared key space. Deleting
//! a namespace deletes its key space.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    api,
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<String>>>;

async fn embeddings(State(requests): State<Requests>, Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = serde_json::from_value(body["input"].clone()).unwrap();
    requests.lock().unwrap().extend(inputs.iter().cloned());
    Json(json!({
        "data":inputs.iter().enumerate().map(|(index, input)|json!({
            "index":index,"embedding":[input.len() as f64, 1.0]
        })).collect::<Vec<_>>(),
        "usage":{"prompt_tokens":inputs.len()}
    }))
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Project {
    _dir: tempfile::TempDir,
    store: Store,
    workspace: String,
    owner: String,
    repo: String,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Cache namespaces").unwrap();
        let (workspace, owner) = (s(&ws, "id").to_string(), s(&ws, "capability").to_string());
        let repo = s(
            &store
                .register(&workspace, &owner, &json!({"project_key":"c","name":"c"}))
                .unwrap(),
            "id",
        )
        .to_string();
        let text = "Deploys go through the blue pipeline.\n";
        let sha = graphicle_memory::text::sha256(text.as_bytes());
        let args = |extra: Value| {
            let mut value = extra;
            value["repository_id"] = json!(repo);
            value
        };
        let state = store
            .project_status(&workspace, &owner, &args(json!({})))
            .unwrap();
        let begin = store
            .sync_prepare(
                &workspace,
                &owner,
                &args(json!({"expected_snapshot":state["repository"]["snapshot"],
                "manifest":[{"path":"notes.txt","sha256":sha,"bytes":text.len()}]})),
            )
            .unwrap();
        store
            .sync_upload(&workspace, &owner, &args(json!({"sync_id":begin["sync_id"],"files":[{"path":"notes.txt","content":text}]})))
            .unwrap();
        store
            .sync_commit(
                &workspace,
                &owner,
                &args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            owner,
            repo,
        }
    }
    fn key(&self, role: &str, namespace: Option<&str>) -> String {
        let mut args = json!({"role":role,"label":role});
        if let Some(namespace) = namespace {
            args["memory_namespace"] = json!(namespace);
            args["self_review"] = json!(true);
        }
        s(
            &self
                .store
                .grant(&self.workspace, &self.owner, &args)
                .unwrap(),
            "capability",
        )
        .to_string()
    }
    fn command(&self, key: &str, operation: &str, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        let app = api::App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        api::command(&app, &self.workspace, key, operation, &args).unwrap()
    }
    /// The same reviewed memory, word for word, as `key`.
    fn remember(&self, key: &str, reviewer: &str, tag: &str) {
        let proposed = self.command(
            key,
            "memory.propose",
            json!({"kind":"decision","title":"Deploy rule","body":"Deploys use the blue pipeline.","idempotency_key":tag,
                "evidence":[{"source_id":"file:notes.txt","quote":"Deploys go through the blue pipeline."}]}),
        );
        self.command(
            reviewer,
            "learning.review",
            json!({"memory_id":proposed["memory_id"],"expected_revision":1,"decision":"approve","maintainer":"r","reason":"checked"}),
        );
    }
    async fn embed(&self, key: &str, config: &model::Config) -> Value {
        let job = self
            .store
            .enqueue(
                &self.workspace,
                key,
                &json!({"repository_id":self.repo,"kind":"embed","max_calls":2}),
                config,
            )
            .unwrap();
        assert!(
            Worker::new(self.store.clone(), config.clone())
                .unwrap()
                .once()
                .await
                .unwrap()
        );
        let status = self
            .store
            .job_status(
                &self.workspace,
                key,
                &json!({"repository_id":self.repo,"job_id":job["id"]}),
            )
            .unwrap();
        assert_eq!(status["state"], "complete", "{status:#}");
        status["result"].clone()
    }
}

#[tokio::test]
async fn each_namespace_has_its_own_cache_key_space() {
    let cache = tempfile::TempDir::new().unwrap();
    // This test binary holds a single test, so the variable cannot leak.
    unsafe {
        std::env::set_var(
            graphicle_runtime::embedding_cache::ENV,
            cache.path().join("vectors.sqlite"),
        );
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(requests.clone());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let config: model::Config = serde_json::from_value(json!({
        "default_model":"fixture","embedding_model":"fixture","models":{"fixture":{
            "protocol":"chat","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":1000
        }}
    }))
    .unwrap();
    let memory = "Deploy rule\nDeploys use the blue pipeline.";
    let embedded = |requests: &Requests| {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|t| *t == memory)
            .count()
    };

    let first = Project::new();
    let a = first.key("agent", Some("dev-a"));
    first.remember(&a, &a, "a");
    let result = first.embed(&a, &config).await;
    assert_eq!(
        (
            result["embedded_passages"].as_u64(),
            result["cached_passages"].as_u64()
        ),
        (Some(2), Some(0))
    );
    assert_eq!(embedded(&requests), 1);

    // B writes the same words: the model is asked again, no hit tells B.
    let b = first.key("agent", Some("dev-b"));
    first.remember(&b, &b, "b");
    let result = first.embed(&b, &config).await;
    assert_eq!(result["cached_passages"], 0, "{result:#}");
    assert_eq!(result["embedded_passages"], 1, "{result:#}");
    assert_eq!(embedded(&requests), 2);

    // Shared memory with the same words is in the shared space: no hit.
    let shared = first.key("agent", None);
    let reviewer = first.key("reviewer", None);
    first.remember(&shared, &reviewer, "s");
    let result = first.embed(&shared, &config).await;
    assert_eq!(result["cached_passages"], 0, "{result:#}");
    assert_eq!(embedded(&requests), 3);

    // Another store: the source passage comes from the shared space, and
    // its dev-a is another workspace's namespace, with no entry of its own.
    let second = Project::new();
    let again = second.key("agent", Some("dev-a"));
    second.remember(&again, &again, "a");
    let result = second.embed(&again, &config).await;
    assert_eq!(
        (
            result["embedded_passages"].as_u64(),
            result["cached_passages"].as_u64()
        ),
        (Some(1), Some(1)),
        "{result:#}"
    );
    assert_eq!(embedded(&requests), 4);
    let c = second.key("agent", Some("dev-c"));
    second.remember(&c, &c, "c");
    let result = second.embed(&c, &config).await;
    assert_eq!(
        (
            result["embedded_passages"].as_u64(),
            result["cached_passages"].as_u64()
        ),
        (Some(1), Some(0)),
        "{result:#}"
    );
    assert_eq!(embedded(&requests), 5);

    // Deleting the first store's dev-a deletes its one entry, and only it
    // (the other store's dev-a has one too): a new key there embeds again.
    let deleted = first
        .store
        .namespace_delete(&first.workspace, &first.owner, "dev-a")
        .unwrap();
    assert_eq!(
        deleted["receipt"]["namespace"]["embedding_cache"],
        json!({"entries_removed":1}),
        "{deleted:#}"
    );
    if !graphicle_runtime::db::postgres_configured() {
        let needle = format!(":memory_namespace:{}:dev-a", first.workspace);
        for suffix in ["", "-wal"] {
            let path = cache.path().join(format!("vectors.sqlite{suffix}"));
            if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{path:?} still holds deleted namespace key {needle}"
                );
            }
        }
    }
    let anew = first.key("agent", Some("dev-a"));
    first.remember(&anew, &anew, "a2");
    let result = first.embed(&anew, &config).await;
    assert_eq!(
        (
            result["embedded_passages"].as_u64(),
            result["cached_passages"].as_u64()
        ),
        (Some(1), Some(0)),
        "{result:#}"
    );
    assert_eq!(embedded(&requests), 6);
}
